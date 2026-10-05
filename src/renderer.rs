use std::collections::HashMap;
use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::fps::Fps;
use crate::line::{self, Line, Vertex, SHADER, VERTICES_PER_LINE};

const INITIAL_VERTEX_CAPACITY: usize = VERTICES_PER_LINE as usize;
const INITIAL_BLOCK_CAPACITY: usize = 6;
/// 音符顶点初始容量（每音符 2 个三角形 = 6 顶点）。
const INITIAL_NOTE_CAPACITY: usize = 6;

/// 文本/UI 图集尺寸（RGBA8，字体字形与 UI 图片共用）。
const TEXT_ATLAS_SIZE: u32 = 1024;
/// 字体资源键（内嵌 `assets/ui/Phigros.ttf`）。
const FONT_KEY: &str = "ui/Phigros.ttf";

const BLOCK_MASK_KEY: &str = "block/Block.png";
const BLOCK_DISPLACE_KEY: &str = "block/BlockNoise1.png";
const BLOCK_SPARK_KEY: &str = "block/PointNoise.png";

/// 块遮罩 RT 相对屏幕的降采样（文档为 8，但边界台阶过粗 → 提高精度）。
const MASK_DOWNSCALE: u32 = 2;
/// 边缘/辉光 RT 相对屏幕的降采样（文档为 4，同步提高）。
const EFFECT_DOWNSCALE: u32 = 2;
/// 块系统 RT 格式（`render.md`：块遮罩 fmt16、disabled/ready/composedDisabled/effect/ping fmt25）。
/// 这些 RT 需要容纳 >1 的值（subtract 归属 `v∈[0,2]`、覆盖度 `ds.y∈[0,20]`、辉光累积），
/// 以及块边缘/辉光的连续渐变，因此用 16F 而非 8bit UNORM（后者会截断并产生台阶）。
const BLOCK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// MSAA 采样数：块遮罩光栅化 与 场景（背景/线/Disabled）都用 4×，随后 resolve。
const MSAA_SAMPLES: u32 = 4;

pub const PHASE_NORMAL: f32 = 0.0;
pub const PHASE_SUBTRACT: f32 = 1.0;
pub const PHASE_DISABLED_NORMAL: f32 = 2.0;
pub const PHASE_DISABLED_SUBTRACT: f32 = 3.0;
pub const PHASE_READY_NORMAL: f32 = 4.0;
pub const PHASE_READY_SUBTRACT: f32 = 5.0;

/// `glowRadius = 6`、`glowWeightFalloff = 2.65` 时的环权重。
const GLOW_RADIUS: i32 = 6;
const GLOW_FALLOFF: f32 = 2.65;
const GLOW_THRESHOLD: f32 = 0.01;

#[derive(Clone, Copy, Debug, Default)]
pub enum Fit {
    #[default]
    Stretch,
    Contain,
}

/// 2D 仿射矩阵（canvas 的 6 参数形式）：
/// `x' = a·x + c·y + e`，`y' = b·x + d·y + f`。
///
/// 作为 [`Renderer`] 的当前变换（CTM），语义与 HTML Canvas 2D 的
/// `ctx.translate` / `rotate` / `scale` 一致：变换按调用顺序**后乘**，
/// 且作用于之后所有 `draw_line` / `draw_block` 的用户坐标。
#[derive(Clone, Copy)]
struct Affine2 {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Affine2 {
    /// 恒等变换。
    const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// `self * other`：先应用 `other`，再应用 `self`（canvas 后乘语义）。
    fn mul(self, o: Self) -> Self {
        Self {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    fn translate(x: f32, y: f32) -> Self {
        Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: x, f: y }
    }

    /// 旋转，角度单位为度，逆时针为正（与 `draw_line` 的角度一致）。
    fn rotate(deg: f32) -> Self {
        let (sin, cos) = deg.to_radians().sin_cos();
        Self { a: cos, b: sin, c: -sin, d: cos, e: 0.0, f: 0.0 }
    }

    fn scale(sx: f32, sy: f32) -> Self {
        Self { a: sx, b: 0.0, c: 0.0, d: sy, e: 0.0, f: 0.0 }
    }

    /// 变换一个点（含平移分量）。
    fn apply(self, p: [f32; 2]) -> [f32; 2] {
        [self.a * p[0] + self.c * p[1] + self.e, self.b * p[0] + self.d * p[1] + self.f]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlockVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    phase: f32,
}

impl BlockVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32];
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress, step_mode: wgpu::VertexStepMode::Vertex, attributes: &Self::ATTRIBS }
    }
}

/// 一条待绘制线段 + 绘制时所处的坐标系变换（canvas 语义：变换在 `draw_line` 时捕获）。
#[derive(Clone, Copy)]
struct LineDraw {
    line: Line,
    transform: Affine2,
}

/// 音符顶点：NDC 位置 + UV + 颜色。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct NoteVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

impl NoteVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress, step_mode: wgpu::VertexStepMode::Vertex, attributes: &Self::ATTRIBS }
    }
}

/// 音符贴图种类（含高亮 HL 版本）。索引与 [`NOTE_TEXTURE_PATHS`] 一一对应。
#[derive(Clone, Copy)]
pub enum NoteTexture {
    /// Tap（普通）。
    Tap,
    /// Tap（高亮）。
    TapHl,
    /// Drag（普通）。
    Drag,
    /// Drag（高亮）。
    DragHl,
    /// Flick（普通）。
    Flick,
    /// Flick（高亮）。
    FlickHl,
    /// Hold 主体（普通）。
    HoldBody,
    /// Hold 主体（高亮）。
    HoldBodyHl,
    /// Hold 头（普通）。
    HoldHead,
    /// Hold 头（高亮）。
    HoldHeadHl,
    /// Hold 尾。
    HoldEnd,
}

impl NoteTexture {
    fn index(self) -> usize {
        match self {
            NoteTexture::Tap => 0,
            NoteTexture::TapHl => 1,
            NoteTexture::Drag => 2,
            NoteTexture::DragHl => 3,
            NoteTexture::Flick => 4,
            NoteTexture::FlickHl => 5,
            NoteTexture::HoldBody => 6,
            NoteTexture::HoldBodyHl => 7,
            NoteTexture::HoldHead => 8,
            NoteTexture::HoldHeadHl => 9,
            NoteTexture::HoldEnd => 10,
        }
    }
}

/// 音符贴图数量（索引与 [`NoteTexture`] 对应）。
const NOTE_TEXTURE_COUNT: usize = 11;

/// 音符贴图文件（`HL` 为高亮版本）；键为相对 `assets/` 的路径。
const NOTE_TEXTURE_PATHS: [&str; NOTE_TEXTURE_COUNT] = [
    "notes/Tap2.png",
    "notes/Tap2HL.png",
    "notes/Drag2.png",
    "notes/DragHL.png",
    "notes/Flick2.png",
    "notes/Flick2HL.png",
    "notes/Hold.png",
    "notes/HoldHL.png",
    "notes/HoldHead.png",
    "notes/HoldHeadHL.png",
    "notes/HoldEnd.png",
];

/// 一个待绘制音符：`center` 为其所在判定线**局部坐标系**中的中心，`angle` 为局部角度（弧度），
/// 宽高为设计像素；`texture` 为 [`NoteTexture`] 索引。
#[derive(Clone, Copy)]
struct NoteSprite {
    center: [f32; 2],
    width: f32,
    height: f32,
    angle: f32,
    texture: usize,
    color: [f32; 4],
    /// 横向 UV 范围（裁剪贴图左右透明留白）。
    uv_x: [f32; 2],
    transform: Affine2,
    /// 绘制层：`0` = Hold（位于所有note最下面）、`1` = 普通note、`2` = 打击特效。
    layer: u8,
}

/// 音符贴图着色器：直接采样贴图并乘以顶点色（顶点色含全局透明度）。
const NOTE_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(position, 0.0, 1.0);
    o.uv = uv;
    o.color = color;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlockUniform {
    fill: [f32; 4],        // _FillColor.rgb, _FillOpacity
    edge: [f32; 4],        // _EdgeColor.rgb, _EdgeOpacity
    glow: [f32; 4],        // _GlowColor.rgb, _GlowIntensity
    params: [f32; 4],      // time, _FillStrength, _DisplaceBlendIntensity, _BackgroundPixelScale
    displace: [f32; 4],    // _DisplaceSpeed, _DisplaceStrength, dir.xy
    texel: [f32; 4],       // 1/ew,1/eh,ew,eh
    dis_fill: [f32; 4],    // disabled _FillColor.rgb, _FillOpacity
    spark: [f32; 4],       // _SparkTint.rgb, _SparkMapOpacity
    spark2: [f32; 4],      // _SparkDisplaceIntensity, _SparkHueShiftAmount
    st_spark: [f32; 4],    // _SparkMap_ST
    displace_compose: [f32; 4],
    st_compose: [f32; 4],
    st_active: [f32; 4],
    screen: [f32; 4],
    blend: [f32; 4],       // _ClampThresholdLow, _ClampThresholdHigh, 0, 0
    dis_spark: [f32; 4],   // disabled _SparkTint.rgb, _SparkMapOpacity
    dis_params: [f32; 4],  // disabled _DisplaceSpeed, _SparkDisplaceIntensity, 0, 0
    st_disabled: [f32; 4], // disabled _DisplaceMap_ST.xy, 0, 0
}

/// 所有块着色器共用的 uniform 结构声明。
const U_STRUCT: &str = r#"
struct Uniforms {
    fill: vec4<f32>, edge: vec4<f32>, glow: vec4<f32>, params: vec4<f32>,
    displace: vec4<f32>, texel: vec4<f32>, dis_fill: vec4<f32>, spark: vec4<f32>,
    spark2: vec4<f32>, st_spark: vec4<f32>, displace_compose: vec4<f32>,
    st_compose: vec4<f32>, st_active: vec4<f32>, screen: vec4<f32>,
    blend: vec4<f32>, dis_spark: vec4<f32>, dis_params: vec4<f32>, st_disabled: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;
"#;

const FULLSCREEN_VS: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    var pts = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    let xy = pts[index];
    var o: VsOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}
"#;

/// 纯拷贝 / 下采样（对应 Unity `Graphics.Blit(CameraTarget, sceneColorRT)`）。
const COPY_SHADER: &str = r#"
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    var pts = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    let xy = pts[index];
    var o: VsOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, samp, in.uv);
}
"#;

fn sprite_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
struct VsOut {{ @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec4<f32> }};
@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>, @location(3) phase: f32) -> VsOut {{
    var o: VsOut; o.pos = vec4<f32>(position, 0.0, 1.0); o.uv = uv; o.color = color; return o;
}}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    let cov = textureSample(mask_tex, samp, in.uv).r;
    return vec4<f32>(cov * in.color.rgb, in.color.a);
}}"
    )
}

fn compose1_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var normal_tex: texture_2d<f32>;
@group(0) @binding(2) var subtract_tex: texture_2d<f32>;
@group(0) @binding(3) var disp_tex: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;
@group(0) @binding(5) var disp_samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    let dir = normalize(u.displace_compose.zw);
    let t = u.params.x * u.displace_compose.x * 0.05;
    let rot = vec2<f32>(-dir.y * t, dir.x * t);
    let st = in.uv * u.st_compose.xy;
    let d1 = textureSample(disp_tex, disp_samp, dir * t + st).r - 0.5;
    let d2 = textureSample(disp_tex, disp_samp, rot + st).r - 0.5;
    let disp = dir * d1 + vec2<f32>(-dir.y, dir.x) * d2;
    let uv = in.uv + disp * u.displace_compose.y;
    let n = textureSample(normal_tex, samp, uv).x;
    let s = textureSample(subtract_tex, samp, uv).x;
    return vec4<f32>(abs(s - n), 0.0, 0.0, 1.0);
}}"
    )
}

fn compose2_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var dn_tex: texture_2d<f32>;
@group(0) @binding(2) var ds_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    let dn = textureSample(dn_tex, samp, in.uv).x;
    let ds = textureSample(ds_tex, samp, in.uv).xy;
    let t = ds.x * ds.y - dn;
    return vec4<f32>(abs(t), ds.y, 0.0, 1.0);
}}"
    )
}

fn edge_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var main_tex: texture_2d<f32>;
@group(0) @binding(2) var compose_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
{FULLSCREEN_VS}
fn max9(uv: vec2<f32>) -> f32 {{
    let t = u.texel.xy;
    var m = textureSample(main_tex, samp, uv).x;
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>( t.x, 0.0)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>(-t.x, 0.0)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>(0.0,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>(0.0, -t.y)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>( t.x,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>( t.x, -t.y)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>(-t.x,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, uv + vec2<f32>(-t.x, -t.y)).x);
    return m;
}}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    let m = max9(in.uv) - textureSample(compose_tex, samp, in.uv).x;
    let e = clamp(m, 0.0, 1.0);
    return vec4<f32>(e, 0.0, 0.0, 0.0);
}}"
    )
}

