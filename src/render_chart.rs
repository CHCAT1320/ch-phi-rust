//! 谱面渲染：按时间绘制判定线（移动 / 旋转 / 消失）。
//!
//! 坐标与时间语义参考 <https://docs.lchzh.net/learning/phigros/>：
//! - 移动事件位置为 `formatVersion = 3` 的归一化坐标（左下角为原点，右上角为 (1, 1)）；
//! - 旋转事件值为逆时针角度（单位：度）；
//! - 时间单位为 T，1 T = 1.875 / BPM 秒。

use std::collections::HashMap;

use sasa::{PlaySfxParams, Sfx};

use crate::chart_fv::{Chart, DisappearEvent, JudgeLine, MoveEvent, Note, RotateEvent, SpeedEvent};
use crate::renderer::{NoteTexture, Renderer};

/// 判定线默认归一化位置（画面中心）。
const DEFAULT_POS: [f32; 2] = [0.5, 0.5];

/// 判定线线宽（像素）。
const LINE_WIDTH: f32 = 5.0;

/// 秒 -> T 的换算系数（每 BPM）：1 T = 1.875 / BPM 秒，故 1 秒 = BPM / 1.875 T。
const T_PER_SECOND: f32 = 1.0 / 1.875;

/// 音符贴图宽度（占屏幕宽度比例）。参考实现为 `100 / 720`，其余尺寸由贴图宽高比推导。
const NOTE_WIDTH: f32 = 100.0 / 640.0;

/// `Note.position_x` 的横向单位（占屏幕宽度比例）。
const NOTE_X_UNIT: f32 = 0.05625;

/// 音符纵向距离单位（占屏幕高度比例）：非 Hold `Y = η·(pN − PJ(t))·0.6H`，`η = note.speed`。
const NOTE_Y_UNIT: f32 = 0.6;

/// 打击特效显示时长（秒）。
const HIT_DURATION: f32 = 0.5;

/// 打击特效尺寸（相对 `note_width`；参考实现为 `(256 / 1.7) / 100`）。
const HIT_SIZE: f32 = (256.0 / 1.7) / 100.0;

/// 打击特效金色小方块（sparks）的飞散距离与边长（相对 `note_width`）。
const SPARK_OFFSET: f32 = 120.0 / 100.0;
const SPARK_SIZE: f32 = 15.0 / 100.0;

/// 音符「实时垂直距离」上限：`speed·currentFloorPosition` 超过它即不可见（2H，docs 实测）。
const FAR_LIMIT: f32 = 3.3333336;

/// 打击音效在 [`ChartRuntime::sfx`] 中的下标。
const SFX_TAP: usize = 0;
const SFX_DRAG: usize = 1;
const SFX_FLICK: usize = 2;

/// 判定线事件共有的时间字段（单位 T）。
trait TimeEvent {
    fn start_time(&self) -> f32;
    fn end_time(&self) -> f32;
}

impl TimeEvent for MoveEvent {
    fn start_time(&self) -> f32 {
        self.start_time
    }
    fn end_time(&self) -> f32 {
        self.end_time
    }
}

impl TimeEvent for RotateEvent {
    fn start_time(&self) -> f32 {
        self.start_time
    }
    fn end_time(&self) -> f32 {
        self.end_time
    }
}

impl TimeEvent for DisappearEvent {
    fn start_time(&self) -> f32 {
        self.start_time
    }
    fn end_time(&self) -> f32 {
        self.end_time
    }
}

