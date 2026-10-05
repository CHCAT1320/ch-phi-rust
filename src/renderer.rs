use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::fps::Fps;
use crate::line::{self, Line, Vertex, SHADER, VERTICES_PER_LINE};

const INITIAL_VERTEX_CAPACITY: usize = VERTICES_PER_LINE as usize;
const INITIAL_BLOCK_CAPACITY: usize = 6;

const BLOCK_MASK_PATH: &str = "assets/block/Block.png";
const BLOCK_DISPLACE_PATH: &str = "assets/block/BlockNoise1.png";
const BLOCK_SPARK_PATH: &str = "assets/block/PointNoise.png";

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
    design: [f32; 2],
    fit: Fit,
    origin: [f32; 2],
    y_up: bool,
    fps: Fps,
    vsync: bool,
    lines: Vec<Line>,
    vertices: Vec<Vertex>,
    block_vertices: Vec<BlockVertex>,
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

        let mask_tex = load_texture(&device, &queue, BLOCK_MASK_PATH, false);
        let disp_tex = load_texture(&device, &queue, BLOCK_DISPLACE_PATH, false);
        let spark_tex = load_texture(&device, &queue, BLOCK_SPARK_PATH, false);
        // 背景图（`IllustrationBlur.0.png`）：先画进 scene_full，供 active 的 `_SceneColor` 采样。
        let bg_view = match &bg_path {
            Some(p) if std::path::Path::new(p).exists() => load_texture(&device, &queue, p, false).view,
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
            block_time: 0.0, size: surface_size, design, fit: Fit::default(), origin: [0.0, 0.0], y_up: false, fps: Fps::new(), vsync,
            lines: Vec::new(), vertices: Vec::new(), block_vertices: Vec::new(),
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
    pub fn window_size(&self) -> [f32; 2] { self.size }
    pub fn set_viewport(&mut self, design: [f32; 2], fit: Fit) { self.design = design; self.fit = fit; }
    pub fn set_time(&mut self, time: f32) { self.block_time = time; }

    pub fn draw_line(&mut self, center: [f32; 2], length: f32, angle: f32, width: f32, color: [f32; 4]) {
        self.lines.push(Line { center, length, angle: angle.to_radians(), width, color: [color[0] / 255.0, color[1] / 255.0, color[2] / 255.0, color[3]] });
    }

    pub fn draw_block(&mut self, center: [f32; 2], width: f32, height: f32, angle: f32, color: [f32; 4], phase: f32) {
        let (sx, sy, ox, oy) = self.viewport();
        let size = self.size;
        let angle = if self.y_up { -angle.to_radians() } else { angle.to_radians() };
        let center = [self.origin[0] + center[0], self.origin[1] + if self.y_up { -center[1] } else { center[1] }];
        let to_ndc = |p: [f32; 2]| { let px = ox + p[0] * sx; let py = oy + p[1] * sy; [px / size[0] * 2.0 - 1.0, 1.0 - py / size[1] * 2.0] };
        let hx = width / 2.0; let hy = height / 2.0;
        let dir = [angle.cos(), angle.sin()]; let perp = [-dir[1], dir[0]];
        let corner = |dx: f32, dy: f32| to_ndc([center[0] + dir[0] * dx + perp[0] * dy, center[1] + dir[1] * dx + perp[1] * dy]);
        let a0 = corner(-hx, -hy); let a1 = corner(-hx, hy); let b1 = corner(hx, hy); let b0 = corner(hx, -hy);
        let v = |position: [f32; 2], uv: [f32; 2]| BlockVertex { position, uv, color, phase };
        for (pos, uv) in [(a0, [0.0, 1.0]), (a1, [0.0, 0.0]), (b1, [1.0, 0.0]), (a0, [0.0, 1.0]), (b1, [1.0, 0.0]), (b0, [1.0, 1.0])] {
            self.block_vertices.push(v(pos, uv));
        }
    }

    pub fn clear(&mut self) { self.lines.clear(); self.block_vertices.clear(); }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        self.config.width = width; self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.size = [width as f32, height as f32];

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
        let (win_w, win_h) = (self.size[0], self.size[1]);
        let (design_w, design_h) = (self.design[0], self.design[1]);
        match self.fit {
            Fit::Stretch => (win_w / design_w, win_h / design_h, 0.0, 0.0),
            Fit::Contain => { let s = (win_w / design_w).min(win_h / design_h); (s, s, (win_w - design_w * s) / 2.0, (win_h - design_h * s) / 2.0) }
        }
    }

    pub fn render(&mut self) { self.render_impl(); }

    fn render_impl(&mut self) {
        self.fps.tick();
        let (sx, sy, ox, oy) = self.viewport();
        let size = self.size;
        let origin = self.origin;
        let y_up = self.y_up;
        let to_ndc = |p: [f32; 2]| { let px = ox + p[0] * sx; let py = oy + p[1] * sy; [px / size[0] * 2.0 - 1.0, 1.0 - py / size[1] * 2.0] };

        self.vertices.clear();
        for line in &self.lines {
            let center = [origin[0] + line.center[0], origin[1] + if y_up { -line.center[1] } else { line.center[1] }];
            let angle = if y_up { -line.angle } else { line.angle };
            let corners = line::corners(center, line.length, angle, line.width);
            let c0 = to_ndc(corners[0]); let c1 = to_ndc(corners[1]); let c2 = to_ndc(corners[2]); let c3 = to_ndc(corners[3]);
            let (mut min_x, mut max_x) = (c0[0], c0[0]); let (mut min_y, mut max_y) = (c0[1], c0[1]);
            for p in [c1, c2, c3] { min_x = min_x.min(p[0]); max_x = max_x.max(p[0]); min_y = min_y.min(p[1]); max_y = max_y.max(p[1]); }
            if max_x < -1.0 || min_x > 1.0 || max_y < -1.0 || min_y > 1.0 { continue; }
            let color = line.color;
            for p in [c0, c1, c2, c0, c2, c3] { self.vertices.push(Vertex { position: p, color }); }
        }

        let line_count = self.vertices.len() as u32;
        if line_count > 0 { self.ensure_vertex_capacity(self.vertices.len()); self.queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertices)); }

        let block_count = self.block_vertices.len() as u32;
        if block_count > 0 {
            self.block_vertices.sort_by(|a, b| a.phase.partial_cmp(&b.phase).unwrap());
            self.ensure_block_capacity(self.block_vertices.len());
            self.queue.write_buffer(&self.block_vertex_buffer, 0, bytemuck::cast_slice(&self.block_vertices));
            let ew = (self.config.width / EFFECT_DOWNSCALE).max(1);
            let eh = (self.config.height / EFFECT_DOWNSCALE).max(1);
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
                screen: [self.config.width as f32, self.config.height as f32, 1.0 / self.config.width as f32, 1.0 / self.config.height as f32],
                blend: [0.09, 0.12, 0.0, 0.0],
                dis_spark: [0.311, 0.078, 0.078, 3.5],
                dis_params: [0.30, 2.29, 0.0, 0.0],
                st_disabled: [0.50, 0.20, 0.0, 0.0],
            };
            self.queue.write_buffer(&self.block_uniform, 0, bytemuck::bytes_of(&uniform));
        }

        self.draw_frame(block_count, line_count);
    }

    fn ensure_vertex_capacity(&mut self, needed: usize) { if needed <= self.vertex_capacity { return; } let c = needed.next_power_of_two(); self.vertex_buffer = create_vertex_buffer(&self.device, c); self.vertex_capacity = c; }
    fn ensure_block_capacity(&mut self, needed: usize) { if needed <= self.block_capacity { return; } let c = needed.next_power_of_two(); self.block_vertex_buffer = create_block_vertex_buffer(&self.device, c); self.block_capacity = c; }

    fn phase_range(&self, phase: f32) -> (u32, u32) {
        let start = self.block_vertices.partition_point(|v| v.phase < phase) as u32;
        let end = self.block_vertices.partition_point(|v| v.phase <= phase) as u32;
        (start, end)
    }

    fn draw_frame(&mut self, block_count: u32, line_count: u32) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => { self.surface.configure(&self.device, &self.config); return; }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

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
            self.fullscreen_pass(&mut encoder, &self.rt_composed_enabled.1, &self.compose1_pipeline, &self.compose1_bg);
            self.fullscreen_pass(&mut encoder, &self.rt_composed_disabled.1, &self.compose2_pipeline, &self.compose2_bg);
            // edge -> effect.R
            self.fullscreen_pass(&mut encoder, &self.rt_effect.1, &self.edge_pipeline, &self.edge_bg);
            // glow ping-pong
            let outputs = [&self.ping_a.1, &self.ping_b.1, &self.ping_a.1, &self.ping_b.1, &self.ping_a.1];
            for i in 0..self.glow_bgs.len() {
                self.fullscreen_pass(&mut encoder, outputs[i], &self.glow_pipeline, &self.glow_bgs[i]);
            }
            // 末轮只写 effect.G（保留 effect.R 的边缘），故必须 load 而非 clear
            self.fullscreen_pass_load(&mut encoder, &self.rt_effect.1, &self.glow_final_pipeline, &self.glow_final_bg);
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
            if block_count > 0 {
                pass.set_pipeline(&self.disabled_pipeline);
                pass.set_bind_group(0, &self.disabled_bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        // 2) Blit(CameraTarget -> sceneColorRT)，供 ActiveBlock 的 `_SceneColor`
        self.fullscreen_pass(&mut encoder, &self.rt_scene.1, &self.copy_pipeline, &self.copy_bg);
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
        // 4) 合成结果拷贝到交换链
        self.fullscreen_pass(&mut encoder, &view, &self.copy_pipeline, &self.copy_bg);

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

struct LoadedTexture { view: wgpu::TextureView }

fn load_texture(device: &wgpu::Device, queue: &wgpu::Queue, path: &str, srgb: bool) -> LoadedTexture {
    let dyn_image = image::open(path).unwrap_or_else(|e| panic!("加载贴图 {path} 失败: {e}"));
    let (width, height) = (dyn_image.width(), dyn_image.height());
    let data = dyn_image.to_rgba8();
    let format = if srgb { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(path), size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2, format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[],
    });
    queue.write_texture(texture.as_image_copy(), data.as_raw().as_slice(), wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * width), rows_per_image: Some(height) }, wgpu::Extent3d { width, height, depth_or_array_layers: 1 });
    LoadedTexture { view: texture.create_view(&wgpu::TextureViewDescriptor::default()) }
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
