//! 视频导出（`--recorder`）：离屏渲染每帧 → 管道给 ffmpeg，并把打击音效与音乐混合。
//!
//! - ffmpeg 必须位于运行目录（`ffmpeg.exe`）。
//! - 音乐可为任意格式（交由 ffmpeg 解码）；打击音效为内嵌 wav，自行解码后按事件时间铺成一条音轨，
//!   再由 ffmpeg `amix` 与音乐混合。
//! - 导出期间窗口显示进度（标题/百分比/进度条），控制台输出详细进度。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

use crate::chart_fv::Chart;
use crate::renderer::{Fit, Renderer};
use crate::{render_block, render_chart};

/// 导出帧率。
const FPS: u32 = 60;
/// 打击音轨采样率。
const SFX_RATE: u32 = 44100;
/// 进度窗口固定物理尺寸。
const PROGRESS_W: u32 = 960;
const PROGRESS_H: u32 = 540;

/// 在运行目录/可执行文件目录查找 ffmpeg。
pub fn available_ffmpeg() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("ffmpeg.exe"), PathBuf::from("ffmpeg")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ffmpeg.exe"));
            candidates.push(dir.join("ffmpeg"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 推导输出视频路径：以谱面所在目录名命名，放在当前目录。
fn output_path(chart_path: &str) -> String {
    let name = Path::new(chart_path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    format!("{name}.mp4")
}

/// 解析 ffmpeg 输出的 `Duration: HH:MM:SS.cc`。
fn ffmpeg_duration(ffmpeg: &Path, music: &str) -> Option<f32> {
    let out = Command::new(ffmpeg).args(["-hide_banner", "-i"]).arg(music).output().ok()?;
    let s = String::from_utf8_lossy(&out.stderr);
    let idx = s.find("Duration:")?;
    let rest = &s[idx + "Duration:".len()..];
    let token = rest.trim_start().split([',', '\n']).next()?.trim();
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() < 3 { return None; }
    let h: f32 = parts[0].trim().parse().ok()?;
    let m: f32 = parts[1].parse().ok()?;
    let sec: f32 = parts[2].parse().ok()?;
    if !(h.is_finite() && m.is_finite() && sec.is_finite()) { return None; }
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// 入口：构建运行时、生成音轨、启动事件循环。
pub fn run(
    chart_path: String,
    music_path: String,
    chart: Chart,
    illustration: Option<String>,
    icon: Option<winit::window::Icon>,
    size: Option<(u32, u32)>,
    scale: Option<f32>,
    output: Option<String>,
    ffmpeg_arg: Option<PathBuf>,
    max_frames: Option<usize>,
    random_sfx: bool,
) {
    let ffmpeg = match ffmpeg_arg {
        Some(p) if p.is_file() => p,
        _ => match available_ffmpeg() {
            Some(p) => p,
            None => {
                eprintln!("未找到 ffmpeg.exe（可用 --ffmpeg 指定路径）。已取消导出。");
                return;
            }
        },
    };
    let abs_ffmpeg = ffmpeg.canonicalize().unwrap_or(ffmpeg);
    let (enc_name, enc_args) = pick_video_encoder(&abs_ffmpeg);
    println!("视频编码器：{enc_name}（优先硬件加速）");
    let music_len = ffmpeg_duration(&abs_ffmpeg, &music_path).unwrap_or(0.0);
    if music_len <= 0.0 {
        eprintln!("无法读取音乐时长：{music_path}");
        return;
    }
    println!("导出：谱面={chart_path} 音乐={music_path} 时长={music_len:.2}s FPS={FPS} 缩放={:.2}", scale.unwrap_or(1.0));

    let sfx_mode = if random_sfx { render_chart::SfxMode::Random } else { render_chart::SfxMode::Default };
    let runtime = render_chart::ChartRuntime::new_silent(&chart, sfx_mode, music_len);
    let sfx_wav = match generate_sfx_wav(&runtime.sfx_times(), music_len, sfx_mode) {
        Ok(p) => p,
        Err(e) => { eprintln!("生成打击音效轨失败：{e}"); return; }
    };
    let out = output.unwrap_or_else(|| output_path(&chart_path));
    let full_total = (music_len * FPS as f32).ceil() as usize;
    let total = max_frames.map(|m| m.clamp(1, full_total)).unwrap_or(full_total);

    let event_loop = EventLoop::new().unwrap();
    let mut rec = Recorder {
        ffmpeg: abs_ffmpeg,
        enc_args,
        music_path,
        sfx_wav,
        out: out.clone(),
        chart,
        runtime,
        illustration,
        icon,
        window: None,
        renderer: None,
        child: None,
        stdin: None,
        size: size.unwrap_or((1440, 1080)),
        scale: scale.unwrap_or(1.0),
        total: total.max(1),
        frame: 0,
        start: Instant::now(),
        last_report: Instant::now(),
        done: false,
    };
    println!("输出：{}（{} 帧）", out, rec.total);
    event_loop.run_app(&mut rec).unwrap();
}

struct Recorder {
    ffmpeg: PathBuf,
    /// 视频编码参数（硬件优先，见 [`pick_video_encoder`]）。
    enc_args: Vec<String>,
    music_path: String,
    sfx_wav: PathBuf,
    out: String,
    chart: Chart,
    runtime: render_chart::ChartRuntime,
    illustration: Option<String>,
    icon: Option<winit::window::Icon>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// 导出画面/窗口物理尺寸。
    size: (u32, u32),
    /// 游戏画面缩放比例（`--size`）。
    scale: f32,
    total: usize,
    frame: usize,
    start: Instant,
    last_report: Instant,
    done: bool,
}

impl Recorder {
    /// 渲染一帧 -> 管道给 ffmpeg -> 更新进度。
    fn step(&mut self, event_loop: &ActiveEventLoop) {
        if self.done { return; }
        let Self { chart, runtime, renderer, window, stdin, .. } = self;
        let renderer = renderer.as_mut().unwrap();
        let window = window.as_ref().unwrap();
        let t = self.frame as f32 / FPS as f32;

        renderer.clear();
        let size = renderer.render_size();
        renderer.set_viewport(size, Fit::Contain);
        renderer.set_coordinate_system([size[0] / 2.0, size[1] / 2.0], true);
        render_block::render(renderer, &chart.block_area_list, t, t);
        render_chart::render(renderer, chart, runtime, t);
        // `--size < 1` 时用绿色矩形标出「原视口」= 视频尺寸 × size（缩放后游戏画面占据的区域）。
        if self.scale < 1.0 {
            let sc = self.scale;
            let green = [0.30, 0.85, 0.42, 1.0];
            let th = 4.0;
            let tw = 2.0 * th / size[0];
            let thh = 2.0 * th / size[1];
            renderer.draw_rect_ndc([0.0, sc - thh / 2.0], [2.0 * sc, thh], green);
            renderer.draw_rect_ndc([0.0, -sc + thh / 2.0], [2.0 * sc, thh], green);
            renderer.draw_rect_ndc([-sc + tw / 2.0, 0.0], [tw, 2.0 * sc], green);
            renderer.draw_rect_ndc([sc - tw / 2.0, 0.0], [tw, 2.0 * sc], green);
        }
        renderer.render_export();
        let pixels = renderer.read_export();
        if let Some(sin) = stdin.as_mut() {
            if sin.write_all(&pixels).is_err() {
                eprintln!("ffmpeg 管道写入失败，提前结束。");
                self.finish();
                event_loop.exit();
                return;
            }
        }

        // 进度显示
        let pct = (self.frame + 1) as f32 / self.total as f32;
        let elapsed = self.start.elapsed().as_secs_f32();
        let enc_fps = if elapsed > 0.0 { (self.frame + 1) as f32 / elapsed } else { 0.0 };
        let eta = if enc_fps > 0.0 { (self.total - self.frame - 1) as f32 / enc_fps } else { 0.0 };
        let title = "ch-phi-rust 视频导出";
        let lines = vec![
            format!("{:>6}/{} 帧  {:.1}%", self.frame + 1, self.total, pct * 100.0),
            format!("{:.1} fps  已用 {:.0}s  剩余 {:.0}s", enc_fps, elapsed, eta),
            format!("{}", self.out),
        ];
        renderer.present_progress(title, &lines, pct);
        window.set_title(&format!("ch-phi-rust 导出 {:.1}%", pct * 100.0));

        if self.last_report.elapsed() >= Duration::from_millis(500) || self.frame + 1 == self.total {
            println!(
                "[导出] {:>6}/{} 帧  {:6.2}%  编码 {:.1} fps  已用 {:.1}s  剩余 {:.1}s",
                self.frame + 1, self.total, pct * 100.0, enc_fps, elapsed, eta
            );
            self.last_report = Instant::now();
        }

        self.frame += 1;
        if self.frame >= self.total {
            self.finish();
            event_loop.exit();
        } else {
            window.request_redraw();
        }
    }

    /// 关闭管道并等待 ffmpeg 收尾。
    fn finish(&mut self) {
        if self.done { return; }
        self.done = true;
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            match child.wait() {
                Ok(st) if st.success() => println!("导出完成：{}", self.out),
                Ok(st) => eprintln!("ffmpeg 退出码：{st}"),
                Err(e) => eprintln!("等待 ffmpeg 失败：{e}"),
            }
        }
        let secs = self.start.elapsed().as_secs_f32();
        let fps = if secs > 0.0 { self.frame as f32 / secs } else { 0.0 };
        println!("渲染总耗时：{secs:.2}s（{} 帧，平均 {fps:.1} fps）", self.frame);
    }
}

impl ApplicationHandler for Recorder {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (vw, vh) = self.size;
        // 进度窗口固定尺寸（与视频尺寸解耦，避免随画面比例变化）。
        let (ww, wh) = (PROGRESS_W, PROGRESS_H);
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("ch-phi-rust 导出")
            .with_inner_size(winit::dpi::PhysicalSize::new(ww, wh))
            .with_window_icon(self.icon.clone());
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_taskbar_icon(self.icon.clone());
        }
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let mut renderer = Renderer::new(window.clone(), self.illustration.take());
        renderer.set_vsync(false);
        // 窗口与视频尺寸解耦：RT 按视频尺寸渲染，窗口仅显示进度。
        renderer.set_render_size(vw, vh);
        renderer.set_render_scale(self.scale);
        let rsize = [vw as f32, vh as f32];
        renderer.set_viewport(rsize, Fit::Contain);
        renderer.set_coordinate_system([rsize[0] / 2.0, rsize[1] / 2.0], true);

        let (child, stdin) = spawn_ffmpeg(&self.ffmpeg, &self.enc_args, &self.music_path, &self.sfx_wav, &self.out, FPS, vw, vh, self.total as f32 / FPS as f32);
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.start = Instant::now();
        self.last_report = Instant::now();
        self.window = Some(window.clone());
        self.renderer = Some(renderer);
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => { self.finish(); event_loop.exit(); }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Key::Named(NamedKey::Escape) = event.logical_key { self.finish(); event_loop.exit(); }
                }
            }
            WindowEvent::RedrawRequested => self.step(event_loop),
            _ => {}
        }
    }
}