fn glow_shader() -> String {
    format!(
        "{U_STRUCT}
struct GlowConfig {{ data: vec4<f32> }};
@group(0) @binding(1) var<uniform> config: GlowConfig;
@group(0) @binding(2) var main_tex: texture_2d<f32>;
@group(0) @binding(3) var compose_tex: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    let t = u.texel.xy;
    let c = textureSample(main_tex, samp, in.uv);
    var m = c.x;
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>( t.x, 0.0)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>(-t.x, 0.0)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>(0.0,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>(0.0, -t.y)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>( t.x,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>( t.x, -t.y)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>(-t.x,  t.y)).x);
    m = max(m, textureSample(main_tex, samp, in.uv + vec2<f32>(-t.x, -t.y)).x);
    // Unlit/GlowMask：ring = max×(1−_ComposeRT)；末轮只写 .y
    var ring = clamp(m - c.x, 0.0, 1.0);
    ring = (1.0 - textureSample(compose_tex, samp, in.uv).x) * ring;
    let first = config.data.y > 0.5;
    var y = select(config.data.x * ring + c.y, config.data.x * ring, first);
    y = clamp(y, 0.0, 1.0);
    return vec4<f32>(m, y, 0.0, 0.0);
}}"
    )
}

fn glow_final_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var main_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    // 只写 .y（ColorWrites::GREEN），保留 effectRT 的 .x（边缘）
    let glow = textureSample(main_tex, samp, in.uv).y;
    return vec4<f32>(0.0, glow, 0.0, 0.0);
}}"
    )
}

fn disabled_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var compose_tex: texture_2d<f32>;
@group(0) @binding(2) var disp_tex: texture_2d<f32>;
@group(0) @binding(3) var spark_tex: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;
@group(0) @binding(5) var disp_samp: sampler;
@group(0) @binding(6) var spark_samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    // Unlit/DisabledBlock：填充 + 火花（无像素化；ST 与 active 不同）
    let comp = textureSample(compose_tex, samp, in.uv).x;
    if (comp - 1e-4 < 0.0) {{ discard; }}
    let dir = normalize(u.displace.zw);
    let t = u.params.x * u.dis_params.x * 0.05;
    let st = in.uv * u.st_disabled.xy;
    let a1 = textureSample(disp_tex, disp_samp, dir * t + st).x;
    let a2 = textureSample(disp_tex, disp_samp, vec2<f32>(-dir.y * t, dir.x * t) + st).x;
    let avg = (a1 + a2) * 0.5;
    let disp = dir * (a1 - 0.5) + vec2<f32>(-dir.y, dir.x) * (a2 - 0.5);
    let spark = textureSample(spark_tex, spark_samp, disp * u.dis_params.y + in.uv * u.st_spark.xy).x;
    let col = spark * u.dis_spark.rgb * avg * u.dis_spark.w + u.dis_fill.rgb * u.dis_fill.w;
    return vec4<f32>(comp * col, 1.0);
}}"
    )
}

/// `Unlit/SubtractBlockBlender` 作为**减块 sprite 材质**（逐 quad 光栅化；`_MainTex` = `Block.png`）。
/// 输出 `(归属 v, 覆盖度 cov*v*10)`；块外不写，避免全屏背景被 `v=1` 污染。
fn subtract_blender_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
struct VsOut {{ @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }};
@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>, @location(3) phase: f32) -> VsOut {{
    var o: VsOut; o.pos = vec4<f32>(position, 0.0, 1.0); o.uv = uv; return o;
}}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    // Unlit/SubtractBlockBlender：双阈值阶跃 × smoothstep
    let t = textureSample(mask_tex, samp, in.uv).xy;
    let lo = select(0.0, 1.0, t.x >= u.blend.x);
    let hi = select(0.0, -1.0, t.x >= u.blend.y);
    let st = lo + hi;
    let k = clamp((t.y - 0.2) * -10.0, 0.0, 1.0);
    let v = (k * -2.0 + 3.0) * (k * k) + st;
    return vec4<f32>(v, t.y * v * 10.0, 0.0, 1.0);
}}"
    )
}

fn active_shader() -> String {
    format!(
        "{U_STRUCT}
@group(0) @binding(1) var compose_tex: texture_2d<f32>;
@group(0) @binding(2) var effect_tex: texture_2d<f32>;
@group(0) @binding(3) var ready_n_tex: texture_2d<f32>;
@group(0) @binding(4) var ready_s_tex: texture_2d<f32>;
@group(0) @binding(5) var ready_compose_tex: texture_2d<f32>;
@group(0) @binding(6) var disp_tex: texture_2d<f32>;
@group(0) @binding(7) var spark_tex: texture_2d<f32>;
@group(0) @binding(8) var samp: sampler;
@group(0) @binding(9) var disp_samp: sampler;
@group(0) @binding(10) var spark_samp: sampler;
@group(0) @binding(11) var scene_tex: texture_2d<f32>;
@group(0) @binding(12) var effect_samp: sampler;
{FULLSCREEN_VS}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    // 入口水平 discard
    let t6 = u.screen.y * 0.8888889 / u.screen.x;
    if (-abs(in.uv.x - 0.5) + t6 < 0.0) {{ discard; }}

    let comp = textureSample(compose_tex, samp, in.uv).x;
    let ep = (floor(in.uv * u.texel.zw) + 0.5) * u.texel.xy;
    let edge = textureSample(effect_tex, effect_samp, ep).x;
    let glow = textureSample(effect_tex, effect_samp, in.uv).y;
    let dn = textureSample(ready_n_tex, samp, in.uv).x;
    let ds = textureSample(ready_s_tex, samp, in.uv).y;
    let ready = textureSample(ready_compose_tex, samp, in.uv).x;
    let m = ds - dn;
    let rd = ready * abs(m);
    let sum_enabled = edge + comp;
    let total50 = glow + sum_enabled;
    let total = abs(m) * ready + total50;
    if (total - 1e-4 < 0.0) {{ discard; }}

    var rgb = vec3<f32>(0.0);
    var alpha = 0.0;
    if (1e-4 < total50) {{
        let edge_term = edge * u.edge.w;
        let glow_adj = sum_enabled * (-glow) + glow;
        let glow_term = glow_adj * u.glow.w;
        let dir = normalize(u.displace.zw);
        let t = u.params.x * u.displace.x * 0.05;
        let pix = max(u.params.w, 1.0);
        let pixv = vec2<f32>(pix, pix);
        let s1 = (floor(((dir * t + in.uv * u.st_active.xy) * u.screen.xy) / pixv) * pixv + pixv * 0.5) / u.screen.xy;
        let rot = vec2<f32>(-dir.y * t, dir.x * t);
        let s2 = (floor(((rot + in.uv * u.st_active.xy) * u.screen.xy) / pixv) * pixv + pixv * 0.5) / u.screen.xy;
        let a1 = textureSample(disp_tex, disp_samp, s1).x;
        let a2 = textureSample(disp_tex, disp_samp, s2).x;
        let avg = (a1 + a2) * 0.5;
        let d1 = a1 - 0.5;
        let d2 = a2 - 0.5;
        let disp = dir * d1 + vec2<f32>(-dir.y, dir.x) * d2;
        let spark = textureSample(spark_tex, spark_samp, disp * u.spark2.x + in.uv * u.st_spark.xy).x * u.spark.rgb;
        let spark_term = avg * spark * u.spark.w;
        // _SceneColor = sceneColorRT（C# `activeBlockMaterial._SceneColor`）。原版在
        // 像素化基 UV + disp*_DisplaceStrength 处采样（u_xlat16.xy）。
        let base = (floor((in.uv * u.screen.xy) / pixv) * pixv + pixv * 0.5) / u.screen.xy + disp * u.displace.y;
        let scene = textureSample(scene_tex, samp, base).rgb;
        let k = vec4<f32>(0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0);
        let p = select(vec4<f32>(scene.b, scene.g, k.w, k.z), vec4<f32>(scene.g, scene.b, k.x, k.y), scene.g >= scene.b);
        let q = select(vec4<f32>(p.x, p.y, p.w, scene.r), vec4<f32>(scene.r, p.y, p.z, p.x), scene.r >= p.x);
        let d = q.x - min(q.w, q.y);
        let e = 1.0e-10;
        let h = abs(q.z + (q.w - q.y) / (6.0 * d + e));
        let s = d / (q.x + e);
        let v = q.x;
        let hh = spark_term.x * u.spark2.y + h;
        let ss = spark_term.y * u.spark2.y + s;
        let vv = spark_term.z * u.spark2.y + v;
        var rgbh = abs((fract(vec3<f32>(hh, hh, hh) + vec3<f32>(1.0, 2.0 / 3.0, 1.0 / 3.0))) * 6.0 - 3.0) - 1.0;
        rgbh = clamp(rgbh, vec3<f32>(0.0), vec3<f32>(1.0));
        let hsv_rgb = (mix(vec3<f32>(1.0), rgbh, ss)) * vv;
        let lo = hsv_rgb * spark_term * 2.0;
        let hi = 1.0 - (1.0 - hsv_rgb) * 2.0 * (1.0 - spark_term);
        let shifted = clamp(select(lo, hi, hsv_rgb >= vec3<f32>(0.5)), vec3<f32>(0.0), vec3<f32>(1.0));
        let fill_base = u.fill.rgb - avg * u.params.z;
        let fill_mix = mix(fill_base, shifted, u.params.y);
        rgb = fill_mix * comp + u.edge.rgb * edge_term + u.glow.rgb * glow_term;
        alpha = glow_term + comp * u.fill.w + edge_term;
    }}

    // 预备态呼吸：_ShineColor(白) × _ShineBrightness × (sin(_Time.y×_ShineSpeed)×0.5+1)，覆盖度乘两次
    var ready_rgb = vec3<f32>(0.0);
    if (rd > 1e-4) {{
        let pulse = sin(u.params.x * u.spark2.z) * 0.5 + 1.0;
        ready_rgb = vec3<f32>(1.0) * u.spark2.w * pulse * rd * rd;
    }}
    return vec4<f32>(rgb + ready_rgb, alpha);
}}"
    )
}

// ---- 主结构 ----

/// 一个已光栅化的字形（存于文本图集）。
#[derive(Clone, Copy)]
struct Glyph {
    /// 图集 UV：`[u0, v0, u1, v1]`。
    uv: [f32; 4],
    /// 位图尺寸（像素）。
    size: [f32; 2],
    /// 相对笔位置的左偏移（`px_bounds.min.x`）。
    left: f32,
    /// 基线上方到字形顶部的距离（像素，正值）。
    top: f32,
    /// 水平前进量。
    advance: f32,
}

/// 打包进文本图集的一张 UI 图片。
#[derive(Clone, Copy)]
struct UiImage {
    uv: [f32; 4],
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,

    sprite_pipeline: wgpu::RenderPipeline,
    compose1_pipeline: wgpu::RenderPipeline,
    compose2_pipeline: wgpu::RenderPipeline,
    edge_pipeline: wgpu::RenderPipeline,
    glow_pipeline: wgpu::RenderPipeline,
    glow_final_pipeline: wgpu::RenderPipeline,
    subtract_pipeline: wgpu::RenderPipeline,
    disabled_pipeline: wgpu::RenderPipeline,
    active_pipeline: wgpu::RenderPipeline,
    copy_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,

    sprite_bg: wgpu::BindGroup,
    compose1_bg: wgpu::BindGroup,
    compose2_bg: wgpu::BindGroup,
    edge_bg: wgpu::BindGroup,
    glow_bgs: Vec<wgpu::BindGroup>,
    glow_final_bg: wgpu::BindGroup,
    disabled_bg: wgpu::BindGroup,
    active_bg: wgpu::BindGroup,
    copy_bg: wgpu::BindGroup,
    bg_bg: wgpu::BindGroup,
    overlay_bg_a: wgpu::BindGroup,
    overlay_bg_b: wgpu::BindGroup,

    block_vertex_buffer: wgpu::Buffer,
    block_capacity: usize,
    block_uniform: wgpu::Buffer,
    glow_configs: Vec<wgpu::Buffer>,
    rt_sampler: wgpu::Sampler,
    disp_sampler: wgpu::Sampler,
    spark_sampler: wgpu::Sampler,
    effect_sampler: wgpu::Sampler,
    disp_view: wgpu::TextureView,
    spark_view: wgpu::TextureView,

