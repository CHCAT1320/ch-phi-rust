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
const CHART_PATH: &str = "assets/charts/DesultorySignals.technoplanet.0/Chart.EZ.json";

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

/// 规范化用户传入的路径：绝对路径原样返回，相对路径按当前工作目录补全。
fn resolve_path(p: &str) -> String {
    let path = Path::new(p);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    resolved.to_string_lossy().into_owned()
}

/// 命令行用法帮助。
const USAGE: &str = "\
ch-phi-rust —— 用 wgpu 绘制谱面判定线并播放音乐

用法:
    ch-phi-rust [谱面路径] [选项]

选项:
    --recorder [WxH]        视频导出（需要 ffmpeg.exe）；可选导出尺寸，默认 1440x1080
    --random-sfx, --idk     随机打击音效：每次命中从 assets/audio/idk/ 随机取一个
    --size <比例>           游戏画面缩放比例（> 0，1.0 表示铺满），播放与导出均生效
    --chart <路径>          谱面 JSON 文件（等价于直接给出位置参数）
    --music <路径>          音乐文件（默认自动查找谱面目录下的 Music.*）
    --output, --out <路径>  导出视频的输出路径（仅 --recorder 有效）
    --ffmpeg <路径>         ffmpeg 可执行文件路径
    --frames <数量>         最多导出的帧数（仅 --recorder 有效）
    -h, --help              显示本帮助

路径:
    --chart/--music/--output/--ffmpeg 均可使用绝对路径或相对路径（相对当前工作目录）。

示例:
    ch-phi-rust --recorder 1440x1080 --chart x.json --music x.wav --output out.mp4
";

/// 已解析并通过合法性检查的命令行选项。
#[derive(Default)]
struct Options {
    /// 是否导出视频（`--recorder`）。
    recorder: bool,
    /// 导出视频尺寸（`--recorder [WxH]`）。
    size: Option<(u32, u32)>,
    /// 游戏画面缩放比例（`--size`）。
    scale: Option<f32>,
    /// 谱面路径（位置参数或 `--chart`）。
    chart: Option<String>,
    /// 音乐路径（`--music`）。
    music: Option<String>,
    /// 导出输出路径（`--output`/`--out`）。
    output: Option<String>,
    /// ffmpeg 路径（`--ffmpeg`）。
    ffmpeg: Option<String>,
    /// 最大导出帧数（`--frames`）。
    frames: Option<usize>,
    /// 随机打击音效（`--random-sfx`/`--idk`）。
    random_sfx: bool,
}

/// 取出选项取值：优先 `--flag=value`，否则消费后一个参数。
fn take_value(
    args: &[String],
    i: &mut usize,
    inline: Option<String>,
    flag: &str,
) -> Result<String, String> {
    if let Some(v) = inline {
        if v.is_empty() {
            return Err(format!("选项 {flag} 缺少取值"));
        }
        return Ok(v);
    }
    match args.get(*i + 1) {
        Some(v) if !v.starts_with("--") => {
            *i += 1;
            Ok(v.clone())
        }
        Some(v) => Err(format!("选项 {flag} 缺少取值（其后为 {v}）")),
        None => Err(format!("选项 {flag} 缺少取值")),
    }
}