/// 选择视频编码器：优先硬件加速（NVENC → QSV → AMF），不可用则回退 libx264。
fn pick_video_encoder(ffmpeg: &Path) -> (&'static str, Vec<String>) {
    let candidates: [(&str, &[&str]); 3] = [
        ("h264_nvenc", &["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", "19", "-b:v", "0", "-pix_fmt", "yuv420p"]),
        ("h264_qsv", &["-c:v", "h264_qsv", "-global_quality", "19", "-pix_fmt", "nv12"]),
        ("h264_amf", &["-c:v", "h264_amf", "-quality", "balanced", "-qp_i", "18", "-qp_p", "18", "-pix_fmt", "yuv420p"]),
    ];
    for (name, args) in candidates {
        if encoder_works(ffmpeg, args) {
            return (name, args.iter().map(|s| s.to_string()).collect());
        }
    }
    ("libx264", vec!["-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p"].iter().map(|s| s.to_string()).collect())
}

/// 用候选参数试编码一小段，判断该编码器与参数是否真的可用（列出≠运行时可用）。
fn encoder_works(ffmpeg: &Path, enc_args: &[&str]) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=256x256:d=0.1"])
        .args(enc_args)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 启动 ffmpeg：stdin = 原始 BGRA 帧，音频 = 音乐文件 + 打击音轨，`amix` 混合后编码。