    rt_normal: (wgpu::Texture, wgpu::TextureView),
    rt_subtract: (wgpu::Texture, wgpu::TextureView),
    rt_disabled_normal: (wgpu::Texture, wgpu::TextureView),
    rt_disabled_subtract: (wgpu::Texture, wgpu::TextureView),
    rt_ready_normal: (wgpu::Texture, wgpu::TextureView),
    rt_ready_subtract: (wgpu::Texture, wgpu::TextureView),
    rt_composed_enabled: (wgpu::Texture, wgpu::TextureView),
    rt_composed_disabled: (wgpu::Texture, wgpu::TextureView),
    rt_effect: (wgpu::Texture, wgpu::TextureView),
    ping_a: (wgpu::Texture, wgpu::TextureView),
    ping_b: (wgpu::Texture, wgpu::TextureView),
    rt_scene_full: (wgpu::Texture, wgpu::TextureView),
    rt_scene: (wgpu::Texture, wgpu::TextureView),
    mask_msaa: Vec<(wgpu::Texture, wgpu::TextureView)>,
    scene_full_msaa: (wgpu::Texture, wgpu::TextureView),

    block_time: f32,
    size: [f32; 2],
    /// 实际渲染尺寸（导出时为视频尺寸；窗口播放时等于 `size`）。RT 与坐标映射基于它。
    render_size: [f32; 2],
    /// 游戏画面缩放比例（`--size`，1.0 = 铺满渲染尺寸）。
    render_scale: f32,
    design: [f32; 2],
    fit: Fit,
    origin: [f32; 2],
    y_up: bool,
    /// 当前用户空间变换（canvas 风格），由 `translate` / `rotate` / `scale` 修改。
    ctm: Affine2,
    /// `save()` 压入的变换栈。
    #[allow(dead_code)]
    transform_stack: Vec<Affine2>,
    fps: Fps,
    vsync: bool,
    lines: Vec<LineDraw>,
    vertices: Vec<Vertex>,
    block_vertices: Vec<BlockVertex>,

    note_pipeline: wgpu::RenderPipeline,
    note_bind_groups: Vec<wgpu::BindGroup>,
    note_tex_size: [[f32; 2]; NOTE_TEXTURE_COUNT],
    note_tex_alpha: [f32; NOTE_TEXTURE_COUNT],
    note_tex_blue: [f32; NOTE_TEXTURE_COUNT],
    note_buffer: wgpu::Buffer,
    note_capacity: usize,
    note_vertices: Vec<NoteVertex>,
    note_sprites: Vec<NoteSprite>,
    /// 打击特效帧数量（`assets/hit/img-*.png`）。
    hit_frame_count: usize,

    /// 字体（内嵌 `assets/ui/Phigros.ttf`）。
    font: FontRef<'static>,
    /// 字形缓存：键 `(字符码点, 像素高度)`。
    glyphs: HashMap<(u32, u32), Glyph>,
    /// 打包进文本图集的 UI 图片（键为资源键）。
    ui_images: HashMap<String, UiImage>,
    /// 文本图集 CPU 数据（RGBA8）与打包游标。
    atlas_data: Vec<u8>,
    atlas_x: u32,
    atlas_y: u32,
    atlas_row_h: u32,
    atlas_dirty: bool,
    /// 文本图集纹理（首帧及其后有新字形时整张上传）。
    text_atlas_tex: wgpu::Texture,
    text_bg: wgpu::BindGroup,
    text_pipeline: wgpu::RenderPipeline,
    text_buffer: wgpu::Buffer,
    text_capacity: usize,
    text_vertices: Vec<NoteVertex>,

    /// 视频导出的离屏渲染目标（`RENDER_ATTACHMENT | COPY_SRC`）。
    export_rt: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// 视频导出的回读缓冲（`COPY_DST | MAP_READ`）。
    export_buffer: Option<wgpu::Buffer>,
    /// 回读缓冲每行对齐后的字节数（`COPY_BYTES_PER_ROW_ALIGNMENT` 对齐）。
    export_padded_bytes_per_row: u32,
    /// 导出尺寸 `[w, h]`。
    export_size: [u32; 2],
}