/// 按当前音乐时间（秒）绘制判定线、音符与打击特效。
///
/// `time` 为音乐播放时间（秒），内部用 `chart.offset` 校正为谱面时间。
pub fn render(renderer: &mut Renderer, chart: &Chart, runtime: &mut ChartRuntime, time: f32) {
    let size = renderer.render_size();
    let (w, h) = (size[0], size[1]);
    let chart_time = time - chart.offset;
    // 判定线长度 = 窗口宽度的 3 倍（与参考实现 `game.html` 的 `3×LW` 一致）。
    let line_len = w * 3.0;
    let note_width = NOTE_WIDTH * w;

    // 到时的音符：播放打击音效并生成特效；随后绘制仍活动的特效
    runtime.update(chart, chart_time, w, h);
    runtime.draw_hits(renderer, chart_time, note_width);
    // HUD：分数/连击/暂停/进度/水印
    runtime.draw_hud(renderer, chart_time);

    // 同一时刻存在多个音符时使用 HL 高亮贴图
    let hl_times = highlight_times(chart);

    for line in &chart.judge_line_list {
        // 谱面时间（秒）换算到该判定线的时间单位 T（每线只算一次）
        let time_t = chart_time * line.bpm * T_PER_SECOND;
        let (center, angle) = line_pose(chart, line, time_t, w, h);

        // 透明度
        let alpha = match locate(&line.judge_line_disappear_events, time_t) {
            Some((e, f)) => lerp(e.start, e.end, f),
            None => 1.0,
        }
        .clamp(0.0, 1.0);

        // 判定线本身按透明度绘制；音符不受判定线透明度影响（与原版一致）。
        // 颜色 `#feffa9`（AP）。
        if alpha > 0.0 {
            renderer.draw_line(
                center,
                line_len,
                angle,
                LINE_WIDTH,
                [254.0, 255.0, 169.0, alpha],
            );
        }

        // 音符：平移到判定线中心、旋转到判定线角度后，按线局部坐标绘制。
        if !line.notes_above.is_empty() || !line.notes_below.is_empty() {
            let fp = LineFloorPosition::new(&line.speed_events, line.bpm);
            let line_fp = fp.fp(time_t);
            renderer.save();
            renderer.translate(center[0], center[1]);
            renderer.rotate(angle);
            for (side, notes) in [(1.0f32, &line.notes_above), (-1.0f32, &line.notes_below)] {
                for note in notes {
                    let highlight = hl_times.contains(&note.time.to_bits());
                    if note.note_type == 3 {
                        draw_hold(renderer, note, side, time_t, line_fp, w, h, note_width, highlight, line.bpm);
                    } else {
                        draw_note(renderer, note, side, time_t, line_fp, w, h, note_width, highlight);
                    }
                }
            }
            renderer.restore();
        }
    }
}

/// 判定线在给定时刻的世界位姿：中心（用户坐标，像素）与逆时针角度（度）。
fn line_pose(chart: &Chart, line: &JudgeLine, time_t: f32, w: f32, h: f32) -> ([f32; 2], f32) {
    let pos = match locate(&line.judge_line_move_events, time_t) {
        Some((e, f)) => unpack_move(chart.format_version, e, f),
        None => {
            if chart.format_version == 1 || chart.format_version == 3 {
                DEFAULT_POS
            } else {
                [0.0, 0.0]
            }
        }
    };
    let angle = match locate(&line.judge_line_rotate_events, time_t) {
        Some((e, f)) => lerp(e.start, e.end, f),
        None => 0.0,
    };
    let center = if chart.format_version == 1 || chart.format_version == 3 {
        [(pos[0] - 0.5) * w, (pos[1] - 0.5) * h]
    } else {
        [pos[0] * 0.1 * h, pos[1] * 0.1 * h]
    };
    (center, angle)
}

/// 一个待触发的非 Hold 打击事件（按谱面绝对时间升序）。
#[derive(Clone, Copy)]
struct HitEvent {
    /// 谱面时间（秒）。
    time: f32,
    /// 所属判定线下标。
    line: usize,
    /// 音符的 `position_x`。
    x: f32,
    /// 音符横向镜像系数（判定线下方为 -1，其余为 1），与音符绘制保持一致。
    mirror: f32,
    /// 音符类型（1 tap / 2 drag / 4 flick）。
    note_type: i32,
}

/// 一个 Hold 音符的打击事件：头在 `time`，尾在 `end`，期间每 `interval` 秒生成一次打击特效。
#[derive(Clone, Copy)]
struct HoldEvent {
    /// 头部判定时间（秒）。
    time: f32,
    /// 结束时间（秒）。
    end: f32,
    /// 所属判定线下标。
    line: usize,
    /// 音符的 `position_x`。
    x: f32,
    /// 音符横向镜像系数（判定线下方为 -1，上方为 1），与音符绘制保持一致。
    mirror: f32,
    /// 连续的打击特效生成间隔（秒）：CHCAT 为 `30 / bpm`。
    interval: f32,
}

/// 正在持续的 Hold：记录下一次生成打击特效的时间。
#[derive(Clone, Copy)]
struct ActiveHold {
    event: HoldEvent,
    /// 下一次生成打击特效的谱面时间（秒）。
    next: f32,
    /// 是否已计入连击/分数（Hold 在接近结束时计一次）。
    counted: bool,
}

/// 一个正在播放的打击特效。
#[derive(Clone, Copy)]
struct ActiveHit {
    /// 特效中心（用户坐标，像素）。
    center: [f32; 2],
    /// 生成时刻（谱面秒）。
    spawn: f32,
    /// 4 个金色小方块的随机初始方向（弧度）。
    block_r: [f32; 4],
}

