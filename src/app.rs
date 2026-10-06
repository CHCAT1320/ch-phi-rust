use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sasa::{AudioManager, Music};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use crate::chart_fv::Chart;
use crate::render_block;
use crate::render_chart::{self, ChartRuntime};
use crate::renderer::{Fit, Renderer};

/// 窗口标题。
const WINDOW_TITLE: &str = "ch-phi-rust";

/// 窗口逻辑宽度（4:3 比例）。
const WINDOW_WIDTH: f64 = 800.0;

/// 窗口逻辑高度（4:3 比例）。
const WINDOW_HEIGHT: f64 = 600.0;

/// 渲染线程句柄：主线程通过它把窗口尺寸变化发给渲染线程，并在退出时停止它。
struct Engine {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Engine {
    /// 停止并等待渲染线程结束（会随之释放 surface）。
    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 应用主体：主线程只负责窗口与事件；实际渲染在独立线程中持续进行。
///
/// 这样即使 Windows 在拖动/缩放窗口时进入系统模态循环阻塞主线程，
/// 渲染线程仍能继续绘制与呈现，画面不会停住。
pub struct App {
    /// 应用窗口；在 `resumed` 中创建。
    window: Option<Arc<Window>>,
    /// 渲染线程句柄。
    engine: Option<Engine>,
    /// 音频管理器（用于设备异常恢复）。
    audio: Option<AudioManager>,
    /// 待交给渲染线程的谱面。
    chart: Option<Chart>,
    /// 待交给渲染线程的音乐。
    music: Option<Music>,
    /// 待交给渲染线程的谱面运行时（打击音效/特效状态）。
    runtime: Option<ChartRuntime>,
    /// 背景图路径（`IllustrationBlur.0.png`，可能不存在）。
    illustration: Option<String>,
    /// 窗口图标（内嵌 `assets/icon/icon.jpg` 解码而来）。
    icon: Option<winit::window::Icon>,
    /// 游戏画面缩放比例（`--size`，1.0 = 铺满）。
    render_scale: f32,
}

impl App {
    /// 创建应用，接收已初始化的音频、谱面、音乐与谱面运行时。
    pub fn new(
        audio: AudioManager,
        chart: Chart,
        music: Music,
        runtime: ChartRuntime,
        illustration: Option<String>,
        icon: Option<winit::window::Icon>,
        render_scale: Option<f32>,
    ) -> Self {
        Self {
            window: None,
            engine: None,
            audio: Some(audio),
            chart: Some(chart),
            music: Some(music),
            runtime: Some(runtime),
            illustration,
            icon,
            render_scale: render_scale.unwrap_or(1.0).max(0.01),
        }
    }
}

impl ApplicationHandler for App {
    /// 窗口系统就绪时创建窗口与渲染器，并启动渲染线程。
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // 已初始化则直接返回，避免重复创建
        if self.window.is_some() {
            return;
        }

        // 可任意缩放的窗口
        #[allow(unused_mut)]
        let mut attributes = Window::default_attributes()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT))
            .with_window_icon(self.icon.clone());
        // `with_window_icon` 在 Windows 上只设置小图标（标题栏）；任务栏用的是大图标，
        // 需通过平台扩展单独设置 `taskbar_icon`。
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes = attributes.with_taskbar_icon(self.icon.clone());
        }

        let window = Arc::new(event_loop.create_window(attributes).unwrap());
        // 窗口创建后再设置一次图标：部分 Windows 版本在创建时设置的任务栏大图标会被外壳忽略。
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowExtWindows;
            window.set_window_icon(self.icon.clone());
            window.set_taskbar_icon(self.icon.clone());
        }
        let renderer = Renderer::new(window.clone(), self.illustration.take());

        // 启动渲染线程
        let running = Arc::new(AtomicBool::new(true));
        let chart = self.chart.take().unwrap();
        let music = self.music.take().unwrap();
        let runtime = self.runtime.take().unwrap();
        let running_thread = Arc::clone(&running);
        let render_window = Arc::clone(&window);
        let render_scale = self.render_scale;
        let handle = thread::spawn(move || {
            render_loop(renderer, chart, music, runtime, render_window, running_thread, render_scale);
        });

        self.window = Some(window);
        self.engine = Some(Engine {
            running,
            handle: Some(handle),
        });
    }

    /// 处理窗口事件：关闭与尺寸变化（尺寸变化仅用于音频恢复）。
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            // 用户关闭窗口：先停渲染线程再退出
            WindowEvent::CloseRequested => {
                if let Some(engine) = &mut self.engine {
                    engine.stop();
                }
                event_loop.exit();
            }
            // 尺寸变化只用于音频恢复；渲染线程自行轮询窗口大小（见 `render_loop`），
            // 因为拖动/缩放窗口期间 winit 会把 `Resized` 缓冲到松开鼠标后才派发。
            WindowEvent::Resized(_) => {
                if let Some(audio) = &mut self.audio {
                    audio.recover_if_needed().ok();
                }
            }
            _ => {}
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // 确保渲染线程先结束，再释放窗口
        if let Some(engine) = &mut self.engine {
            engine.stop();
        }
    }
}