impl Renderer {
    pub fn new(window: Arc<Window>, bg_path: Option<String>) -> Self {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(&surface), ..Default::default() })).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("device"), ..Default::default() })).unwrap();

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let mut config = surface.get_default_config(&adapter, size.width, size.height).unwrap();
        // Phigros 为 Gamma 色彩空间：材质色（`_FillColor 0.713/0.235` 等）本身就是显示值。
        // 用非 sRGB 目标，避免线性→sRGB 编码把红色洗浅/发白。
        if let Some(f) = caps.formats.iter().copied().find(|f| !f.is_srgb()) {
            config.format = f;
        }
        surface.configure(&device, &config);

        let pipeline = build_line_pipeline(&device, config.format, MSAA_SAMPLES);
        let vertex_buffer = create_vertex_buffer(&device, INITIAL_VERTEX_CAPACITY);
        let block_vertex_buffer = create_block_vertex_buffer(&device, INITIAL_BLOCK_CAPACITY);

        let mask_tex = load_texture(&device, &queue, BLOCK_MASK_KEY, false);
        let disp_tex = load_texture(&device, &queue, BLOCK_DISPLACE_KEY, false);
        let spark_tex = load_texture(&device, &queue, BLOCK_SPARK_KEY, false);
        // 背景图（`IllustrationBlur.0.png`）：随谱面一起提供，运行时从磁盘读取。
        let bg_view = match &bg_path {
            Some(p) if std::path::Path::new(p).exists() => load_texture_file(&device, &queue, p, false).view,
            _ => solid_texture(&device, &queue, [0, 0, 0, 255]),
        };
        let bg_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("bg sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge, address_mode_v: wgpu::AddressMode::ClampToEdge, address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        // 贴图导入设置（`materials.md`）：Block=Clamp/sRGB、BlockNoise1=Mirror/sRGB、PointNoise=Repeat/Linear
        let mask_sampler = make_sampler(&device, "mask sampler", wgpu::AddressMode::ClampToEdge);
        let rt_sampler = make_sampler(&device, "rt sampler", wgpu::AddressMode::Repeat);
        let disp_sampler = make_sampler(&device, "disp sampler", wgpu::AddressMode::MirrorRepeat);
        let spark_sampler = make_sampler(&device, "spark sampler", wgpu::AddressMode::Repeat);
        // effectRT 是 13 张 RT 中唯一的 Bilinear（其余含所有块遮罩均为 Point）
        let effect_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("effect sampler"),
            address_mode_u: wgpu::AddressMode::Repeat, address_mode_v: wgpu::AddressMode::Repeat, address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let block_uniform = device.create_buffer(&wgpu::BufferDescriptor { label: Some("block uniform"), size: std::mem::size_of::<BlockUniform>() as wgpu::BufferAddress, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });

        // 音符贴图（普通 + HL 高亮）、采样器、绑定组与管线。
        let note_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("note sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge, address_mode_v: wgpu::AddressMode::ClampToEdge, address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let note_bgl = create_note_bgl(&device);
        let mut note_tex_size = [[1.0f32, 1.0f32]; NOTE_TEXTURE_COUNT];
        let mut note_tex_alpha = [1.0f32; NOTE_TEXTURE_COUNT];
        let mut note_tex_blue = [1.0f32; NOTE_TEXTURE_COUNT];
        let mut note_bind_groups = Vec::with_capacity(NOTE_TEXTURE_COUNT);
        for (i, path) in NOTE_TEXTURE_PATHS.iter().enumerate() {
            let t = load_texture(&device, &queue, path, false);
            note_tex_size[i] = [t.size[0] as f32, t.size[1] as f32];
            note_tex_alpha[i] = t.alpha_width;
            note_tex_blue[i] = t.blue_width;
            note_bind_groups.push(bg_note(&device, &note_bgl, &t.view, &note_sampler));
        }
        // 打击特效帧（内嵌 `hit/img-N.png`），索引紧跟在音符贴图之后；统一染成金色。
        let mut hit_frame_count = 0usize;
        for i in 1..=64 {
            let key = format!("hit/img-{i}.png");
            if crate::embedded::get(&key).is_none() { break; }
            let t = load_texture_tinted(&device, &queue, &key, false, Some([255, 236, 160]));
            note_bind_groups.push(bg_note(&device, &note_bgl, &t.view, &note_sampler));
            hit_frame_count += 1;
        }
        // 打击特效的小方块：纯白贴图，绘制时用顶点色染成金色。
        let spark_view = solid_texture(&device, &queue, [255, 255, 255, 255]);
        note_bind_groups.push(bg_note(&device, &note_bgl, &spark_view, &note_sampler));
        let note_pipeline = build_note_pipeline(&device, config.format, MSAA_SAMPLES, &note_bgl);
        let note_buffer = create_note_vertex_buffer(&device, INITIAL_NOTE_CAPACITY);

        // ---- 文本/UI 图集（字形与 UI 图片共用，绘制在最终合成之上）----
        let font = FontRef::try_from_slice(crate::embedded::expect(FONT_KEY)).expect("解析字体失败");
        let mut atlas_data = vec![0u8; (TEXT_ATLAS_SIZE as usize) * (TEXT_ATLAS_SIZE as usize) * 4];
        let mut atlas_cursor = (0u32, 0u32, 0u32);
        let mut ui_images: HashMap<String, UiImage> = HashMap::new();
        for key in ["ui/pause.png", "ui/timerLine.png"] {
            let img = image::load_from_memory(crate::embedded::expect(key)).expect("解码 UI 图片失败").to_rgba8();
            let (iw, ih) = (img.width(), img.height());
            let (ax, ay) = atlas_pack(&mut atlas_cursor, iw + 2, ih + 2);
            atlas_blit(&mut atlas_data, ax + 1, ay + 1, &img);
            ui_images.insert(
                key.to_string(),
                UiImage {
                    uv: [
                        (ax + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ay + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ax + 1 + iw) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ay + 1 + ih) as f32 / TEXT_ATLAS_SIZE as f32,
                    ],
                },
            );
        }
        // 16×16 纯白，供绘制纯色矩形（进度条等）；足够大以免线性过滤把边缘滤空。
        {
            let (ax, ay) = atlas_pack(&mut atlas_cursor, 18, 18);
            for yy in 0..16u32 {
                for xx in 0..16u32 {
                    let idx = (((ay + 1 + yy) * TEXT_ATLAS_SIZE + (ax + 1 + xx)) * 4) as usize;
                    atlas_data[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            ui_images.insert(
                "ui/white".to_string(),
                UiImage {
                    uv: [
                        (ax + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ay + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ax + 17) as f32 / TEXT_ATLAS_SIZE as f32,
                        (ay + 17) as f32 / TEXT_ATLAS_SIZE as f32,
                    ],
                },
            );
        }
        let text_atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("text atlas"),
            size: wgpu::Extent3d { width: TEXT_ATLAS_SIZE, height: TEXT_ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm, usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[],
        });
        let text_atlas_view = text_atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let text_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("text sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge, address_mode_v: wgpu::AddressMode::ClampToEdge, address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest, min_filter: wgpu::FilterMode::Nearest, mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let text_bg = bg_note(&device, &note_bgl, &text_atlas_view, &text_sampler);
        let text_pipeline = build_note_pipeline(&device, config.format, 1, &note_bgl);
        let text_buffer = create_note_vertex_buffer(&device, INITIAL_NOTE_CAPACITY);

        let all = wgpu::ColorWrites::ALL;
        // Unlit/BlockSprite 固定状态：Blend SrcAlpha, One（加性）
        let sprite_blend = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
        };
        let sprite_pipeline = build_fullscreen(&device, "sprite", &sprite_shader(), "fs_main", BLOCK_FORMAT, &create_sprite_bgl(&device), sprite_blend, true, all, MSAA_SAMPLES);
        let compose1_pipeline = build_fullscreen(&device, "compose1", &compose1_shader(), "fs_main", BLOCK_FORMAT, &create_compose1_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let compose2_pipeline = build_fullscreen(&device, "compose2", &compose2_shader(), "fs_main", BLOCK_FORMAT, &create_compose2_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let edge_pipeline = build_fullscreen(&device, "edge", &edge_shader(), "fs_main", BLOCK_FORMAT, &create_edge_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let glow_pipeline = build_fullscreen(&device, "glow", &glow_shader(), "fs_main", BLOCK_FORMAT, &create_glow_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let glow_final_pipeline = build_fullscreen(&device, "glow_final", &glow_final_shader(), "fs_main", BLOCK_FORMAT, &create_glow_final_bgl(&device), wgpu::BlendState::REPLACE, false, wgpu::ColorWrites::GREEN, 1);
        let subtract_pipeline = build_fullscreen(&device, "subtract_blender", &subtract_blender_shader(), "fs_main", BLOCK_FORMAT, &create_sprite_bgl(&device), wgpu::BlendState::REPLACE, true, all, MSAA_SAMPLES);
        let disabled_pipeline = build_fullscreen(&device, "disabled", &disabled_shader(), "fs_main", config.format, &create_disabled_bgl(&device), wgpu::BlendState { color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add }, alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add } }, false, all, MSAA_SAMPLES);
        let active_pipeline = build_fullscreen(&device, "active", &active_shader(), "fs_main", config.format, &create_active_bgl(&device), wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING, false, all, 1);
        let copy_pipeline = build_fullscreen(&device, "copy", COPY_SHADER, "fs_main", config.format, &create_copy_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        // 背景压暗：叠两张黑色矩形（alpha 0.5 / 0.1），SrcAlpha, OneMinusSrcAlpha
        let overlay_blend = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
        };
        let overlay_pipeline = build_fullscreen(&device, "overlay", COPY_SHADER, "fs_main", config.format, &create_copy_bgl(&device), overlay_blend, false, all, MSAA_SAMPLES);

        let mask_size = mask_dimensions(config.width, config.height);
        let effect_size = effect_dimensions(config.width, config.height);
        let (m_normal, rt_normal) = create_rt_msaa(&device, mask_size, "normal", BLOCK_FORMAT);
        let (m_subtract, rt_subtract) = create_rt_msaa(&device, mask_size, "subtract", BLOCK_FORMAT);
        let (m_disabled_normal, rt_disabled_normal) = create_rt_msaa(&device, mask_size, "disabled_normal", BLOCK_FORMAT);
        let (m_disabled_subtract, rt_disabled_subtract) = create_rt_msaa(&device, mask_size, "disabled_subtract", BLOCK_FORMAT);
        let (m_ready_normal, rt_ready_normal) = create_rt_msaa(&device, mask_size, "ready_normal", BLOCK_FORMAT);
        let (m_ready_subtract, rt_ready_subtract) = create_rt_msaa(&device, mask_size, "ready_subtract", BLOCK_FORMAT);
        let mask_msaa = vec![m_normal, m_subtract, m_disabled_normal, m_disabled_subtract, m_ready_normal, m_ready_subtract];
        let rt_composed_enabled = create_rt(&device, mask_size, "composed_enabled");
        let rt_composed_disabled = create_rt(&device, mask_size, "composed_disabled");
        let rt_effect = create_rt(&device, effect_size, "effect");
        let ping_a = create_rt(&device, effect_size, "pingA");
        let ping_b = create_rt(&device, effect_size, "pingB");
        // sceneColorRT = Screen/6（Point）；scene_full 是 active 合成前的完整相机目标
        let scene_size = scene_dimensions(config.width, config.height);
        let rt_scene = create_rt_fmt(&device, scene_size, "scene", config.format);
        let (scene_full_msaa, rt_scene_full) = create_rt_msaa(&device, [config.width, config.height], "scene_full", config.format);

        let sprite_bg = bg_sprite(&device, &block_uniform, &mask_tex.view, &mask_sampler);
        let compose1_bg = bg_compose1(&device, &block_uniform, &rt_normal.1, &rt_subtract.1, &disp_tex.view, &rt_sampler, &disp_sampler);
        let compose2_bg = bg_compose2(&device, &block_uniform, &rt_disabled_normal.1, &rt_disabled_subtract.1, &rt_sampler);
        let edge_bg = bg_edge(&device, &block_uniform, &rt_composed_enabled.1, &rt_composed_enabled.1, &rt_sampler);

        let weights = glow_weights();
        let mut glow_configs = Vec::new();
        let mut glow_bgs = Vec::new();
        // round0: in=composedEnabled -> pingA; round1: pingA->pingB; round2: pingB->pingA; round3: pingA->pingB; round4: pingB->pingA
        let inputs = [&rt_composed_enabled.1, &ping_a.1, &ping_b.1, &ping_a.1, &ping_b.1];
        for (i, w) in weights.iter().enumerate() {
            let first = if i == 0 { 1.0 } else { 0.0 };
            let cfg = device.create_buffer(&wgpu::BufferDescriptor { label: Some("glow cfg"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            queue.write_buffer(&cfg, 0, bytemuck::cast_slice(&[*w, first, 0.0, 0.0]));
            let bg = bg_glow(&device, &block_uniform, &cfg, inputs[i], &rt_composed_enabled.1, &rt_sampler);
            glow_configs.push(cfg);
            glow_bgs.push(bg);
        }
        let glow_final_bg = bg_glow_final(&device, &block_uniform, &ping_a.1, &rt_sampler);
        let disabled_bg = bg_disabled(&device, &block_uniform, &rt_composed_disabled.1, &disp_tex.view, &spark_tex.view, &rt_sampler, &disp_sampler, &spark_sampler);
        let active_bg = bg_active(&device, &block_uniform, &rt_composed_enabled.1, &rt_effect.1, &rt_ready_normal.1, &rt_ready_subtract.1, &rt_composed_disabled.1, &disp_tex.view, &spark_tex.view, &rt_scene.1, &rt_sampler, &disp_sampler, &spark_sampler, &effect_sampler);
        let copy_bg = bg_copy(&device, &rt_scene_full.1, &rt_sampler);
        let bg_bg = bg_copy(&device, &bg_view, &bg_sampler);
        let overlay_tex_a = solid_texture(&device, &queue, [0, 0, 0, 128]); // alpha 0.5
        let overlay_tex_b = solid_texture(&device, &queue, [0, 0, 0, 26]);  // alpha 0.1
        let overlay_bg_a = bg_copy(&device, &overlay_tex_a, &bg_sampler);
        let overlay_bg_b = bg_copy(&device, &overlay_tex_b, &bg_sampler);

        let scale = window.scale_factor() as f32;
        let surface_size = [config.width as f32, config.height as f32];
        let design = [surface_size[0] / scale, surface_size[1] / scale];
        let vsync = config.present_mode == wgpu::PresentMode::Fifo;

        Self {
            surface, device, queue, config,
            pipeline, vertex_buffer, vertex_capacity: INITIAL_VERTEX_CAPACITY,
            sprite_pipeline, compose1_pipeline, compose2_pipeline, edge_pipeline, glow_pipeline, glow_final_pipeline, subtract_pipeline, disabled_pipeline, active_pipeline, copy_pipeline, overlay_pipeline,
            sprite_bg, compose1_bg, compose2_bg, edge_bg, glow_bgs, glow_final_bg, disabled_bg, active_bg, copy_bg, bg_bg, overlay_bg_a, overlay_bg_b,
            block_vertex_buffer, block_capacity: INITIAL_BLOCK_CAPACITY, block_uniform, glow_configs, rt_sampler, disp_sampler, spark_sampler, effect_sampler,
            disp_view: disp_tex.view, spark_view: spark_tex.view,
            rt_normal, rt_subtract, rt_disabled_normal, rt_disabled_subtract, rt_ready_normal, rt_ready_subtract, rt_composed_enabled, rt_composed_disabled, rt_effect, ping_a, ping_b, rt_scene_full, rt_scene, mask_msaa, scene_full_msaa,
            block_time: 0.0, size: surface_size, render_size: surface_size, render_scale: 1.0, design, fit: Fit::default(), origin: [0.0, 0.0], y_up: false, ctm: Affine2::IDENTITY, transform_stack: Vec::new(), fps: Fps::new(), vsync,
            lines: Vec::new(), vertices: Vec::new(), block_vertices: Vec::new(),
            note_pipeline, note_bind_groups, note_tex_size, note_tex_alpha, note_tex_blue, note_buffer, note_capacity: INITIAL_NOTE_CAPACITY, note_vertices: Vec::new(), note_sprites: Vec::new(), hit_frame_count,
            font, glyphs: HashMap::new(), ui_images, atlas_data, atlas_x: atlas_cursor.0, atlas_y: atlas_cursor.1, atlas_row_h: atlas_cursor.2, atlas_dirty: true,
            text_atlas_tex, text_bg, text_pipeline, text_buffer, text_capacity: INITIAL_NOTE_CAPACITY, text_vertices: Vec::new(),
            export_rt: None, export_buffer: None, export_padded_bytes_per_row: 0, export_size: [0, 0],
        }
    }

    pub fn set_vsync(&mut self, enabled: bool) {
        if self.vsync == enabled { return; }
        self.vsync = enabled;
        self.config.present_mode = if enabled { wgpu::PresentMode::Fifo } else { wgpu::PresentMode::AutoNoVsync };
        self.surface.configure(&self.device, &self.config);
    }
    pub fn fps(&self) -> f32 { self.fps.value() }
    pub fn set_coordinate_system(&mut self, origin: [f32; 2], y_up: bool) { self.origin = origin; self.y_up = y_up; }

    /// 保存当前用户坐标系变换（对应 canvas `ctx.save()`）。
    #[allow(dead_code)]
    pub fn save(&mut self) { self.transform_stack.push(self.ctm); }
    /// 恢复到最近一次 [`Self::save`] 的变换（对应 canvas `ctx.restore()`）。
    #[allow(dead_code)]
    pub fn restore(&mut self) { if let Some(m) = self.transform_stack.pop() { self.ctm = m; } }
    /// 把用户坐标系变换重置为恒等。
    #[allow(dead_code)]
    pub fn reset_transform(&mut self) { self.ctm = Affine2::IDENTITY; }
    /// 平移用户坐标系（对应 canvas `ctx.translate(x, y)`）。
    #[allow(dead_code)]
    pub fn translate(&mut self, x: f32, y: f32) { self.ctm = self.ctm.mul(Affine2::translate(x, y)); }
    /// 旋转用户坐标系，角度单位为度、逆时针为正（对应 canvas `ctx.rotate`，但用度）。
    #[allow(dead_code)]
    pub fn rotate(&mut self, deg: f32) { self.ctm = self.ctm.mul(Affine2::rotate(deg)); }
    /// 缩放用户坐标系（对应 canvas `ctx.scale(sx, sy)`）。
    #[allow(dead_code)]
    pub fn scale(&mut self, sx: f32, sy: f32) { self.ctm = self.ctm.mul(Affine2::scale(sx, sy)); }

    pub fn window_size(&self) -> [f32; 2] { self.size }
    /// 实际渲染尺寸（导出时为视频尺寸）。
    pub fn render_size(&self) -> [f32; 2] { self.render_size }
    /// 设置游戏画面缩放比例（`--size`）。
    pub fn set_render_scale(&mut self, s: f32) { self.render_scale = s.max(0.01); }
    pub fn set_viewport(&mut self, design: [f32; 2], fit: Fit) { self.design = design; self.fit = fit; }
    pub fn set_time(&mut self, time: f32) { self.block_time = time; }

    pub fn draw_line(&mut self, center: [f32; 2], length: f32, angle: f32, width: f32, color: [f32; 4]) {
        let line = Line { center, length, angle: angle.to_radians(), width, color: [color[0] / 255.0, color[1] / 255.0, color[2] / 255.0, color[3]] };
        self.lines.push(LineDraw { line, transform: self.ctm });
    }

    /// 绘制一个音符（tap/drag/flick）。`center`、`angle` 与当前用户坐标系一致，通常配合
    /// `translate` / `rotate` 放到判定线局部坐标里使用；`width` 为设计像素宽度，高度按
    /// `texture` 的贴图宽高比自动换算，`color` 的 alpha 用于整体淡入淡出。
    pub fn draw_note(&mut self, center: [f32; 2], width: f32, angle: f32, texture: NoteTexture, color: [f32; 4]) {
        self.draw_note_flip(center, width, angle, texture, color, false);
    }

    /// 同 [`Self::draw_note`]，但 `flip_v` 为真时上下翻转贴图（判定线下方的 Hold 使用）。
    pub fn draw_note_flip(&mut self, center: [f32; 2], width: f32, angle: f32, texture: NoteTexture, color: [f32; 4], flip_v: bool) {
        let tex = texture.index();
        let size = self.note_tex_size[tex];
        let height = width * size[1] / size[0] * if flip_v { -1.0 } else { 1.0 };
        self.note_sprites.push(NoteSprite { center, width, height, angle: angle.to_radians(), texture: tex, color, uv_x: [0.0, 1.0], transform: self.ctm, layer: 1 });
    }

    /// 绘制指定宽高的音符四边形（Hold 主体用，高度随 Hold 长度变化）。`uv_x` 用于裁剪贴图左右留白。
    pub fn draw_note_sized(&mut self, center: [f32; 2], width: f32, height: f32, angle: f32, texture: NoteTexture, color: [f32; 4], uv_x: [f32; 2]) {
        self.note_sprites.push(NoteSprite { center, width, height, angle: angle.to_radians(), texture: texture.index(), color, uv_x, transform: self.ctm, layer: 0 });
    }

    /// 打击特效帧数量（`assets/hit/img-*.png`）。
    pub fn hit_frame_count(&self) -> usize {
        self.hit_frame_count
    }

    /// 绘制一帧打击特效（正方形、轴对齐，不随判定线旋转）。`frame` 会被裁剪到有效范围。
    pub fn draw_hit(&mut self, center: [f32; 2], size: f32, frame: usize, color: [f32; 4]) {
        if self.hit_frame_count == 0 { return; }
        let frame = frame.min(self.hit_frame_count - 1);
        self.note_sprites.push(NoteSprite { center, width: size, height: size, angle: 0.0, texture: NOTE_TEXTURE_COUNT + frame, color, uv_x: [0.0, 1.0], transform: self.ctm, layer: 2 });
    }

    /// 绘制打击特效的金色小方块（轴对齐正方形，用顶点色染色）。
    pub fn draw_spark(&mut self, center: [f32; 2], size: f32, color: [f32; 4]) {
        let texture = NOTE_TEXTURE_COUNT + self.hit_frame_count;
        self.note_sprites.push(NoteSprite { center, width: size, height: size, angle: 0.0, texture, color, uv_x: [0.0, 1.0], transform: self.ctm, layer: 2 });
    }

    /// 在文本图集里分配一块区域。
    fn atlas_alloc(&mut self, w: u32, h: u32) -> (u32, u32) {
        let mut c = (self.atlas_x, self.atlas_y, self.atlas_row_h);
        let p = atlas_pack(&mut c, w, h);
        self.atlas_x = c.0; self.atlas_y = c.1; self.atlas_row_h = c.2;
        p
    }

    /// 取得（必要时光栅化并打包）指定像素高度的字符字形。
    fn glyph_for(&mut self, ch: char, px: u32) -> Glyph {
        let key = (ch as u32, px);
        if let Some(g) = self.glyphs.get(&key) { return *g; }
        let scale = PxScale::from(px as f32);
        let advance = self.font.as_scaled(scale).h_advance(self.font.glyph_id(ch));
        let glyph = if let Some(outline) = self.font.outline_glyph(self.font.glyph_id(ch).with_scale_and_position(scale, ab_glyph::point(0.0, 0.0))) {
            let b = outline.px_bounds();
            let w = (b.width().ceil() as u32).max(1);
            let h = (b.height().ceil() as u32).max(1);
            let mut cov = vec![0u8; (w * h) as usize];
            outline.draw(|x, y, c| {
                if x < w && y < h { cov[(y * w + x) as usize] = (c * 255.0 + 0.5).clamp(0.0, 255.0) as u8; }
            });
            let (ax, ay) = self.atlas_alloc(w + 2, h + 2);
            for yy in 0..h {
                for xx in 0..w {
                    let idx = (((ay + 1 + yy) * TEXT_ATLAS_SIZE + (ax + 1 + xx)) * 4) as usize;
                    let c = cov[(yy * w + xx) as usize];
                    self.atlas_data[idx] = 255;
                    self.atlas_data[idx + 1] = 255;
                    self.atlas_data[idx + 2] = 255;
                    self.atlas_data[idx + 3] = c;
                }
            }
            self.atlas_dirty = true;
            Glyph {
                uv: [
                    (ax + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                    (ay + 1) as f32 / TEXT_ATLAS_SIZE as f32,
                    (ax + 1 + w) as f32 / TEXT_ATLAS_SIZE as f32,
                    (ay + 1 + h) as f32 / TEXT_ATLAS_SIZE as f32,
                ],
                size: [w as f32, h as f32],
                left: b.min.x,
                top: -b.min.y,
                advance,
            }
        } else {
            Glyph { uv: [0.0; 4], size: [0.0, 0.0], left: 0.0, top: 0.0, advance }
        };
        self.glyphs.insert(key, glyph);
        glyph
    }

    /// 文本渲染宽度（像素）。
    pub fn measure_text(&self, text: &str, px: f32) -> f32 {
        let scaled = self.font.as_scaled(PxScale::from(px));
        text.chars().map(|c| scaled.h_advance(self.font.glyph_id(c))).sum()
    }

    /// 以 `(x, baseline_y)` 为基线、`px` 为字号绘制文本（用户空间，y 向上）。
    pub fn draw_text(&mut self, text: &str, x: f32, baseline_y: f32, px: f32, color: [f32; 4]) {
        let base = self.to_ndc();
        let pxu = px.round().max(1.0) as u32;
        let mut pen = x;
        for ch in text.chars() {
            let g = self.glyph_for(ch, pxu);
            if g.size[0] > 0.0 {
                let left = pen + g.left;
                let top = baseline_y + g.top;
                let (w, h) = (g.size[0], g.size[1]);
                let (u0, v0, u1, v1) = (g.uv[0], g.uv[1], g.uv[2], g.uv[3]);
                let tl = base([left, top]);
                let tr = base([left + w, top]);
                let br = base([left + w, top - h]);
                let bl = base([left, top - h]);
                for (p, uv) in [(tl, [u0, v0]), (tr, [u1, v0]), (br, [u1, v1]), (tl, [u0, v0]), (br, [u1, v1]), (bl, [u0, v1])] {
                    self.text_vertices.push(NoteVertex { position: p, uv, color });
                }
            }
            pen += g.advance;
        }
    }

    /// 绘制一张打包进文本图集的 UI 图片（在用户空间中按 `center`/`size` 居中）。
    pub fn draw_ui_image(&mut self, key: &str, center: [f32; 2], size: [f32; 2]) {
        self.draw_ui_image_color(key, center, size, [1.0, 1.0, 1.0, 1.0]);
    }

    /// 同 [`draw_ui_image`]，但用 `color` 着色（用于纯色矩形/进度条）。
    pub fn draw_ui_image_color(&mut self, key: &str, center: [f32; 2], size: [f32; 2], color: [f32; 4]) {
        let Some(img) = self.ui_images.get(key).copied() else { return };
        let base = self.to_ndc();
        let (hw, hh) = (size[0] / 2.0, size[1] / 2.0);
        let (u0, v0, u1, v1) = (img.uv[0], img.uv[1], img.uv[2], img.uv[3]);
        let tl = base([center[0] - hw, center[1] + hh]);
        let tr = base([center[0] + hw, center[1] + hh]);
        let br = base([center[0] + hw, center[1] - hh]);
        let bl = base([center[0] - hw, center[1] - hh]);
        for (p, uv) in [(tl, [u0, v0]), (tr, [u1, v0]), (br, [u1, v1]), (tl, [u0, v0]), (br, [u1, v1]), (bl, [u0, v1])] {
            self.text_vertices.push(NoteVertex { position: p, uv, color });
        }
    }

    /// 绘制纯色矩形（基于图集内的 1×1 白色块）。
    pub fn draw_rect(&mut self, center: [f32; 2], size: [f32; 2], color: [f32; 4]) {
        self.draw_ui_image_color("ui/white", center, size, color);
    }

    /// 直接以 NDC 坐标绘制纯色矩形（不受坐标缩放影响，用于视口边框等叠加）。
    pub fn draw_rect_ndc(&mut self, center: [f32; 2], size: [f32; 2], color: [f32; 4]) {
        let Some(img) = self.ui_images.get("ui/white").copied() else { return };
        let (hw, hh) = (size[0] / 2.0, size[1] / 2.0);
        let (u0, v0, u1, v1) = (img.uv[0], img.uv[1], img.uv[2], img.uv[3]);
        let tl = [center[0] - hw, center[1] + hh];
        let tr = [center[0] + hw, center[1] + hh];
        let br = [center[0] + hw, center[1] - hh];
        let bl = [center[0] - hw, center[1] - hh];
        for (p, uv) in [(tl, [u0, v0]), (tr, [u1, v0]), (br, [u1, v1]), (tl, [u0, v0]), (br, [u1, v1]), (bl, [u0, v1])] {
            self.text_vertices.push(NoteVertex { position: p, uv, color });
        }
    }

    /// 音符贴图的高宽比（`height / width`），用于按宽度推算高度。
    pub fn note_aspect(&self, texture: NoteTexture) -> f32 {
        let size = self.note_tex_size[texture.index()];
        size[1] / size[0]
    }

    /// 贴图非透明内容的横向范围占全宽比例。
    pub fn note_alpha_width(&self, texture: NoteTexture) -> f32 {
        self.note_tex_alpha[texture.index()]
    }

    /// 贴图蓝色主色内容的横向范围占全宽比例（无蓝色时退回 alpha 宽度）。
    pub fn note_blue_width(&self, texture: NoteTexture) -> f32 {
        self.note_tex_blue[texture.index()]
    }

    pub fn draw_block(&mut self, center: [f32; 2], width: f32, height: f32, angle: f32, color: [f32; 4], phase: f32) {
        let ctm = self.ctm;
        let map = self.to_ndc();
        let (hx, hy) = (width / 2.0, height / 2.0);
        let rad = angle.to_radians();
        // 垂直向量取 (sin, -cos)：`to_ndc` 里已并入 y 轴翻转，
        // 与旧的「先取负角度再映射」等价，保证贴图朝向不变。
        let dir = [rad.cos(), rad.sin()];
        let perp = [rad.sin(), -rad.cos()];
        let corner = |dx: f32, dy: f32| map(ctm.apply([center[0] + dir[0] * dx + perp[0] * dy, center[1] + dir[1] * dx + perp[1] * dy]));
        let a0 = corner(-hx, -hy); let a1 = corner(-hx, hy); let b1 = corner(hx, hy); let b0 = corner(hx, -hy);
        let v = |position: [f32; 2], uv: [f32; 2]| BlockVertex { position, uv, color, phase };
        for (pos, uv) in [(a0, [0.0, 1.0]), (a1, [0.0, 0.0]), (b1, [1.0, 0.0]), (a0, [0.0, 1.0]), (b1, [1.0, 0.0]), (b0, [1.0, 1.0])] {
            self.block_vertices.push(v(pos, uv));
        }
    }

    pub fn clear(&mut self) { self.lines.clear(); self.block_vertices.clear(); self.note_sprites.clear(); self.text_vertices.clear(); }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        self.config.width = width; self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.size = [width as f32, height as f32];
        self.render_size = [width as f32, height as f32];
        self.recreate_targets(width, height);
    }

    /// 设定实际渲染/导出尺寸（不改动窗口 surface）。导出时窗口与视频尺寸分离。
    pub fn set_render_size(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        self.render_size = [width as f32, height as f32];
        self.recreate_targets(width, height);
    }

    /// 按给定尺寸重建全部离屏 RT 与相关绑定。
    fn recreate_targets(&mut self, width: u32, height: u32) {
        let mask_size = mask_dimensions(width, height);
        let effect_size = effect_dimensions(width, height);
        let (m_normal, rt_normal) = create_rt_msaa(&self.device, mask_size, "normal", BLOCK_FORMAT);
        let (m_subtract, rt_subtract) = create_rt_msaa(&self.device, mask_size, "subtract", BLOCK_FORMAT);
        let (m_disabled_normal, rt_disabled_normal) = create_rt_msaa(&self.device, mask_size, "disabled_normal", BLOCK_FORMAT);
        let (m_disabled_subtract, rt_disabled_subtract) = create_rt_msaa(&self.device, mask_size, "disabled_subtract", BLOCK_FORMAT);
        let (m_ready_normal, rt_ready_normal) = create_rt_msaa(&self.device, mask_size, "ready_normal", BLOCK_FORMAT);
        let (m_ready_subtract, rt_ready_subtract) = create_rt_msaa(&self.device, mask_size, "ready_subtract", BLOCK_FORMAT);
        self.rt_normal = rt_normal; self.rt_subtract = rt_subtract; self.rt_disabled_normal = rt_disabled_normal; self.rt_disabled_subtract = rt_disabled_subtract; self.rt_ready_normal = rt_ready_normal; self.rt_ready_subtract = rt_ready_subtract;
        self.mask_msaa = vec![m_normal, m_subtract, m_disabled_normal, m_disabled_subtract, m_ready_normal, m_ready_subtract];
        self.rt_composed_enabled = create_rt(&self.device, mask_size, "composed_enabled");
        self.rt_composed_disabled = create_rt(&self.device, mask_size, "composed_disabled");
        self.rt_effect = create_rt(&self.device, effect_size, "effect");
        self.ping_a = create_rt(&self.device, effect_size, "pingA");
        self.ping_b = create_rt(&self.device, effect_size, "pingB");
        let scene_size = scene_dimensions(width, height);
        self.rt_scene = create_rt_fmt(&self.device, scene_size, "scene", self.config.format);
        let (scene_full_msaa, rt_scene_full) = create_rt_msaa(&self.device, [width, height], "scene_full", self.config.format);
        self.scene_full_msaa = scene_full_msaa;
        self.rt_scene_full = rt_scene_full;

        let disp = self.disp_view.clone();
        let spark = self.spark_view.clone();
        let c1 = bg_compose1(&self.device, &self.block_uniform, &self.rt_normal.1, &self.rt_subtract.1, &disp, &self.rt_sampler, &self.disp_sampler);
        let c2 = bg_compose2(&self.device, &self.block_uniform, &self.rt_disabled_normal.1, &self.rt_disabled_subtract.1, &self.rt_sampler);
        let eb = bg_edge(&self.device, &self.block_uniform, &self.rt_composed_enabled.1, &self.rt_composed_enabled.1, &self.rt_sampler);
        self.compose1_bg = c1;
        self.compose2_bg = c2;
        self.edge_bg = eb;
        let weights = glow_weights();
        let inputs = [&self.rt_composed_enabled.1, &self.ping_a.1, &self.ping_b.1, &self.ping_a.1, &self.ping_b.1];
        for (i, w) in weights.iter().enumerate() {
            let first = if i == 0 { 1.0 } else { 0.0 };
            self.queue.write_buffer(&self.glow_configs[i], 0, bytemuck::cast_slice(&[*w, first, 0.0, 0.0]));
            let bg = bg_glow(&self.device, &self.block_uniform, &self.glow_configs[i], inputs[i], &self.rt_composed_enabled.1, &self.rt_sampler);
            self.glow_bgs[i] = bg;
        }
        let gf = bg_glow_final(&self.device, &self.block_uniform, &self.ping_a.1, &self.rt_sampler);
        let db = bg_disabled(&self.device, &self.block_uniform, &self.rt_composed_disabled.1, &disp, &spark, &self.rt_sampler, &self.disp_sampler, &self.spark_sampler);
        let ab = bg_active(&self.device, &self.block_uniform, &self.rt_composed_enabled.1, &self.rt_effect.1, &self.rt_ready_normal.1, &self.rt_ready_subtract.1, &self.rt_composed_disabled.1, &disp, &spark, &self.rt_scene.1, &self.rt_sampler, &self.disp_sampler, &self.spark_sampler, &self.effect_sampler);
        self.copy_bg = bg_copy(&self.device, &self.rt_scene_full.1, &self.rt_sampler);
        self.glow_final_bg = gf;
        self.disabled_bg = db;
        self.active_bg = ab;
    }

    fn viewport(&self) -> (f32, f32, f32, f32) {
        let (win_w, win_h) = (self.render_size[0], self.render_size[1]);
        let (design_w, design_h) = (self.design[0], self.design[1]);
        let (mut sx, mut sy) = match self.fit {
            Fit::Stretch => (win_w / design_w, win_h / design_h),
            Fit::Contain => { let s = (win_w / design_w).min(win_h / design_h); (s, s) }
        };
        sx *= self.render_scale;
        sy *= self.render_scale;
        let ox = (win_w - design_w * sx) / 2.0;
        let oy = (win_h - design_h * sy) / 2.0;
        (sx, sy, ox, oy)
    }

    /// 返回「用户坐标 -> NDC」的基础映射：按原点、y 轴方向（y 上时翻转 y）与视口缩放映射。
    /// 不含 `ctm`——变换在 `draw_*` 时已捕获进各图元，绘制时对顶点应用。闭包只捕获副本。
    fn to_ndc(&self) -> impl Fn([f32; 2]) -> [f32; 2] + use<> {
        let origin = self.origin;
        let y_up = self.y_up;
        let size = self.render_size;
        let (sx, sy, ox, oy) = self.viewport();
        move |p: [f32; 2]| {
            let px = ox + (origin[0] + p[0]) * sx;
            let py = oy + (origin[1] + if y_up { -p[1] } else { p[1] }) * sy;
            [px / size[0] * 2.0 - 1.0, 1.0 - py / size[1] * 2.0]
        }
    }

    pub fn render(&mut self) { self.render_impl(); }

    fn render_impl(&mut self) {
        self.fps.tick();
        let (block_count, line_count, note_draws) = self.prepare_frame();
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => { self.surface.configure(&self.device, &self.config); return; }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        self.encode_scene(&mut encoder, &view, block_count, line_count, &note_draws);
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
    }

    /// 构建本帧全部顶点/贴图上传，返回 `(块顶点数, 线段顶点数, 音符绘制区间)`。
    fn prepare_frame(&mut self) -> (u32, u32, Vec<(usize, u32, u32)>) {
        let base = self.to_ndc();

        self.vertices.clear();
        for d in &self.lines {
            let l = d.line;
            let corners = line::corners(l.center, l.length, l.angle, l.width);
            let c0 = base(d.transform.apply(corners[0])); let c1 = base(d.transform.apply(corners[1])); let c2 = base(d.transform.apply(corners[2])); let c3 = base(d.transform.apply(corners[3]));
            let (mut min_x, mut max_x) = (c0[0], c0[0]); let (mut min_y, mut max_y) = (c0[1], c0[1]);
            for p in [c1, c2, c3] { min_x = min_x.min(p[0]); max_x = max_x.max(p[0]); min_y = min_y.min(p[1]); max_y = max_y.max(p[1]); }
            if max_x < -1.0 || min_x > 1.0 || max_y < -1.0 || min_y > 1.0 { continue; }
            let color = l.color;
            for p in [c0, c1, c2, c0, c2, c3] { self.vertices.push(Vertex { position: p, color }); }
        }

        // 音符：按贴图分组生成顶点，记录每张贴图的连续绘制区间，避免逐个音符切换 bind group。
        self.note_vertices.clear();
        let mut note_draws: Vec<(usize, u32, u32)> = Vec::new();
        if !self.note_sprites.is_empty() {
            let mut order: Vec<usize> = (0..self.note_sprites.len()).collect();
            order.sort_by_key(|&i| (self.note_sprites[i].layer, self.note_sprites[i].texture));
            for &i in &order {
                let s = self.note_sprites[i];
                let start = self.note_vertices.len() as u32;
                let (hw, hh) = (s.width / 2.0, s.height / 2.0);
                let dir = [s.angle.cos(), s.angle.sin()];
                let perp = [-dir[1], dir[0]];
                let corner = |dx: f32, dy: f32| base(s.transform.apply([s.center[0] + dir[0] * dx + perp[0] * dy, s.center[1] + dir[1] * dx + perp[1] * dy]));
                let a0 = corner(-hw, -hh); let a1 = corner(-hw, hh); let b1 = corner(hw, hh); let b0 = corner(hw, -hh);
                let v = |position: [f32; 2], uv: [f32; 2]| NoteVertex { position, uv, color: s.color };
                let (u0, u1) = (s.uv_x[0], s.uv_x[1]);
                for (pos, uv) in [(a0, [u0, 1.0]), (a1, [u0, 0.0]), (b1, [u1, 0.0]), (a0, [u0, 1.0]), (b1, [u1, 0.0]), (b0, [u1, 1.0])] {
                    self.note_vertices.push(v(pos, uv));
                }
                match note_draws.last_mut() {
                    Some((t, _, count)) if *t == s.texture => *count += 6,
                    _ => note_draws.push((s.texture, start, 6)),
                }
            }
            self.ensure_note_capacity(self.note_vertices.len());
            self.queue.write_buffer(&self.note_buffer, 0, bytemuck::cast_slice(&self.note_vertices));
        }

        let line_count = self.vertices.len() as u32;
        if line_count > 0 { self.ensure_vertex_capacity(self.vertices.len()); self.queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertices)); }

        let block_count = self.block_vertices.len() as u32;
        if block_count > 0 {
            self.block_vertices.sort_by(|a, b| a.phase.partial_cmp(&b.phase).unwrap());
            self.ensure_block_capacity(self.block_vertices.len());
            self.queue.write_buffer(&self.block_vertex_buffer, 0, bytemuck::cast_slice(&self.block_vertices));
            let rw = self.render_size[0] as u32;
            let rh = self.render_size[1] as u32;
            let ew = (rw / EFFECT_DOWNSCALE).max(1);
            let eh = (rh / EFFECT_DOWNSCALE).max(1);
            let uniform = BlockUniform {
                fill: [0.713, 0.235, 0.235, 0.667],
                edge: [1.0, 0.330, 0.330, 0.80],
                glow: [1.0, 0.179, 0.179, 0.80],
                params: [self.block_time, 0.667, 0.411, 6.0],
                displace: [1.5, 0.15, 0.7071, 0.7071],
                texel: [1.0 / ew as f32, 1.0 / eh as f32, ew as f32, eh as f32],
                dis_fill: [0.497, 0.138, 0.138, 0.40],
                spark: [1.0, 0.285, 0.285, 5.69],
                spark2: [2.39, 0.20, 37.9, 0.12],   // _SparkDisplaceIntensity, _SparkHueShiftAmount, _ShineSpeed, _ShineBrightness
                st_spark: [3.0, 1.2, 0.0, 0.0],
                displace_compose: [2.59, 0.10, 0.5, 0.5],
                st_compose: [2.13, 1.02, 0.0, 0.0],
                st_active: [0.80, 0.30, 0.0, 0.0],
                screen: [self.render_size[0], self.render_size[1], 1.0 / self.render_size[0], 1.0 / self.render_size[1]],
                blend: [0.09, 0.12, 0.0, 0.0],
                dis_spark: [0.311, 0.078, 0.078, 3.5],
                dis_params: [0.30, 2.29, 0.0, 0.0],
                st_disabled: [0.50, 0.20, 0.0, 0.0],
            };
            self.queue.write_buffer(&self.block_uniform, 0, bytemuck::bytes_of(&uniform));
        }

        self.upload_text();
        (block_count, line_count, note_draws)
    }

    /// 上传文本图集（有变化时）与文本顶点。
    fn upload_text(&mut self) {
        if self.atlas_dirty {
            self.queue.write_texture(
                self.text_atlas_tex.as_image_copy(),
                &self.atlas_data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * TEXT_ATLAS_SIZE), rows_per_image: Some(TEXT_ATLAS_SIZE) },
                wgpu::Extent3d { width: TEXT_ATLAS_SIZE, height: TEXT_ATLAS_SIZE, depth_or_array_layers: 1 },
            );
            self.atlas_dirty = false;
        }
        if !self.text_vertices.is_empty() {
            self.ensure_text_capacity(self.text_vertices.len());
            self.queue.write_buffer(&self.text_buffer, 0, bytemuck::cast_slice(&self.text_vertices));
        }
    }

    fn ensure_vertex_capacity(&mut self, needed: usize) { if needed <= self.vertex_capacity { return; } let c = needed.next_power_of_two(); self.vertex_buffer = create_vertex_buffer(&self.device, c); self.vertex_capacity = c; }
    fn ensure_block_capacity(&mut self, needed: usize) { if needed <= self.block_capacity { return; } let c = needed.next_power_of_two(); self.block_vertex_buffer = create_block_vertex_buffer(&self.device, c); self.block_capacity = c; }
    fn ensure_note_capacity(&mut self, needed: usize) { if needed <= self.note_capacity { return; } let c = needed.next_power_of_two(); self.note_buffer = create_note_vertex_buffer(&self.device, c); self.note_capacity = c; }
    fn ensure_text_capacity(&mut self, needed: usize) { if needed <= self.text_capacity { return; } let c = needed.next_power_of_two(); self.text_buffer = create_note_vertex_buffer(&self.device, c); self.text_capacity = c; }

    fn phase_range(&self, phase: f32) -> (u32, u32) {
        let start = self.block_vertices.partition_point(|v| v.phase < phase) as u32;
        let end = self.block_vertices.partition_point(|v| v.phase <= phase) as u32;
        (start, end)
    }

    fn encode_scene(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, block_count: u32, line_count: u32, note_draws: &[(usize, u32, u32)]) {
        if block_count > 0 {
            // 6 张遮罩：普通块用 BlockSprite，减块用 SubtractBlockBlender（逐 quad）
            let targets = [&self.rt_normal.1, &self.rt_subtract.1, &self.rt_disabled_normal.1, &self.rt_disabled_subtract.1, &self.rt_ready_normal.1, &self.rt_ready_subtract.1];
            let phases = [PHASE_NORMAL, PHASE_SUBTRACT, PHASE_DISABLED_NORMAL, PHASE_DISABLED_SUBTRACT, PHASE_READY_NORMAL, PHASE_READY_SUBTRACT];
            let pipes = [&self.sprite_pipeline, &self.subtract_pipeline, &self.sprite_pipeline, &self.subtract_pipeline, &self.sprite_pipeline, &self.subtract_pipeline];
            // ready 阶段同时写入 merged（disabled_*）与 ready_* 两张
            let extra = [(4usize, 2usize), (5usize, 3usize)];
            for (i, target) in targets.iter().enumerate() {
                let range = self.phase_range(phases[i]);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("sprite"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &self.mask_msaa[i].1, depth_slice: None, resolve_target: Some(target), ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store } })],
                    ..Default::default()
                });
                // merged 额外接收对应的 ready 阶段
                let mut ranges = vec![range];
                for (r, m) in extra { if m == i { ranges.push(self.phase_range(phases[r])); } }
                pass.set_pipeline(pipes[i]);
                pass.set_bind_group(0, &self.sprite_bg, &[]);
                pass.set_vertex_buffer(0, self.block_vertex_buffer.slice(..));
                for r in ranges { if r.1 > r.0 { pass.draw(r.0..r.1, 0..1); } }
            }

            // compose1 / compose2
            self.fullscreen_pass(encoder, &self.rt_composed_enabled.1, &self.compose1_pipeline, &self.compose1_bg);
            self.fullscreen_pass(encoder, &self.rt_composed_disabled.1, &self.compose2_pipeline, &self.compose2_bg);
            // edge -> effect.R
            self.fullscreen_pass(encoder, &self.rt_effect.1, &self.edge_pipeline, &self.edge_bg);
            // glow ping-pong
            let outputs = [&self.ping_a.1, &self.ping_b.1, &self.ping_a.1, &self.ping_b.1, &self.ping_a.1];
            for i in 0..self.glow_bgs.len() {
                self.fullscreen_pass(encoder, outputs[i], &self.glow_pipeline, &self.glow_bgs[i]);
            }
            // 末轮只写 effect.G（保留 effect.R 的边缘），故必须 load 而非 clear
            self.fullscreen_pass_load(encoder, &self.rt_effect.1, &self.glow_final_pipeline, &self.glow_final_bg);
        }

        // 1) 背景 + 压暗 + 判定线 + DisabledBlock：一次 4×MSAA 渲染到 scene_full_msaa，
        //    resolve 到 scene_full（= CameraTarget 内容），供 active 的 `_SceneColor`。
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene_msaa"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &self.scene_full_msaa.1, depth_slice: None, resolve_target: Some(&self.rt_scene_full.1), ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }), store: wgpu::StoreOp::Store } })],
                ..Default::default()
            });
            // 背景（IllustrationBlur）+ 两层黑色压暗
            pass.set_pipeline(&self.overlay_pipeline);
            pass.set_bind_group(0, &self.bg_bg, &[]);
            pass.draw(0..3, 0..1);
            pass.set_bind_group(0, &self.overlay_bg_a, &[]);
            pass.draw(0..3, 0..1);
            pass.set_bind_group(0, &self.overlay_bg_b, &[]);
            pass.draw(0..3, 0..1);
            if line_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..line_count, 0..1);
            }
            // 音符（在判定线之上、DisabledBlock 之下）
            if !note_draws.is_empty() {
                pass.set_pipeline(&self.note_pipeline);
                pass.set_vertex_buffer(0, self.note_buffer.slice(..));
                for (tex, start, count) in note_draws {
                    pass.set_bind_group(0, &self.note_bind_groups[*tex], &[]);
                    pass.draw(*start..*start + *count, 0..1);
                }
            }
            if block_count > 0 {
                pass.set_pipeline(&self.disabled_pipeline);
                pass.set_bind_group(0, &self.disabled_bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        // 2) Blit(CameraTarget -> sceneColorRT)，供 ActiveBlock 的 `_SceneColor`
        self.fullscreen_pass(encoder, &self.rt_scene.1, &self.copy_pipeline, &self.copy_bg);
        // 3) ActiveBlock 预乘覆盖到 scene_full（此时 `_SceneColor` 已就绪）
        if block_count > 0 {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("active"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &self.rt_scene_full.1, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                ..Default::default()
            });
            pass.set_pipeline(&self.active_pipeline);
            pass.set_bind_group(0, &self.active_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        // 3.5) HUD（分数/连击/暂停/进度/水印）：叠加到 scene_full 最上层
        if !self.text_vertices.is_empty() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &self.rt_scene_full.1, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                ..Default::default()
            });
            pass.set_pipeline(&self.text_pipeline);
            pass.set_bind_group(0, &self.text_bg, &[]);
            pass.set_vertex_buffer(0, self.text_buffer.slice(..));
            pass.draw(0..self.text_vertices.len() as u32, 0..1);
        }
        // 4) 合成结果拷贝到目标（交换链或离屏导出纹理）
        self.fullscreen_pass(encoder, view, &self.copy_pipeline, &self.copy_bg);
    }

    /// 渲染一帧到离屏导出纹理（供视频导出），不触碰窗口交换链。
    pub fn render_export(&mut self) {
        let (block_count, line_count, note_draws) = self.prepare_frame();
        self.ensure_export();
        let (tex, view) = self.export_rt.clone().unwrap();
        let [w, h] = self.export_size;
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("export") });
        self.encode_scene(&mut encoder, &view, block_count, line_count, &note_draws);
        encoder.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: self.export_buffer.as_ref().unwrap(),
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.export_padded_bytes_per_row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// 读取上一帧 `render_export` 的结果，返回紧密排列的 BGRA 像素（去除行对齐填充）。
    pub fn read_export(&mut self) -> Vec<u8> {
        let buffer = self.export_buffer.as_ref().unwrap();
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let _ = rx.recv();
        let (w, h) = (self.export_size[0] as usize, self.export_size[1] as usize);
        let padded = self.export_padded_bytes_per_row as usize;
        let mut out = vec![0u8; w * h * 4];
        {
            let data = slice.get_mapped_range().unwrap();
            for row in 0..h {
                out[row * w * 4..(row + 1) * w * 4].copy_from_slice(&data[row * padded..row * padded + w * 4]);
            }
        }
        buffer.unmap();
        out
    }

    /// 确保离屏导出纹理/回读缓冲与当前尺寸一致。
    fn ensure_export(&mut self) {
        let (w, h) = ((self.render_size[0] as u32).max(1), (self.render_size[1] as u32).max(1));
        if self.export_rt.is_some() && self.export_size == [w, h] { return; }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("export"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: self.config.format, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let padded = ((w * 4 + wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1) / wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("export readback"), size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        self.export_rt = Some((tex, view));
        self.export_buffer = Some(buffer);
        self.export_padded_bytes_per_row = padded;
        self.export_size = [w, h];
    }

    /// 导出期间在窗口上显示进度。直接以窗口尺寸绘制，避免被拉伸。
    pub fn present_progress(&mut self, title: &str, lines: &[String], pct: f32) {
        self.text_vertices.clear();
        // 临时切到「窗口尺寸、1:1」的坐标系来排版进度。
        let saved = (self.render_size, self.design, self.fit, self.origin, self.y_up, self.render_scale);
        let (w, h) = (self.size[0], self.size[1]);
        self.render_size = [w, h];
        self.design = [w, h];
        self.fit = Fit::Stretch;
        self.origin = [w / 2.0, h / 2.0];
        self.y_up = true;
        self.render_scale = 1.0;

        let fg = [0.92, 0.92, 0.92, 1.0];
        let green = [0.30, 0.85, 0.42, 1.0]; // 绿色进度填充
        let p = pct.clamp(0.0, 1.0);
        let s = (w / 960.0).min(h / 540.0).max(0.6);
        let px = 40.0 * s;

        // 全部文本同一字号、同一颜色（不区分标题/正文）。
        let mut items: Vec<String> = Vec::with_capacity(lines.len() + 2);
        items.push(title.to_string());
        items.extend(lines.iter().cloned());
        items.push(format!("{:.1}%", p * 100.0));
        let mut y = h / 2.0 - 46.0 * s;
        for it in &items {
            let tw = self.measure_text(it, px);
            self.draw_text(it, -tw / 2.0, y, px, fg);
            y -= px * 1.6;
        }

        // 进度条：白色边框 + 深槽 + 绿色填充。
        let bw = w * 0.86;
        let bh = 40.0 * s;
        let bx = -bw / 2.0;
        let by = -h / 2.0 + 92.0 * s;
        self.draw_rect([0.0, by], [bw + 8.0 * s, bh + 8.0 * s], [0.95, 0.95, 0.95, 1.0]);
        self.draw_rect([0.0, by], [bw, bh], [0.06, 0.07, 0.07, 1.0]);
        if p > 0.0 {
            self.draw_rect([bx + bw * p / 2.0, by], [bw * p, bh], green);
        }
        self.upload_text();
        // 还原渲染坐标系。
        (self.render_size, self.design, self.fit, self.origin, self.y_up, self.render_scale) = saved;

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => { self.surface.configure(&self.device, &self.config); return; }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("progress") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("progress"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.04, g: 0.05, b: 0.07, a: 1.0 }), store: wgpu::StoreOp::Store } })],
                ..Default::default()
            });
            if !self.text_vertices.is_empty() {
                pass.set_pipeline(&self.text_pipeline);
                pass.set_bind_group(0, &self.text_bg, &[]);
                pass.set_vertex_buffer(0, self.text_buffer.slice(..));
                pass.draw(0..self.text_vertices.len() as u32, 0..1);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
    }

    fn fullscreen_pass(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, pipeline: &wgpu::RenderPipeline, bg: &wgpu::BindGroup) {
        self.fullscreen_pass_op(encoder, target, pipeline, bg, wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT));
    }

    fn fullscreen_pass_load(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, pipeline: &wgpu::RenderPipeline, bg: &wgpu::BindGroup) {
        self.fullscreen_pass_op(encoder, target, pipeline, bg, wgpu::LoadOp::Load);
    }

    fn fullscreen_pass_op(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, pipeline: &wgpu::RenderPipeline, bg: &wgpu::BindGroup, load: wgpu::LoadOp<wgpu::Color>) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fullscreen"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: target, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load, store: wgpu::StoreOp::Store } })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
    }
}