/// 谱面运行时状态：打击音效、到时的打击事件与活动的打击特效。
pub struct ChartRuntime {
    /// 非 Hold 打击事件（按时间升序）。
    events: Vec<HitEvent>,
    cursor: usize,
    /// Hold 头部事件（按时间升序）。
    holds: Vec<HoldEvent>,
    hold_cursor: usize,
    /// 当前正在持续的 Hold（周期性生成打击特效）。
    active_holds: Vec<ActiveHold>,
    hits: Vec<ActiveHit>,
    /// 打击音效：`[tap, drag, flick]`；导出模式（`--recorder`）为 `None`（不实时播放）。
    sfx: Option<[Sfx; 3]>,
    /// 简易 LCG，用于生成小方块的随机方向。
    rng: u32,
    /// 总音符数（用于计分）。
    note_count: usize,
    /// 连击数。
    combo: u32,
    /// 当前分数（满分 1000000）。用 f64 累加，避免 2026 次相加的 f32 精度误差。
    score: f64,
    /// 音乐总时长（秒），用于进度条。
    music_len: f32,
}

impl ChartRuntime {
    /// 构建运行时：收集所有音符的打击事件（按时间排序），并接收已加载的打击音效与音乐时长。
    pub fn new(chart: &Chart, sfx_tap: Sfx, sfx_drag: Sfx, sfx_flick: Sfx, music_len: f32) -> Self {
        Self::build(chart, Some([sfx_tap, sfx_drag, sfx_flick]), music_len)
    }

    /// 导出模式（`--recorder`）：不实时播放打击音效（音效由 ffmpeg 与音乐混合）。
    pub fn new_silent(chart: &Chart, music_len: f32) -> Self {
        Self::build(chart, None, music_len)
    }