/// 判断一个词是否「看起来像尺寸」，用于识别 `--recorder` 后误写的尺寸。
fn looks_like_size(s: &str) -> bool {
    s.chars().any(|c| matches!(c, 'x' | 'X' | '*' | '×'))
        || (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
}

/// 解析并校验命令行参数；`--help` 会打印帮助后退出。
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut positional: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        // 拆分 `--flag=value` 形式。
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a, None),
        };
        match flag {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--recorder" => {
                o.recorder = true;
                if let Some(v) = inline {
                    o.size = Some(parse_size(&v).ok_or_else(|| {
                        format!("无效的导出尺寸：{v}（应为 WxH，如 1440x1080）")
                    })?);
                } else if let Some(next) = args.get(i + 1) {
                    // 仅当后一个参数确实能解析为尺寸时才消费，兼容「后跟谱面路径」的写法。
                    if !next.starts_with('-') {
                        if let Some(s) = parse_size(next) {
                            o.size = Some(s);
                            i += 1;
                        } else if looks_like_size(next) {
                            return Err(format!("无效的导出尺寸：{next}（应为 WxH，如 1440x1080）"));
                        }
                    }
                }
            }
            "--size" => {
                let v = take_value(args, &mut i, inline, "--size")?;
                let n: f32 = v
                    .parse()
                    .map_err(|_| format!("无效的缩放比例：{v}（应为 > 0 的数值）"))?;
                if !n.is_finite() || n <= 0.0 {
                    return Err(format!("无效的缩放比例：{v}（应为 > 0 的数值）"));
                }
                o.scale = Some(n);
            }
            "--random-sfx" | "--idk" => o.random_sfx = true,
            "--chart" => o.chart = Some(take_value(args, &mut i, inline, "--chart")?),
            "--music" => o.music = Some(take_value(args, &mut i, inline, "--music")?),
            "--output" | "--out" => o.output = Some(take_value(args, &mut i, inline, "--output")?),
            "--ffmpeg" => o.ffmpeg = Some(take_value(args, &mut i, inline, "--ffmpeg")?),
            "--frames" => {
                let v = take_value(args, &mut i, inline, "--frames")?;
                let n: usize = v
                    .parse()
                    .map_err(|_| format!("无效的帧数：{v}（应为正整数）"))?;
                if n == 0 {
                    return Err("无效的帧数：0（应为正整数）".to_string());
                }
                o.frames = Some(n);
            }
            "--" => {
                // 之后所有参数都视为位置参数。
                for extra in &args[i + 1..] {
                    if positional.is_some() {
                        return Err(format!("多余的参数：{extra}（只能指定一个谱面路径）"));
                    }
                    positional = Some(extra.clone());
                }
                break;
            }
            _ if a.starts_with('-') => {
                return Err(format!("未知选项：{a}（运行 ch-phi-rust --help 查看用法）"));
            }
            _ => {
                if positional.is_some() {
                    return Err(format!("多余的参数：{a}（只能指定一个谱面路径）"));
                }
                positional = Some(a.to_string());
            }
        }
        i += 1;
    }
    if o.chart.is_some() && positional.is_some() {
        return Err("同时用位置参数和 --chart 指定了谱面，请只保留一个".to_string());
    }
    if o.chart.is_none() {
        o.chart = positional;
    }
    // 仅与视频导出有关的选项在非导出模式下提示并忽略，避免被静默丢弃。
    if !o.recorder {
        if o.output.is_some() {
            eprintln!("提示：--output/--out 仅在 --recorder 模式下生效，已忽略。");
            o.output = None;
        }
        if o.frames.is_some() {
            eprintln!("提示：--frames 仅在 --recorder 模式下生效，已忽略。");
            o.frames = None;
        }
        if o.ffmpeg.is_some() {
            eprintln!("提示：--ffmpeg 仅在 --recorder 模式下生效，已忽略。");
            o.ffmpeg = None;
        }
    }
    Ok(o)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = parse_args(&args).unwrap_or_else(|e| {
        eprintln!("参数错误：{e}");
        eprintln!("运行 ch-phi-rust --help 查看用法。");
        std::process::exit(2);
    });

    // 路径参数同时支持绝对路径与相对路径；相对路径按当前工作目录补全。
    let chart_path = resolve_path(&opts.chart.take().unwrap_or_else(|| CHART_PATH.to_owned()));
    let ffmpeg_arg = opts.ffmpeg.take().map(|p| resolve_path(&p));
    let output_arg = opts.output.take().map(|p| resolve_path(&p));

    // 提前校验文件/目录是否存在，给出可读提示而不是读取时 panic。
    if !Path::new(&chart_path).is_file() {
        eprintln!("找不到谱面文件：{chart_path}");
        std::process::exit(1);
    }
    if let Some(f) = &ffmpeg_arg {
        if !Path::new(f).is_file() {
            eprintln!("--ffmpeg 指定的文件不存在：{f}");
            std::process::exit(1);
        }
    }
    if let Some(o) = &output_arg {
        if let Some(dir) = Path::new(o).parent() {
            if !dir.as_os_str().is_empty() && !dir.is_dir() {
                eprintln!("输出目录不存在：{}", dir.display());
                std::process::exit(1);
            }
        }
    }

    // ---- 读取并解析谱面 ----
    let chart_data = std::fs::read_to_string(&chart_path).unwrap_or_else(|e| {
        eprintln!("读取谱面文件失败：{chart_path}（{e}）");
        std::process::exit(1);
    });
    let mut chart: chart_fv::Chart = serde_json::from_str(&chart_data).unwrap_or_else(|e| {
        eprintln!("解析谱面失败：{chart_path}（{e}）");
        std::process::exit(1);
    });
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

    let music_path = resolve_path(&opts.music.take().unwrap_or_else(|| find_music(&chart_path)));
    if !Path::new(&music_path).is_file() {
        eprintln!("找不到音乐文件：{music_path}（可用 --music 指定）");
        std::process::exit(1);
    }

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
    if opts.recorder {
        export::run(chart_path, music_path, chart, illustration, icon, opts.size, opts.scale, output_arg, ffmpeg_arg.map(std::path::PathBuf::from), opts.frames, opts.random_sfx);
        return;
    }

    // ---- 初始化音频并播放音乐 ----
    let event_loop = EventLoop::new().unwrap();
    let mut audio =
        AudioManager::new(CpalBackend::new(CpalSettings::default())).expect("初始化音频后端失败");
    let music_data = std::fs::read(&music_path).unwrap_or_else(|e| {
        eprintln!("读取音乐文件失败：{music_path}（{e}）");
        std::process::exit(1);
    });
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
    let default_sfx = |audio: &mut AudioManager| {
        render_chart::SfxSet::Default([
            load_sfx(audio, "audio/tap.wav"),
            load_sfx(audio, "audio/drag.wav"),
            load_sfx(audio, "audio/flick.wav"),
        ])
    };
    let sfx = if opts.random_sfx {
        let keys = render_chart::random_sfx_keys();
        if keys.is_empty() {
            eprintln!("随机打击音效：assets/audio/idk/ 下没有可用音频，回退默认音效。");
            default_sfx(&mut audio)
        } else {
            println!("随机打击音效：{} 个音频（audio/idk/）", keys.len());
            render_chart::SfxSet::Random(keys.iter().map(|k| load_sfx(&mut audio, k)).collect())
        }
    } else {
        default_sfx(&mut audio)
    };
    let runtime = render_chart::ChartRuntime::new(&chart, sfx, music_len);

    // 窗口与渲染由 app 负责；渲染在独立线程中进行，拖动窗口时也不会停。
    let mut app = app::App::new(audio, chart, music, runtime, illustration, icon, opts.scale);
    event_loop.run_app(&mut app).unwrap();
}