fn spawn_ffmpeg(ffmpeg: &Path, enc_args: &[String], music: &str, sfx_wav: &Path, out: &str, fps: u32, w: u32, h: u32, dur: f32) -> (Child, ChildStdin) {
    let filter = format!(
        "[0:v]scale=trunc(iw/2)*2:trunc(ih/2)*2[v];[1:a][2:a]amix=inputs=2:duration=first:normalize=0[a]"
    );
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "warning"])
        .args(["-f", "rawvideo", "-pixel_format", "bgra", "-video_size", &format!("{w}x{h}"), "-framerate", &fps.to_string(), "-i", "pipe:0"])
        .arg("-i").arg(music)
        .arg("-i").arg(sfx_wav)
        .args(["-filter_complex", &filter])
        .args(["-map", "[v]", "-map", "[a]"])
        .args(enc_args)
        .args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"])
        .args(["-t", &format!("{dur:.6}")])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("启动 ffmpeg 失败（请确认 ffmpeg.exe 可用）");
    let stdin = child.stdin.take().expect("无法获取 ffmpeg stdin");
    (child, stdin)
}

// ---- 打击音轨生成 ----

/// 一段解码后的音频（交错立体声 f32，采样率 `SFX_RATE`）。
struct Clip {
    data: Vec<f32>,
}