    fn build(chart: &Chart, sfx: Option<[Sfx; 3]>, music_len: f32) -> Self {
        let mut events = Vec::new();
        let mut holds = Vec::new();
        let mut note_count = 0usize;
        for (line_index, line) in chart.judge_line_list.iter().enumerate() {
            let seconds_per_t = 1.875 / line.bpm;
            for (side, notes) in [(1.0f32, &line.notes_above), (-1.0f32, &line.notes_below)] {
                for note in notes {
                    note_count += 1;
                    let time = note.time * seconds_per_t;
                    if note.note_type == 3 {
                        // Hold：下方为 180° 放置（x 镜像），故打击特效也取镜像后的 x。
                        holds.push(HoldEvent {
                            time,
                            end: time + note.hold_time * seconds_per_t,
                            line: line_index,
                            x: note.position_x,
                            mirror: side,
                            interval: if line.bpm.abs() > f32::EPSILON { 30.0 / line.bpm } else { 0.0 },
                        });
                    } else {
                        // 非 Hold 音符不镜像（与 `draw_note` 一致）。
                        events.push(HitEvent {
                            time,
                            line: line_index,
                            x: note.position_x,
                            mirror: 1.0,
                            note_type: note.note_type,
                        });
                    }
                }
            }
        }
        events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        holds.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            events,
            cursor: 0,
            holds,
            hold_cursor: 0,
            active_holds: Vec::new(),
            hits: Vec::new(),
            sfx,
            rng: 0x1234_5678,
            note_count,
            combo: 0,
            score: 0.0,
            music_len,
        }
    }

    /// 供导出混音使用：全部打击音效事件 `(谱面秒, 音效索引 0=tap / 1=drag / 2=flick)`。
    pub fn sfx_times(&self) -> Vec<(f32, usize)> {
        let mut out: Vec<(f32, usize)> = Vec::with_capacity(self.events.len() + self.holds.len());
        for e in &self.events {
            let idx = match e.note_type { 2 => SFX_DRAG, 4 => SFX_FLICK, _ => SFX_TAP };
            out.push((e.time, idx));
        }
        for h in &self.holds { out.push((h.time, SFX_TAP)); }
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// 简易 LCG 随机数（0..1）。
    fn next_rand(&mut self) -> f32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.rng >> 8) & 0x00FF_FFFF) as f32 / 16_777_216.0
    }

    /// 记一次命中：连击 +1，分数按满分均分累加（无输入判定，参考 CHCAT 在音符到时计分）。
    fn hit_score(&mut self) {
        self.combo += 1;
        if self.note_count > 0 {
            self.score += 1_000_000.0 / self.note_count as f64;
        }
    }

    /// 在 `line` 上 `position_x = x`（按 `mirror` 镜像）处、按 `now` 时刻的判定线位姿生成打击特效。
    fn spawn_hit(&mut self, chart: &Chart, line_index: usize, x: f32, mirror: f32, now: f32, w: f32, h: f32) {
        let line = &chart.judge_line_list[line_index];
        let (center, angle) = line_pose(chart, line, now * line.bpm * T_PER_SECOND, w, h);
        let px = mirror * x * NOTE_X_UNIT * w;
        let rad = angle.to_radians();
        let tau = std::f32::consts::TAU;
        let block_r = [
            self.next_rand() * tau,
            self.next_rand() * tau,
            self.next_rand() * tau,
            self.next_rand() * tau,
        ];
        self.hits.push(ActiveHit {
            center: [center[0] + px * rad.cos(), center[1] + px * rad.sin()],
            spawn: now,
            block_r,
        });
    }

    /// 处理到时的打击事件：生成特效并播放音效。
    fn update(&mut self, chart: &Chart, chart_time: f32, w: f32, h: f32) {
        // 非 Hold：到点即触发一次。
        while self.cursor < self.events.len() && self.events[self.cursor].time <= chart_time {
            let e = self.events[self.cursor];
            self.spawn_hit(chart, e.line, e.x, e.mirror, chart_time, w, h);
            let idx = match e.note_type {
                2 => SFX_DRAG,
                4 => SFX_FLICK,
                _ => SFX_TAP,
            };
            if let Some(sfx) = &mut self.sfx { let _ = sfx[idx].play(PlaySfxParams::default()); }
            self.hit_score();
            self.cursor += 1;
        }

        // Hold 头：播放一次音效，并登记其周期性打击（首次在头部时刻触发）。
        while self.hold_cursor < self.holds.len() && self.holds[self.hold_cursor].time <= chart_time {
            let event = self.holds[self.hold_cursor];
            if let Some(sfx) = &mut self.sfx { let _ = sfx[SFX_TAP].play(PlaySfxParams::default()); }
            self.active_holds.push(ActiveHold { next: event.time, event, counted: false });
            self.hold_cursor += 1;
        }

        // Hold：在持续期间每 `interval` 秒生成一次打击特效（CHCAT 逻辑）。
        let mut i = 0;
        while i < self.active_holds.len() {
            if chart_time >= self.active_holds[i].event.end {
                self.active_holds.swap_remove(i);
                continue;
            }
            // 接近结束（`end - 0.2`）时计一次连击/分数（参考 CHCAT）。
            if !self.active_holds[i].counted && chart_time > self.active_holds[i].event.end - 0.2 {
                self.active_holds[i].counted = true;
                self.hit_score();
            }
            let line = self.active_holds[i].event.line;
            let x = self.active_holds[i].event.x;
            let mirror = self.active_holds[i].event.mirror;
            let interval = self.active_holds[i].event.interval;
            if interval <= 0.0 {
                i += 1;
                continue;
            }
            while self.active_holds[i].next <= chart_time {
                self.active_holds[i].next += interval;
                self.spawn_hit(chart, line, x, mirror, chart_time, w, h);
            }
            i += 1;
        }
    }

    /// 绘制所有活动的打击特效，并移除已过期的。
    fn draw_hits(&mut self, renderer: &mut Renderer, chart_time: f32, note_width: f32) {
        // 参考实现为 6×5 共 30 帧。
        let frames = renderer.hit_frame_count().min(30);
        if frames == 0 {
            self.hits.clear();
            return;
        }
        let size = note_width * HIT_SIZE;
        let scale = note_width / 100.0;
        let gold = [1.0, 236.0 / 255.0, 160.0 / 255.0];
        let mut i = 0;
        while i < self.hits.len() {
            let hit = self.hits[i];
            let age = chart_time - hit.spawn;
            if age >= HIT_DURATION {
                self.hits.swap_remove(i);
                continue;
            }
            let k = (age / HIT_DURATION).clamp(0.0, 0.999);
            let frame = ((k * frames as f32) as usize).min(frames - 1);
            renderer.draw_hit(hit.center, size, frame, [1.0, 1.0, 1.0, 1.0]);

            // 金色小方块（sparks）：起点为 CHCAT `drawBlock` 的 `(hitX+256/4·size, hitY+256/3·size)`，
            // 相对特效中心约为 `(-10, -10)·size`（用户坐标 y 向上）。每个方块以**左上角**锚定
            // （CHCAT 用 `fillRect(x, y, w, h)`），故绘制中心再偏移 `(+bs/2, -bs/2)`。
            // 位移沿屏幕径向：`(cos, -sin)`（屏幕 y 向下，换算到用户坐标 y 向上取负）。
            // 缓动严格对应 CHCAT：`easeFuncs[7]`=out cubic（偏移）、`easeFuncs[8]`=in cubic（透明度）、
            // `easeFuncs[11]`=io cubic（边长）。
            let off = ease_out_cubic(k) * SPARK_OFFSET * note_width;
            let alpha = 1.0 - ease_in_cubic(k);
            let bs = SPARK_SIZE * note_width - ease_io_cubic(k);
            let ox = hit.center[0] - 10.0 * scale;
            let oy = hit.center[1] - 10.0 * scale;
            for ang in hit.block_r {
                let px = ox + off * ang.cos();
                let py = oy - off * ang.sin();
                renderer.draw_spark(
                    [px + bs * 0.5, py - bs * 0.5],
                    bs,
                    [gold[0], gold[1], gold[2], alpha],
                );
            }
            i += 1;
        }
    }

    /// 绘制 HUD（参考 CHCAT `rpe.js`：暂停图标、进度线、水印、连击、分数）。
    fn draw_hud(&self, renderer: &mut Renderer, chart_time: f32) {
        let size = renderer.render_size();
        let (w, h) = (size[0], size[1]);
        // 参考画布为 720×540，按 contain 比例缩放，保证与实际画面比例一致。
        let s = (w / 720.0).min(h / 540.0);
        let white = [1.0, 1.0, 1.0, 1.0];

        // 暂停图标：左上角，20×20。
        renderer.draw_ui_image("ui/pause.png", [-w / 2.0 + 30.0 * s, h / 2.0 - 30.0 * s], [20.0 * s, 20.0 * s]);

        // 进度线：顶部细线，白点随进度从左到右（timerLine 图 960×5，绘制 1.5×）。
        if self.music_len > 0.0 {
            let p = (chart_time / self.music_len).clamp(0.0, 1.0);
            let (dw, dh) = (960.0 * 1.5 * s, 5.0 * 1.5 * s);
            renderer.draw_ui_image(
                "ui/timerLine.png",
                [-w / 2.0 + p * w - dw / 2.0, h / 2.0 - dh / 2.0],
                [dw, dh],
            );
        }

        // 水印：右下角，12px（项目名 + 版本号）。
        let watermark = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"), "all code by CHCAT1320");
        let wm_px = 12.0 * s;
        let wm_w = renderer.measure_text(watermark, wm_px);
        renderer.draw_text(watermark, w / 2.0 - wm_w - 10.0 * s, -h / 2.0 + 5.0 * s, wm_px, white);

        // 连击：顶部居中，仅 combo > 2（数字 40px，文本 12px）。
        if self.combo > 2 {
            let combo_px = 40.0 * s;
            let combo_s = self.combo.to_string();
            let cw = renderer.measure_text(&combo_s, combo_px);
            renderer.draw_text(&combo_s, -cw / 2.0, h / 2.0 - 45.0 * s, combo_px, white);
            let cat_px = 12.0 * s;
            let tw = renderer.measure_text("CATPLAY", cat_px);
            renderer.draw_text("CATPLAY", -tw / 2.0, h / 2.0 - 60.0 * s, cat_px, white);
        }

        // 分数：右上角，24px（参考 CHCAT `getScoreText`）。
        let score_px = 24.0 * s;
        let score_text = score_to_text(self.score);
        let sw = renderer.measure_text(&score_text, score_px);
        renderer.draw_text(&score_text, w / 2.0 - sw - 25.0 * s, h / 2.0 - 35.0 * s, score_px, white);
    }
}

