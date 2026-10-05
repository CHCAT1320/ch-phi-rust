use bytemuck::{Pod, Zeroable};

/// 一条有宽度的线段由一个矩形（2 个三角形、6 个顶点）组成。
pub const VERTICES_PER_LINE: u32 = 6;

/// 一条线段的绘制参数（逻辑像素 + 弧度 + RGBA）。
///
/// 由 [`crate::renderer::Renderer::draw_line`] 记录，渲染时转换为顶点。
#[derive(Clone, Copy, Debug)]
pub struct Line {
    /// 线段中点（逻辑像素）。
    pub center: [f32; 2],
    /// 线段长度（逻辑像素）。
    pub length: f32,
    /// 线段角度（弧度，内部单位；对外 API 使用度）。
    pub angle: f32,
    /// 线宽（逻辑像素）。
    pub width: f32,
    /// RGBA 颜色（各分量 0..=1，内部已归一化）。
    pub color: [f32; 4],
}

/// 线条绘制的 WGSL 着色器。
///
/// 顶点着色器把已转换到 NDC 的坐标原样输出，并把顶点颜色传给片段着色器；
/// 片段着色器直接输出该颜色。
pub const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.color = color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
"#;

/// 单个顶点：位置使用 NDC 坐标（x、y 均在 -1..1），颜色为 RGBA（各分量 0..1）。
///
/// `Pod`/`Zeroable` 让顶点数据可以安全地按字节上传到 GPU 顶点缓冲。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    /// 顶点属性布局：location 0 为 `vec2<f32>` 位置，location 1 为 `vec4<f32>` 颜色。
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    /// 输出给 wgpu 的顶点缓冲布局，说明如何从字节中解析 [`Vertex`]。
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            // 每个顶点占用的字节数
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            // 按顶点步进（而非按实例）
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// 根据中心点、长度、角度与线宽，在**设计坐标系**中生成有宽度线段的 4 个角点。
///
/// - `center`：线段中点（设计坐标）；
/// - `length`：线段总长度（设计坐标单位）；
/// - `angle`：线段方向（弧度，从 +X 轴逆时针）；
/// - `line_width`：线宽（设计坐标单位）。
///
/// 做法：沿 `angle` 方向求出两个端点，再沿垂直方向各偏移半个线宽得到 4 个角点，
/// 顺序为 `[a0, a1, b1, b0]`（对应两个三角形 `a0-a1-b1` 与 `a0-b1-b0`）。
/// 屏幕坐标的换算（原点、y 方向、视口缩放）由调用方处理。
pub fn corners(center: [f32; 2], length: f32, angle: f32, line_width: f32) -> [[f32; 2]; 4] {
    let half_len = length / 2.0;
    let half_w = line_width / 2.0;

    // 沿线段方向的单位向量
    let dir = [angle.cos(), angle.sin()];
    // 垂直方向的单位向量
    let perp = [-dir[1], dir[0]];

    // 线段两个端点（中心 ± 半个长度）
    let a = [
        center[0] - dir[0] * half_len,
        center[1] - dir[1] * half_len,
    ];
    let b = [
        center[0] + dir[0] * half_len,
        center[1] + dir[1] * half_len,
    ];

    // 四个角点：端点沿垂直方向各偏移 ±半个线宽
    [
        [a[0] - perp[0] * half_w, a[1] - perp[1] * half_w],
        [a[0] + perp[0] * half_w, a[1] + perp[1] * half_w],
        [b[0] + perp[0] * half_w, b[1] + perp[1] * half_w],
        [b[0] - perp[0] * half_w, b[1] - perp[1] * half_w],
    ]
}