// ---- 辅助 ----

/// 文本图集货架式打包：返回左上角，并推进游标 `(x, y, 行高)`。
fn atlas_pack(cursor: &mut (u32, u32, u32), w: u32, h: u32) -> (u32, u32) {
    let (mut x, mut y, mut row_h) = *cursor;
    if x + w > TEXT_ATLAS_SIZE { x = 0; y += row_h; row_h = 0; }
    if y + h > TEXT_ATLAS_SIZE { panic!("文本图集已满"); }
    let pos = (x, y);
    x += w;
    row_h = row_h.max(h);
    *cursor = (x, y, row_h);
    pos
}

/// 把一张 RGBA 图片写入图集数据（不处理 alpha 预乘）。
fn atlas_blit(data: &mut [u8], x: u32, y: u32, img: &image::RgbaImage) {
    let (w, h) = (img.width(), img.height());
    for yy in 0..h {
        for xx in 0..w {
            let px = img.get_pixel(xx, yy).0;
            let idx = (((y + yy) * TEXT_ATLAS_SIZE + (x + xx)) * 4) as usize;
            data[idx..idx + 4].copy_from_slice(&px);
        }
    }
}

fn glow_weights() -> Vec<f32> {
    let mut sum = 0.0f32;
    for i in 1..=GLOW_RADIUS { sum += (i as f32).powf(GLOW_FALLOFF); }
    let mut w = Vec::new();
    for p in 0..GLOW_RADIUS {
        let v = ((GLOW_RADIUS - p) as f32).powf(GLOW_FALLOFF) / sum;
        if v < GLOW_THRESHOLD { break; }
        w.push(v);
    }
    w
}