impl Clip {
    fn frames(&self) -> usize { self.data.len() / 2 }
}

/// 解码一段 wav（PCM/float），输出交错立体声 f32（必要时重采样到 `SFX_RATE`）。
fn decode_wav(bytes: &[u8]) -> Clip {
    let mut pos = 12usize;
    let mut fmt = (1u16, 1u16, 0u32, 16u16);
    let mut data: &[u8] = &[];
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            fmt.0 = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            fmt.1 = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            fmt.2 = u32::from_le_bytes([bytes[body + 4], bytes[body + 5], bytes[body + 6], bytes[body + 7]]);
            fmt.3 = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            let end = (body + size).min(bytes.len());
            data = &bytes[body..end];
        }
        pos = body + size + (size & 1);
    }
    let (ch, rate, bits, afmt) = (fmt.1.max(1) as usize, fmt.2.max(1), fmt.3 as usize, fmt.0);
    let bytes_per = (bits / 8).max(1);
    let stride = ch * bytes_per;
    let n = if stride > 0 { data.len() / stride } else { 0 };
    let mut src: Vec<f32> = Vec::with_capacity(n * ch);
    for i in 0..n {
        for c in 0..ch {
            let o = i * stride + c * bytes_per;
            let s = match (afmt, bits) {
                (3, 32) => f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
                (1, 8) => (data[o] as f32 - 128.0) / 128.0,
                (1, 16) => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
                (1, 24) => {
                    let v = ((data[o + 2] as i32) << 16 | (data[o + 1] as i32) << 8 | data[o] as i32) << 8;
                    (v >> 8) as f32 / 8_388_608.0
                }
                (1, 32) => i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as f32 / 2_147_483_648.0,
                _ => 0.0,
            };
            src.push(s);
        }
    }
    // 统一为立体声
    let mut stereo: Vec<f32> = Vec::with_capacity(n * 2);
    for i in 0..n {
        let l = src[i * ch];
        let r = if ch >= 2 { src[i * ch + 1] } else { l };
        stereo.push(l);
        stereo.push(r);
    }
    resample_to_sfx(stereo, rate, n)
}