/// 分数显示文本（移植 CHCAT `getScoreText`）：
/// `score += 0.5` 后取整；`>= 1e6` 显示 `1000000`，否则 `0` + `(score/1e5)` 的 5 位小数拼接。
fn score_to_text(score: f64) -> String {
    let s = (score + 0.5).floor().max(0.0) as i64;
    if s >= 1_000_000 {
        "1000000".to_string()
    } else {
        format!("0{s:06}")
    }
}

/// 缓动：out cubic（对应 CHCAT `easeFuncs[7]`，用于小方块偏移）。
fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// 缓动：in cubic（对应 CHCAT `easeFuncs[8]`，用于小方块淡出）。
fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t.powi(3)
}

/// 缓动：in-out cubic（对应 CHCAT `easeFuncs[11]`，用于小方块边长）。
fn ease_io_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t.powi(3) } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// 模拟 Phigros 对 `Hold.holdTime` 的取整（负数的表现特殊：`-0.999→0`、`-1.001→-1`）。
fn floor_hold_time(t: f32) -> i32 {
    let t = t.floor();
    if t < 0.0 {
        (t + 1.0) as i32
    } else {
        t as i32
    }
}

/// 音符最大可见位置：判定线实时位置超过它后音符不再渲染（docs 实测数据 v2.3.1）。
///
/// 对应 JS `getMaxVisiblePos`：在大数值下按 2 的幂缩放，精确处理 f32 浮点误差。
fn get_max_visible_pos(x: f32) -> f32 {    if !x.is_finite() {
        return x;
    }
    const MAGIC: f64 = 11718.75;
    let n = x as f64;
    let prime: f64 = if n >= MAGIC {
        2f64.powi((1.0 + (n / MAGIC).log2()).floor() as i32)
    } else {
        1.0
    };
    let a = n / prime + 0.001;
    let r = a as f32;
    if (r as f64) <= a {
        return (r as f64 * prime) as f32;
    }
    let bits = r.to_bits();
    let bits = if r <= 0.0 {
        bits.wrapping_add(1)
    } else {
        bits.wrapping_sub(1)
    };
    (f32::from_bits(bits) as f64 * prime) as f32
}