struct LoadedTexture { view: wgpu::TextureView, size: [u32; 2], alpha_width: f32, blue_width: f32 }

/// 加载内嵌资源（键相对 `assets/`、正斜杠）为贴图。
fn load_texture(device: &wgpu::Device, queue: &wgpu::Queue, key: &str, srgb: bool) -> LoadedTexture {
    load_texture_tinted(device, queue, key, srgb, None)
}

/// 同 [`load_texture`]，但可把像素 RGB 统一替换为 `tint`（保留 alpha），用于把打击特效
/// 贴图染成金色（参考 CHCAT_Phi 的 `applyGoldenEffect`）。
fn load_texture_tinted(device: &wgpu::Device, queue: &wgpu::Queue, key: &str, srgb: bool, tint: Option<[u8; 3]>) -> LoadedTexture {
    load_texture_bytes(device, queue, key, crate::embedded::expect(key), srgb, tint)
}

/// 从磁盘文件加载贴图（背景图随谱面提供，不内嵌）。
fn load_texture_file(device: &wgpu::Device, queue: &wgpu::Queue, path: &str, srgb: bool) -> LoadedTexture {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("读取贴图 {path} 失败: {e}"));
    load_texture_bytes(device, queue, path, &bytes, srgb, None)
}

/// 从内存字节解码并创建贴图。
fn load_texture_bytes(device: &wgpu::Device, queue: &wgpu::Queue, label: &str, bytes: &[u8], srgb: bool, tint: Option<[u8; 3]>) -> LoadedTexture {
    let dyn_image = image::load_from_memory(bytes).unwrap_or_else(|e| panic!("解码贴图 {label} 失败: {e}"));
    let (width, height) = (dyn_image.width(), dyn_image.height());
    let mut data = dyn_image.to_rgba8();
    if let Some([r, g, b]) = tint {
        for px in data.pixels_mut() {
            px[0] = r;
            px[1] = g;
            px[2] = b;
        }
    }
    // 贴图左右往往有透明留白。`alpha_width` 为非透明内容的横向范围；
    // `blue_width` 为「蓝色主色（B 明显大于 R、G）」内容的横向范围（排除 HL 的黄色辉光），
    // 供 Hold 头/尾按颜色与主体对齐。
    let mut vmin = width;
    let mut vmax = 0u32;
    let mut bmin = width;
    let mut bmax = 0u32;
    for x in 0..width {
        for y in 0..height {
            let px = data.get_pixel(x, y);
            if px[3] > 8 {
                if x < vmin { vmin = x; }
                if x > vmax { vmax = x; }
                if px[2] > px[0].saturating_add(24) && px[2] > px[1] {
                    if x < bmin { bmin = x; }
                    if x > bmax { bmax = x; }
                }
                break;
            }
        }
    }
    let ratio = |lo: u32, hi: u32| if hi >= lo && lo < width { (hi - lo + 1) as f32 / width as f32 } else { 0.0 };
    let alpha_width = ratio(vmin, vmax).max(0.01);
    let blue = ratio(bmin, bmax);
    let blue_width = if blue > 0.0 { blue } else { alpha_width };
    let format = if srgb { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label), size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2, format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[],
    });
    queue.write_texture(texture.as_image_copy(), data.as_raw().as_slice(), wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * width), rows_per_image: Some(height) }, wgpu::Extent3d { width, height, depth_or_array_layers: 1 });
    LoadedTexture { view: texture.create_view(&wgpu::TextureViewDescriptor::default()), size: [width, height], alpha_width, blue_width }
}

