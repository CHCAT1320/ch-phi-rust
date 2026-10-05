//! ch-phi-rust：用 wgpu 绘制谱面判定线，并用 sasa 播放音乐。
//!
//! 模块划分：
//! - `main`：初始化音频、谱面并启动应用
//! - [`app`]：窗口管理与事件循环，并在**独立线程**中持续渲染
//! - [`renderer`]：wgpu 资源、绘制 API、坐标/视口与渲染逻辑
//! - [`render_block`]：按 `blockAreaList` 绘制块（阶段/缓动/变换）
//! - [`render_chart`]：按 `judgeLineList` 绘制判定线
//! - [`chart_fv`]：谱面类型定义
//! - [`line`]：线段参数、顶点数据与着色器
//! - [`fps`]：帧率统计

// 发布版作为纯 GUI 程序（无控制台窗口），任务栏才会使用窗口自身的大图标。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chart_fv;
mod embedded;
mod fps;
mod line;
mod render_block;
mod render_chart;
mod renderer;

use sasa::backend::cpal::{CpalBackend, CpalSettings};
use sasa::{AudioClip, AudioManager, MusicParams, Sfx};
use winit::event_loop::EventLoop;

/// 音乐文件路径（相对项目根目录）。
const MUSIC_PATH: &str = "assets/charts/DesultorySignals.technoplanet.0/Music.0.wav";

/// 谱面文件路径（相对项目根目录）。
const CHART_PATH: &str = "assets/charts/DesultorySignals.technoplanet.0/Chart.AT.json";

fn main() {
    let event_loop = EventLoop::new().unwrap();

    // ---- 读取并解析谱面 ----
    // 可用命令行参数指定谱面路径，默认使用内置的谱面
    let chart_path = std::env::args().nth(1).unwrap_or_else(|| CHART_PATH.to_owned());
    let chart_data = std::fs::read_to_string(&chart_path).expect("读取谱面文件失败");
    let mut chart: chart_fv::Chart = serde_json::from_str(&chart_data).expect("解析谱面失败");
    // 加载音符：按各判定线速度事件重算所有音符的 floorPosition
    render_chart::load_notes(&mut chart);
    let note_count: usize = chart
        .judge_line_list
        .iter()
        .map(|line| line.notes_above.len() + line.notes_below.len())
        .sum();
    println!(
        "谱面: {} | formatVersion={}, offset={}, 判定线={}, 音符={}, 遮罩区域={}",
        chart_path,
        chart.format_version,
        chart.offset,
        chart.judge_line_list.len(),
        note_count,
        chart.block_area_list.len(),
    );

    // ---- 初始化音频并播放音乐 ----
    let mut audio =
        AudioManager::new(CpalBackend::new(CpalSettings::default())).expect("初始化音频后端失败");
    let music_data = std::fs::read(MUSIC_PATH).expect("读取音乐文件失败");
    let clip = AudioClip::new(music_data).expect("解码音乐失败");
    let music_len = clip.length();
    let mut music = audio
        .create_music(clip, MusicParams::default())
        .expect("创建音乐播放器失败");
    music.play().expect("播放音乐失败");

    // ---- 打击音效（内嵌资源）----
    fn load_sfx(audio: &mut AudioManager, key: &str) -> Sfx {
        let data = crate::embedded::expect(key).to_vec();
        let clip = AudioClip::new(data).unwrap_or_else(|e| panic!("解码打击音效 {key} 失败: {e}"));
        audio.create_sfx(clip, None).expect("创建打击音效失败")
    }
    let sfx_tap = load_sfx(&mut audio, "audio/tap.wav");
    let sfx_drag = load_sfx(&mut audio, "audio/drag.wav");
    let sfx_flick = load_sfx(&mut audio, "audio/flick.wav");
    let runtime = render_chart::ChartRuntime::new(&chart, sfx_tap, sfx_drag, sfx_flick, music_len);

    // ---- 窗口图标（内嵌 `assets/icon/icon.jpg`）----
    // 缩放到 256×256：Windows 任务栏/标题栏对超大尺寸图标支持不佳，会退回默认图标。
    let icon = {
        let bytes = crate::embedded::expect("icon/icon.jpg");
        let decoded = image::load_from_memory(bytes).expect("解码窗口图标失败");
        let rgba = decoded
            .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        winit::window::Icon::from_rgba(rgba.into_raw(), 256, 256).ok()
    };

    // 窗口与渲染由 app 负责；渲染在独立线程中进行，拖动窗口时也不会停。
    let illustration = std::path::Path::new(&chart_path)
        .parent()
        .map(|dir| dir.join("IllustrationBlur.0.png"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned());
    let mut app = app::App::new(audio, chart, music, runtime, illustration, icon);
    event_loop.run_app(&mut app).unwrap();
}