/// 绘制单个非 Hold 音符（tap/drag/flick）。`side`：1 = 线上方，-1 = 线下方。
///
/// 局部坐标取自 docs「音符的实时参数」：x 为 `positionX·0.05625W`，y 为
/// `side·η·(pN − PJ(t))·0.6H`（η = `note.speed`）；`highlight` 为真时使用 HL 贴图。
fn draw_note(
    renderer: &mut Renderer,
    note: &Note,
    side: f32,
    time_t: f32,
    line_fp: f32,
    w: f32,
    h: f32,
    note_width: f32,
    highlight: bool,
) {
    let normal = match note.note_type {
        1 => NoteTexture::Tap,
        2 => NoteTexture::Drag,
        4 => NoteTexture::Flick,
        _ => return, // Hold（3）等暂不绘制
    };
    // 已过判定时刻（含 Hold 尾部）→ 不再显示
    if time_t >= note.time + note.hold_time {
        return;
    }
    // 已越过判定线（未打击）：判定线实时位置超过音符最大可见位置 → 不渲染
    let current = note.floor_position - line_fp;
    if line_fp > get_max_visible_pos(note.floor_position) {
        return;
    }
    // 实时垂直距离 Y(t) = η·current 超过 2H（η·current > 3.3333336）：不渲染
    if note.speed * current > FAR_LIMIT && note.time - time_t > 0.0 {
        return;
    }

    // docs 非 Hold 音符：Y = η·(pN − PJ(t))，η = note.speed
    let y = side * note.speed * current * NOTE_Y_UNIT * h;

    // 同一时刻有多个音符 → 用 HL 高亮贴图
    let texture = if highlight {
        match normal {
            NoteTexture::Tap => NoteTexture::TapHl,
            NoteTexture::Drag => NoteTexture::DragHl,
            NoteTexture::Flick => NoteTexture::FlickHl,
            hl => hl,
        }
    } else {
        normal
    };

    let x = note.position_x * NOTE_X_UNIT * w;
    renderer.draw_note([x, y], note_width, 0.0, texture, [1.0, 1.0, 1.0, 1.0]);
}