/// 渲染线程主循环：持续绘制并呈现，直到 `running` 置为 false。
fn render_loop(
    mut renderer: Renderer,
    chart: Chart,
    music: Music,
    mut     runtime: ChartRuntime,
    window: Arc<Window>,
    running: Arc<AtomicBool>,
    render_scale: f32,
) {
    // `--size` 的游戏画面缩放（之前只在导出模式生效，播放模式被丢弃）。
    renderer.set_render_scale(render_scale);
    let mut last_report = Instant::now();
    let start = Instant::now();
    let initial = renderer.window_size();
    let mut last_size = [initial[0] as u32, initial[1] as u32];

    while running.load(Ordering::Relaxed) {
        // 直接轮询窗口物理尺寸。Windows 在拖动/缩放时进入系统模态循环，winit 会把
        // `Resized` 缓冲到松手后才派发，若只依赖该事件，拖动期间判定线尺寸不变、
        // 交换链也不重配，看起来就是「窗口不刷新」。轮询可绕开这个缓冲。
        let physical = window.inner_size();
        let size = [physical.width, physical.height];
        if size[0] > 0 && size[1] > 0 && size != last_size {
            last_size = size;
            renderer.resize(size[0], size[1]);
        }

        // 清屏
        renderer.clear();

        // 视口与坐标系（原点在窗口中心，y 轴向上）
        let size = renderer.window_size();
        renderer.set_viewport(size, Fit::Contain);
        renderer.set_coordinate_system([size[0] / 2.0, size[1] / 2.0], true);
        renderer.set_vsync(false);

        // 按当前播放时间绘制块与谱面判定线
        let chart_time = music.position() - chart.offset;
        let shader_time = start.elapsed().as_secs_f32();
        render_block::render(&mut renderer, &chart.block_area_list, chart_time, shader_time);
        render_chart::render(&mut renderer, &chart, &mut runtime, music.position());

        // `--size < 1` 时用绿色矩形标出缩放后的游戏画面区域（与导出模式一致）。
        if render_scale < 1.0 {
            let sc = render_scale;
            let green = [0.30, 0.85, 0.42, 1.0];
            let th = 4.0;
            let tw = 2.0 * th / size[0];
            let thh = 2.0 * th / size[1];
            renderer.draw_rect_ndc([0.0, sc - thh / 2.0], [2.0 * sc, thh], green);
            renderer.draw_rect_ndc([0.0, -sc + thh / 2.0], [2.0 * sc, thh], green);
            renderer.draw_rect_ndc([-sc + tw / 2.0, 0.0], [tw, 2.0 * sc], green);
            renderer.draw_rect_ndc([sc - tw / 2.0, 0.0], [tw, 2.0 * sc], green);
        }

        // 渲染上屏
        renderer.render();

        // 每秒打印一次状态
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            last_report = Instant::now();
            println!("time: {:.2}s, fps: {:.1}", music.position(), renderer.fps());
        }

        // 轻微让出，避免占满 CPU
        thread::sleep(Duration::from_millis(1));
    }
}
