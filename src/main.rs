//! ch-phi-rust：用 wgpu 绘制谱面判定线，并用 sasa 播放音乐。
//!
//! 模块划分：
//! - `main`：初始化音频、谱面并启动应用
//! - [`app`]：窗口管理与事件循环，并在**独立线程**中持续渲染
//! - [`renderer`]：wgpu 资源、绘制 API、坐标/视口与渲染逻辑
//! - [`render_block`]：按 `blockAreaList` 绘制块（阶段/缓动/变换）
//! - [`render_chart`]：按 `judgeLineList` 绘制判定线
//! - [`export`]：`--recorder` 视频导出（ffmpeg）
//! - [`chart_fv`]：谱面类型定义
//! - [`line`]：线段参数、顶点数据与着色器
//! - [`fps`]：帧率统计

// 发布版作为纯 GUI 程序（无控制台窗口），任务栏才会使用窗口自身的大图标。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chart_fv;
mod embedded;
mod export;
mod fps;
mod line;
mod render_block;
mod render_chart;
mod renderer;

use std::path::Path;

use sasa::backend::cpal::{CpalBackend, CpalSettings};
use sasa::{AudioClip, AudioManager, MusicParams, Sfx};
use winit::event_loop::EventLoop;

/// 默认音乐文件路径（当谱面目录内找不到 `Music.*` 时使用）。
const MUSIC_PATH: &str = "assets/charts/DesultorySignals.technoplanet.0/Music.0.wav";

/// 默认谱面文件路径（相对项目根目录）。
const CHART_PATH: &str = "assets/charts/DesultorySignals.technoplanet.0/Chart.AT.json";

/// 在谱面目录内查找 `Music.*`（任意音频格式；优先字典序第一）。
fn find_music(chart_path: &str) -> String {
    if let Some(dir) = Path::new(chart_path).parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut found: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("Music."))
                .collect();
            found.sort();
            if let Some(name) = found.into_iter().next() {
                return dir.join(name).to_string_lossy().into_owned();
            }
        }
    }
    MUSIC_PATH.to_owned()
}

/// 解析 `WxH`（支持 `x`/`X`/`*`/`×` 分隔）。
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once(['x', 'X', '*', '×'])?;
    let w: u32 = a.trim().parse().ok()?;
    let h: u32 = b.trim().parse().ok()?;
    if w >= 2 && h >= 2 { Some((w, h)) } else { None }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // `--recorder`：视频导出（要求运行目录存在 ffmpeg.exe）；后接可选 `WxH` 指定画面大小。
    // 也可用 `--size WxH` 单独指定。文件路径可用 `--chart/--music/--output/--ffmpeg` 指定。
    // 例：`ch-phi-rust --recorder 1440x1080 --chart x.json --music x.wav --output out.mp4`
    let mut recorder = false;
    let mut size: Option<(u32, u32)> = None;
    let mut scale: Option<f32> = None;
    let mut chart_arg: Option<String> = None;
    let mut music_arg: Option<String> = None;
    let mut output_arg: Option<String> = None;
    let mut ffmpeg_arg: Option<String> = None;
    let mut frames_arg: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--recorder" => {
                recorder = true;
                if let Some(s) = args.get(i + 1).and_then(|v| parse_size(v)) { size = Some(s); i += 1; }
            }
            "--size" => { if let Some(v) = args.get(i + 1).and_then(|v| v.parse::<f32>().ok()) { scale = Some(v); i += 1; } }
            "--chart" => { if let Some(v) = args.get(i + 1) { chart_arg = Some(v.clone()); i += 1; } }
            "--music" => { if let Some(v) = args.get(i + 1) { music_arg = Some(v.clone()); i += 1; } }
            "--output" | "--out" => { if let Some(v) = args.get(i + 1) { output_arg = Some(v.clone()); i += 1; } }
            "--ffmpeg" => { if let Some(v) = args.get(i + 1) { ffmpeg_arg = Some(v.clone()); i += 1; } }
            "--frames" => { if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) { frames_arg = Some(v); i += 1; } }
            _ => {
                if let Some(rest) = a.strip_prefix("--size=") { scale = rest.parse().ok(); }
                else if let Some(rest) = a.strip_prefix("--recorder=") { recorder = true; size = parse_size(rest); }
                else if let Some(rest) = a.strip_prefix("--chart=") { chart_arg = Some(rest.to_string()); }
                else if let Some(rest) = a.strip_prefix("--music=") { music_arg = Some(rest.to_string()); }
                else if let Some(rest) = a.strip_prefix("--output=") { output_arg = Some(rest.to_string()); }
                else if let Some(rest) = a.strip_prefix("--ffmpeg=") { ffmpeg_arg = Some(rest.to_string()); }
                else if let Some(rest) = a.strip_prefix("--frames=") { frames_arg = rest.parse().ok(); }
                else if !a.starts_with("--") { chart_arg = Some(a.to_string()); }
            }
        }
        i += 1;
    }
    let chart_path = chart_arg.unwrap_or_else(|| CHART_PATH.to_owned());

    // ---- 读取并解析谱面 ----
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

    let music_path = music_arg.unwrap_or_else(|| find_music(&chart_path));

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

    let illustration = Path::new(&chart_path)
        .parent()
        .map(|dir| dir.join("IllustrationBlur.0.png"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned());

    // ---- 视频导出模式 ----
    if recorder {
        export::run(chart_path, music_path, chart, illustration, icon, size, scale, output_arg, ffmpeg_arg.map(std::path::PathBuf::from), frames_arg);
        return;
    }

    // ---- 初始化音频并播放音乐 ----
    let event_loop = EventLoop::new().unwrap();
    let mut audio =
        AudioManager::new(CpalBackend::new(CpalSettings::default())).expect("初始化音频后端失败");
    let music_data = std::fs::read(&music_path).expect("读取音乐文件失败");
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

    // 窗口与渲染由 app 负责；渲染在独立线程中进行，拖动窗口时也不会停。
    let mut app = app::App::new(audio, chart, music, runtime, illustration, icon);
    event_loop.run_app(&mut app).unwrap();
}