/// 绘制 Hold（type 3）：头/尾贴图 + 长度随剩余时间变化的主体四边形。
///
/// - 判定前（`t < tN`）：头按 `PN(t)` 接近判定线（不含 η），主体长度 `η·tH`，尾在头外侧；
/// - 判定后（`tN ≤ t < tN+tH`）：头停在判定线上，主体按 `η·(tN+tH−t)` 收缩。
/// 判定线下方的 Hold 水平镜像并上下翻转贴图（对应参考实现的 180° 旋转）。
fn draw_hold(
    renderer: &mut Renderer,
    note: &Note,
    side: f32,
    time_t: f32,
    line_fp: f32,
    w: f32,
    h: f32,
    note_width: f32,
    highlight: bool,
    bpm: f32,
) {
    // 长度为 0 的 Hold 不渲染：`speed` 为 0，或 `holdTime` 取整后为 0（docs 实测）
    if note.speed == 0.0 || floor_hold_time(note.hold_time) == 0 {
        return;
    }
    let t_end = note.time + note.hold_time;
    if time_t >= t_end {
        return;
    }

    let seconds_per_t = 1.875 / bpm;
    let unit = NOTE_Y_UNIT * h * note.speed;
    let current = note.floor_position - line_fp;
    let (head_y, tail_y) = if time_t < note.time {
        let head_y = side * current * NOTE_Y_UNIT * h;
        (head_y, head_y + side * unit * note.hold_time * seconds_per_t)
    } else {
        let remaining = t_end - time_t;
        (0.0, side * unit * remaining * seconds_per_t)
    };

    // 实时垂直距离 Y(t) = current 超过 2H（3.3333336）：不渲染
    if time_t < note.time && current > FAR_LIMIT {
        return;
    }

    let flip = side < 0.0;
    let x = note.position_x * NOTE_X_UNIT * w * if flip { -1.0 } else { 1.0 };
    let color = [1.0, 1.0, 1.0, 1.0];

    // 下侧 hold 相当于参考实现的 180° 旋转：x 已镜像，这里再对贴图上下翻转。
    let vflip = if flip { -1.0 } else { 1.0 };

    let body_tex = if highlight { NoteTexture::HoldBodyHl } else { NoteTexture::HoldBody };
    // 以主体「蓝色主色」宽度为目标，头/尾按自身「非透明」宽度比例缩放，
    // 使头/尾整条的宽度与主体蓝色部分对齐（HL 的黄色辉光不计入主体宽度）。
    let body_target = renderer.note_blue_width(body_tex).max(0.01);
    let head_tex = if highlight { NoteTexture::HoldHeadHl } else { NoteTexture::HoldHead };
    let head_w = note_width * body_target / renderer.note_alpha_width(head_tex).max(0.01);

    // 头：外边缘对齐头部位置，整条落在主体内侧（对应参考实现的左上角锚点）
    if time_t < note.time {
        let head_h = note_width * renderer.note_aspect(head_tex);
        renderer.draw_note_sized([x, head_y - side * head_h * 0.5], head_w, vflip * head_h, 0.0, head_tex, color, [0.0, 1.0]);
    }
    // 尾：内边缘对齐尾部位置，整条落在主体外侧。
    // 尾贴图与普通 hold 共用，HL 时其辉光占比与普通不同，故 HL 单独取与头同宽。
    let end_tex = NoteTexture::HoldEnd;
    let end_w = if highlight { head_w } else { note_width * body_target / renderer.note_alpha_width(end_tex).max(0.01) };
    let end_h = note_width * renderer.note_aspect(end_tex);
    renderer.draw_note_sized([x, tail_y + side * end_h * 0.5], end_w, vflip * end_h, 0.0, end_tex, color, [0.0, 1.0]);
    // 主体（最后绘制，盖在头尾之上）。高度随上/下方取号。
    let len = (tail_y - head_y).abs();
    if len > 0.0 {
        renderer.draw_note_sized([x, (head_y + tail_y) * 0.5], note_width, vflip * len, 0.0, body_tex, color, [0.0, 1.0]);
    }
}

/// 收集「出现次数 > 1」的音符判定时间（按位表示，避免浮点相等误差），
/// 用于判定该时刻的音符是否应使用 HL 高亮贴图。
fn highlight_times(chart: &Chart) -> std::collections::HashSet<u32> {
    let mut count: HashMap<u32, u32> = HashMap::new();
    for line in &chart.judge_line_list {
        for note in line.notes_above.iter().chain(line.notes_below.iter()) {
            *count.entry(note.time.to_bits()).or_insert(0) += 1;
        }
    }
    count
        .into_iter()
        .filter(|&(_, c)| c > 1)
        .map(|(t, _)| t)
        .collect()
}

/// `formatVersion` 移动事件坐标解包（docs.lchzh.net）。
///
/// - `1`：`start/end = 1000x+y`，左下原点，右上 `(880,520)`；
/// - `3`：`start/end` 为 x、`start2/end2` 为 y，左下原点，右上 `(1,1)`；
/// - 其它：画面中心原点，单位 `0.1 H`。
fn unpack_move(format_version: i32, e: &MoveEvent, f: f32) -> [f32; 2] {
    match format_version {
        1 => {
            let packed = lerp(e.start, e.end, f);
            let x = (packed / 1000.0).trunc();
            let y = packed - x * 1000.0;
            [x / 880.0, y / 520.0]
        }
        3 => [lerp(e.start, e.end, f), lerp(e.start2, e.end2, f)],
        _ => [lerp(e.start, e.end, f), lerp(e.start2, e.end2, f)],
    }
}