fn solid_texture(device: &wgpu::Device, queue: &wgpu::Queue, rgba: [u8; 4]) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("solid"), size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[],
    });
    queue.write_texture(texture.as_image_copy(), &rgba, wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) }, wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// 贴图导入设置的 Wrap/Filter（`materials.md`）：全部 Point；Block=Clamp，BlockNoise1=Mirror，PointNoise=Repeat，RT=Repeat。
fn make_sampler(device: &wgpu::Device, label: &str, address: wgpu::AddressMode) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: address, address_mode_v: address, address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest, min_filter: wgpu::FilterMode::Nearest, mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

fn mask_dimensions(w: u32, h: u32) -> [u32; 2] { [(w / MASK_DOWNSCALE).max(1), (h / MASK_DOWNSCALE).max(1)] }
fn effect_dimensions(w: u32, h: u32) -> [u32; 2] { [(w / EFFECT_DOWNSCALE).max(1), (h / EFFECT_DOWNSCALE).max(1)] }
/// `sceneColorRT` 尺寸：`Screen/6`。
fn scene_dimensions(w: u32, h: u32) -> [u32; 2] { [(w / 6).max(1), (h / 6).max(1)] }

fn create_rt(device: &wgpu::Device, size: [u32; 2], label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    create_rt_fmt(device, size, label, BLOCK_FORMAT)
}

