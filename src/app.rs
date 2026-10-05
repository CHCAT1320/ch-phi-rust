use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
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
use crate::render_chart;
use crate::renderer::{Fit, Renderer};

/// 窗口标题。
const WINDOW_TITLE: &str = "ch-phi-rust";

/// 窗口逻辑宽度（4:3 比例）。
const WINDOW_WIDTH: f64 = 800.0;

/// 窗口逻辑高度（4:3 比例）。
const WINDOW_HEIGHT: f64 = 600.0;

/// 渲染线程句柄：主线程通过它把窗口尺寸变化发给渲染线程，并在退出时停止它。
struct Engine {
    resize_tx: SyncSender<[u32; 2]>,
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
    /// 背景图路径（`IllustrationBlur.0.png`，可能不存在）。
    illustration: Option<String>,
}

impl App {
    /// 创建应用，接收已初始化的音频、谱面与音乐。
    pub fn new(audio: AudioManager, chart: Chart, music: Music, illustration: Option<String>) -> Self {
        Self {
            window: None,
            engine: None,
            audio: Some(audio),
            chart: Some(chart),
            music: Some(music),
            illustration,
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
        let attributes = Window::default_attributes()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));

        let window = Arc::new(event_loop.create_window(attributes).unwrap());
        let renderer = Renderer::new(window.clone(), self.illustration.take());

        // 启动渲染线程
        let (resize_tx, resize_rx) = mpsc::sync_channel(64);
        let running = Arc::new(AtomicBool::new(true));
        let chart = self.chart.take().unwrap();
        let music = self.music.take().unwrap();
        let running_thread = Arc::clone(&running);
        let handle = thread::spawn(move || {
            render_loop(renderer, chart, music, resize_rx, running_thread);
        });

        self.window = Some(window);
        self.engine = Some(Engine {
            resize_tx,
            running,
            handle: Some(handle),
        });
    }

    /// 处理窗口事件：关闭、尺寸/缩放变化。
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
            // 尺寸变化：通知渲染线程
            WindowEvent::Resized(size) => {
                if let Some(engine) = &self.engine {
                    let _ = engine.resize_tx.try_send([size.width, size.height]);
                }
                if let Some(audio) = &mut self.audio {
                    audio.recover_if_needed().ok();
                }
            }
            // 缩放因子变化：按新的物理尺寸通知渲染线程
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if let Some(engine) = &self.engine {
                        let _ = engine.resize_tx.try_send([size.width, size.height]);
                    }
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
    resize_rx: Receiver<[u32; 2]>,
    running: Arc<AtomicBool>,
) {
    let mut last_report = Instant::now();
    let start = Instant::now();

    while running.load(Ordering::Relaxed) {
        // 应用渲染线程收到的最新窗口尺寸
        while let Ok([w, h]) = resize_rx.try_recv() {
            renderer.resize(w, h);
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
        render_chart::render(&mut renderer, &chart, music.position());

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