/// 二分定位 `time_t`（单位 T）所在的事件，并返回其插值系数。
///
/// 分母用**下一事件**的 `startTime`（`docs.lchzh.net/learning/phigros/calc`：
/// `SJ(t)=sk+(t−tk)(ek−sk)/(tk+1−tk)`）。官谱事件连续时与 `endTime` 等价。
fn locate<T: TimeEvent>(events: &[T], time_t: f32) -> Option<(&T, f32)> {
    if events.is_empty() {
        return None;
    }
    let idx = events.partition_point(|e| e.start_time() <= time_t);
    let i = if idx == 0 { 0 } else { idx - 1 };
    let e = &events[i];
    let s = e.start_time();
    let en = if i + 1 < events.len() {
        events[i + 1].start_time()
    } else {
        e.end_time()
    };
    let f = if en > s {
        ((time_t - s) / (en - s)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    Some((e, f))
}

/// 线性插值。
#[inline]
fn lerp(a: f32, b: f32, f: f32) -> f32 {
    a + (b - a) * f
}

/// 判定线速度事件积分出的 `floorPosition` 采样器。
///
/// 参考 <https://docs.lchzh.net/learning/phigros/calc>「速度事件的 floorPosition」。
/// 设第 k 个事件的 `startTime` 为 `tk`、`value` 为 `vk`、`floorPosition` 为 `pk`，
/// 判定线 BPM 为 `B`，则（`t` 与 `tk` 单位均为 T）：
///
/// ```text
/// p1 = t1 * 1.875 / B = 0
/// pk = pk-1 + vk-1 * (tk - tk-1) * 1.875 / B,   k >= 2
///
/// PJ(t) = pk + vk * (t - tk) * 1.875 / B,       tk <= t < tk+1
/// ```
///
/// 其中 `1.875 / B` 即 1 T 对应的秒数。该值与谱面音符的 `floorPosition` 同为 Y 单位，
/// 可直接用于 `currentFloorPosition = pN - PJ(t)`。
pub struct LineFloorPosition<'a> {
    /// 速度事件（须按 `startTime` 升序，首事件 `startTime = 0`，事件首尾相接）。
    speed_events: &'a [SpeedEvent],
    /// 各事件 `startTime` 处的 `floorPosition` `pk`，与 `speed_events` 一一对应。
    start_floor: Vec<f32>,
    /// `1.875 / BPM`：把单位 T 的时间换算为秒。
    scale: f32,
}

impl<'a> LineFloorPosition<'a> {
    /// 按判定线 BPM 预积分速度事件，之后用 [`Self::fp`] 采样任意时刻的 `floorPosition`。
    pub fn new(speed_events: &'a [SpeedEvent], bpm: f32) -> Self {
        let scale = if bpm.abs() > f32::EPSILON {
            1.875 / bpm
        } else {
            0.0
        };
        let mut start_floor = Vec::with_capacity(speed_events.len());
        let mut p = 0.0;
        for (k, event) in speed_events.iter().enumerate() {
            if k > 0 {
                let prev = &speed_events[k - 1];
                p += prev.value * (event.start_time - prev.start_time) * scale;
            }
            start_floor.push(p);
        }
        Self {
            speed_events,
            start_floor,
            scale,
        }
    }

    /// 传入时刻 `s`（单位 T，与 [`SpeedEvent::start_time`] 一致），返回判定线在该时刻的
    /// `floorPosition`（单位 Y）。`s` 早于首个事件时返回其初始值（规范谱面下为 `0`）。
    pub fn fp(&self, s: f32) -> f32 {
        if self.speed_events.is_empty() {
            return 0.0;
        }
        // 最后一个 `start_time <= s` 的事件；`s` 早于首事件时 `idx == 0`。
        let idx = self.speed_events.partition_point(|e| e.start_time <= s);
        if idx == 0 {
            return self.start_floor[0];
        }
        let k = idx - 1;
        let event = &self.speed_events[k];
        self.start_floor[k] + event.value * (s - event.start_time) * self.scale
    }
}

/// 加载谱面音符：用各判定线的速度事件重算所有音符的 `floorPosition`。
///
/// 谱面预存的 `floorPosition` 本应等于所在判定线在音符判定时刻的垂直位置
/// （`pN = PJ(tN)`，见 docs「音符的实时参数」）。此处按速度事件重新积分并覆写，
/// 使音符位置始终与渲染判定线所用的 [`LineFloorPosition`] 一致，不再依赖谱面存值。
///
/// [`Note::time`](crate::chart_fv::Note::time) 与速度事件同为 T 单位，可直接传入
/// [`LineFloorPosition::fp`]。
pub fn load_notes(chart: &mut Chart) {
    for line in &mut chart.judge_line_list {
        let fp = LineFloorPosition::new(&line.speed_events, line.bpm);
        // 先只读算出新值，再写回，避免同时借用 `line` 的不可变与可变字段。
        let above: Vec<f32> = line.notes_above.iter().map(|n| fp.fp(n.time)).collect();
        let below: Vec<f32> = line.notes_below.iter().map(|n| fp.fp(n.time)).collect();
        for (note, value) in line.notes_above.iter_mut().zip(above) {
            note.floor_position = value;
        }
        for (note, value) in line.notes_below.iter_mut().zip(below) {
            note.floor_position = value;
            // println!("note.floor_position: {}", note.floor_position);
        }
    }
}