fn create_rt_fmt(device: &wgpu::Device, size: [u32; 2], label: &str, format: wgpu::TextureFormat) -> (wgpu::Texture, wgpu::TextureView) {
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label), size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2, format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING, view_formats: &[],
    });
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    (t, v)
}

/// 返回 `((msaa 纹理, msaa 视图), (resolve 纹理, resolve 视图))`。render pass 的
/// `view` 用 msaa 视图、`resolve_target` 用 resolve 视图，采样时用 resolve 视图。
fn create_rt_msaa(device: &wgpu::Device, size: [u32; 2], label: &str, format: wgpu::TextureFormat) -> ((wgpu::Texture, wgpu::TextureView), (wgpu::Texture, wgpu::TextureView)) {
    let msaa = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label), size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: MSAA_SAMPLES,
        dimension: wgpu::TextureDimension::D2, format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT, view_formats: &[],
    });
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    ((msaa, msaa_view), create_rt_fmt(device, size, label, format))
}

fn ub(binding: u32) -> wgpu::BindGroupLayoutEntry { wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::VERTEX_FRAGMENT, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None } }
fn tex(binding: u32) -> wgpu::BindGroupLayoutEntry { wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None } }
fn smp(binding: u32) -> wgpu::BindGroupLayoutEntry { wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None } }
fn ubf(binding: u32) -> wgpu::BindGroupLayoutEntry { wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None } }

fn bgl(device: &wgpu::Device, label: &str, entries: &[wgpu::BindGroupLayoutEntry]) -> wgpu::BindGroupLayout { device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries }) }

fn create_sprite_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "sprite", &[ub(0), tex(1), smp(2)]) }
fn create_compose1_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "compose1", &[ub(0), tex(1), tex(2), tex(3), smp(4), smp(5)]) }
fn create_compose2_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "compose2", &[ub(0), tex(1), tex(2), smp(3)]) }
fn create_edge_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "edge", &[ub(0), tex(1), tex(2), smp(3)]) }
fn create_glow_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "glow", &[ub(0), ubf(1), tex(2), tex(3), smp(4)]) }
fn create_glow_final_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "glow_final", &[ub(0), tex(1), smp(2)]) }
fn create_disabled_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "disabled", &[ub(0), tex(1), tex(2), tex(3), smp(4), smp(5), smp(6)]) }
fn create_active_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "active", &[ub(0), tex(1), tex(2), tex(3), tex(4), tex(5), tex(6), tex(7), smp(8), smp(9), smp(10), tex(11), smp(12)]) }
fn create_copy_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "copy", &[tex(0), smp(1)]) }
fn create_note_bgl(d: &wgpu::Device) -> wgpu::BindGroupLayout { bgl(d, "note", &[tex(0), smp(1)]) }

fn bg_sprite(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_sprite_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(s) }] })
}
fn bg_compose1(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, t1: &wgpu::TextureView, t2: &wgpu::TextureView, rt_s: &wgpu::Sampler, disp_s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_compose1_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t1) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(t2) }, wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(rt_s) }, wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(disp_s) }] })
}
fn bg_compose2(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, t1: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_compose2_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t1) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(s) }] })
}
fn bg_edge(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, t1: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_edge_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t1) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(s) }] })
}
fn bg_glow(d: &wgpu::Device, u: &wgpu::Buffer, cfg: &wgpu::Buffer, t0: &wgpu::TextureView, compose: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_glow_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: cfg.as_entire_binding() }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(compose) }, wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(s) }] })
}
fn bg_glow_final(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_glow_final_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(s) }] })
}
fn bg_disabled(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, t1: &wgpu::TextureView, t2: &wgpu::TextureView, rt_s: &wgpu::Sampler, disp_s: &wgpu::Sampler, spark_s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_disabled_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t1) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(t2) }, wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(rt_s) }, wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(disp_s) }, wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::Sampler(spark_s) }] })
}
#[allow(clippy::too_many_arguments)]
fn bg_active(d: &wgpu::Device, u: &wgpu::Buffer, t0: &wgpu::TextureView, t1: &wgpu::TextureView, t2: &wgpu::TextureView, t3: &wgpu::TextureView, t4: &wgpu::TextureView, t5: &wgpu::TextureView, t6: &wgpu::TextureView, t7: &wgpu::TextureView, rt_s: &wgpu::Sampler, disp_s: &wgpu::Sampler, spark_s: &wgpu::Sampler, effect_s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_active_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(t1) }, wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(t2) }, wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(t3) }, wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(t4) }, wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(t5) }, wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(t6) }, wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(rt_s) }, wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::Sampler(disp_s) }, wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::Sampler(spark_s) }, wgpu::BindGroupEntry { binding: 11, resource: wgpu::BindingResource::TextureView(t7) }, wgpu::BindGroupEntry { binding: 12, resource: wgpu::BindingResource::Sampler(effect_s) }] })
}
fn bg_copy(d: &wgpu::Device, t0: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &create_copy_bgl(d), entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(s) }] })
}

fn bg_note(d: &wgpu::Device, bgl: &wgpu::BindGroupLayout, t0: &wgpu::TextureView, s: &wgpu::Sampler) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("note"), layout: bgl, entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(t0) }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(s) }] })
}

fn pipeline_layout(device: &wgpu::Device, bgl: &wgpu::BindGroupLayout) -> wgpu::PipelineLayout {
    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(bgl)], immediate_size: 0 })
}

fn build_line_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat, sample_count: u32) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("line"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("line"), layout: None,
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[Some(Vertex::layout())] },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
        depth_stencil: None, multisample: wgpu::MultisampleState { count: sample_count, ..Default::default() },
        fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })] }),
        multiview_mask: None, cache: None,
    })
}

fn build_fullscreen(device: &wgpu::Device, label: &str, src: &str, entry: &str, format: wgpu::TextureFormat, bgl: &wgpu::BindGroupLayout, blend: wgpu::BlendState, vertex_blocks: bool, write_mask: wgpu::ColorWrites, sample_count: u32) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) });
    let layout = pipeline_layout(device, bgl);
    let buffers: &[Option<wgpu::VertexBufferLayout>] = if vertex_blocks { &[Some(BlockVertex::layout())] } else { &[] };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label), layout: Some(&layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
        depth_stencil: None, multisample: wgpu::MultisampleState { count: sample_count, ..Default::default() },
        fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some(entry), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format, blend: Some(blend), write_mask })] }),
        multiview_mask: None, cache: None,
    })
}

fn create_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor { label: Some("line vertices"), size: (std::mem::size_of::<Vertex>() * capacity) as wgpu::BufferAddress, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
}
fn create_block_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor { label: Some("block vertices"), size: (std::mem::size_of::<BlockVertex>() * capacity) as wgpu::BufferAddress, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
}
fn create_note_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor { label: Some("note vertices"), size: (std::mem::size_of::<NoteVertex>() * capacity) as wgpu::BufferAddress, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
}

/// 音符精灵管线：采样贴图 × 顶点色，Alpha 混合，写入场景 MSAA RT。
fn build_note_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat, sample_count: u32, bgl: &wgpu::BindGroupLayout) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("note"), source: wgpu::ShaderSource::Wgsl(NOTE_SHADER.into()) });
    let layout = pipeline_layout(device, bgl);
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("note"), layout: Some(&layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[Some(NoteVertex::layout())] },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
        depth_stencil: None, multisample: wgpu::MultisampleState { count: sample_count, ..Default::default() },
        fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })] }),
        multiview_mask: None, cache: None,
    })
}

#[cfg(test)]
mod shader_tests {
    use super::*;

    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("shader-test"), ..Default::default() })).ok()
    }

    /// 无窗口构建全部着色器与管线，捕获 WGSL 解析/类型/绑定布局错误。
    #[test]
    fn builds_all_pipelines() {
        let Some((device, _queue)) = headless_device() else {
            eprintln!("跳过：无可用 GPU adapter");
            return;
        };
        let fmt = wgpu::TextureFormat::Rgba8Unorm;
        let all = wgpu::ColorWrites::ALL;
        let _ = build_line_pipeline(&device, fmt, MSAA_SAMPLES);
        let _ = build_fullscreen(&device, "sprite", &sprite_shader(), "fs_main", BLOCK_FORMAT, &create_sprite_bgl(&device), wgpu::BlendState::REPLACE, true, all, MSAA_SAMPLES);
        let _ = build_fullscreen(&device, "compose1", &compose1_shader(), "fs_main", BLOCK_FORMAT, &create_compose1_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let _ = build_fullscreen(&device, "compose2", &compose2_shader(), "fs_main", BLOCK_FORMAT, &create_compose2_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let _ = build_fullscreen(&device, "edge", &edge_shader(), "fs_main", BLOCK_FORMAT, &create_edge_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let _ = build_fullscreen(&device, "glow", &glow_shader(), "fs_main", BLOCK_FORMAT, &create_glow_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
        let _ = build_fullscreen(&device, "glow_final", &glow_final_shader(), "fs_main", BLOCK_FORMAT, &create_glow_final_bgl(&device), wgpu::BlendState::REPLACE, false, wgpu::ColorWrites::GREEN, 1);
        let _ = build_fullscreen(&device, "subtract", &subtract_blender_shader(), "fs_main", BLOCK_FORMAT, &create_sprite_bgl(&device), wgpu::BlendState::REPLACE, true, all, MSAA_SAMPLES);
        let _ = build_fullscreen(&device, "disabled", &disabled_shader(), "fs_main", fmt, &create_disabled_bgl(&device), wgpu::BlendState::REPLACE, false, all, MSAA_SAMPLES);
        let _ = build_fullscreen(&device, "active", &active_shader(), "fs_main", fmt, &create_active_bgl(&device), wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING, false, all, 1);
        let _ = build_fullscreen(&device, "copy", COPY_SHADER, "fs_main", fmt, &create_copy_bgl(&device), wgpu::BlendState::REPLACE, false, all, 1);
    }
}