/// 把交错立体声从 `rate` 重采样到 [`SFX_RATE`]。
fn resample_to_sfx(stereo: Vec<f32>, rate: u32, n: usize) -> Clip {
    if rate == SFX_RATE || n == 0 {
        return Clip { data: stereo };
    }
    let out_frames = ((n as f64) * SFX_RATE as f64 / rate as f64).round() as usize;
    let mut out = vec![0f32; out_frames * 2];
    for i in 0..out_frames {
        let src_pos = i as f64 * rate as f64 / SFX_RATE as f64;
        let i0 = src_pos.floor() as usize;
        let i1 = (i0 + 1).min(n - 1);
        let f = (src_pos - i0 as f64) as f32;
        for c in 0..2 {
            let a = stereo[i0.min(n - 1) * 2 + c];
            let b = stereo[i1 * 2 + c];
            out[i * 2 + c] = a + (b - a) * f;
        }
    }
    Clip { data: out }
}

/// 解码一段内嵌音频（wav 自解；mp3/aac 等交给 `sasa` 的 symphonia 解码），
/// 输出交错立体声 f32 并重采样到 [`SFX_RATE`]。
fn decode_audio(key: &str) -> Clip {
    let bytes = crate::embedded::expect(key);
    if key.ends_with(".wav") {
        return decode_wav(bytes);
    }
    let (frames, rate) = sasa::AudioClip::decode(bytes.to_vec())
        .unwrap_or_else(|e| panic!("解码打击音效 {key} 失败: {e}"));
    let n = frames.len();
    let mut stereo = Vec::with_capacity(n * 2);
    for f in &frames {
        stereo.push(f.0);
        stereo.push(f.1);
    }
    resample_to_sfx(stereo, rate, n)
}

/// 把打击音效按事件时间铺成一条与音乐等长的立体声轨，写入临时 f32 wav。
fn generate_sfx_wav(events: &[(f32, usize)], music_len: f32, mode: render_chart::SfxMode) -> std::io::Result<PathBuf> {
    let clips: Vec<Clip> = match mode {
        render_chart::SfxMode::Default => vec![
            decode_wav(crate::embedded::expect("audio/tap.wav")),
            decode_wav(crate::embedded::expect("audio/drag.wav")),
            decode_wav(crate::embedded::expect("audio/flick.wav")),
        ],
        render_chart::SfxMode::Random => {
            let keys = render_chart::random_sfx_keys();
            println!("随机打击音效：{} 个音频（audio/idk/）", keys.len());
            keys.iter().map(|k| decode_audio(k)).collect()
        }
    };
    let total_frames = ((music_len * SFX_RATE as f32).ceil() as usize) + SFX_RATE as usize; // 多留 1s
    let mut buf = vec![0f32; total_frames * 2];
    let mut placed = 0usize;
    if !clips.is_empty() {
        for &(t, kind) in events {
            let clip = &clips[kind.min(clips.len() - 1)];
            let start = (t.max(0.0) * SFX_RATE as f32).round() as usize;
            for i in 0..clip.frames() {
                let d = (start + i) * 2;
                if d + 1 >= buf.len() { break; }
                buf[d] += clip.data[i * 2];
                buf[d + 1] += clip.data[i * 2 + 1];
            }
            placed += 1;
        }
    }
    println!("打击音效：{} 个事件，已混入音轨。", placed);
    let path = std::env::temp_dir().join("ch_phi_sfx_track.wav");
    write_wav_f32(&path, &buf, 2, SFX_RATE)?;
    Ok(path)
}

/// 写 32-bit float WAV。
fn write_wav_f32(path: &Path, samples: &[f32], channels: u16, rate: u32) -> std::io::Result<()> {
    let bits = 32u16;
    let block_align = channels * bits / 8;
    let byte_rate = rate * block_align as u32;
    let data_len = (samples.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 4);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples { out.extend_from_slice(&s.to_le_bytes()); }
    std::fs::write(path, out)
}
