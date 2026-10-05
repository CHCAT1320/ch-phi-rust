//! 谱面渲染：按时间绘制判定线（移动 / 旋转 / 消失）。
//!
//! 坐标与时间语义参考 <https://docs.lchzh.net/learning/phigros/>：
//! - 移动事件位置为 `formatVersion = 3` 的归一化坐标（左下角为原点，右上角为 (1, 1)）；
//! - 旋转事件值为逆时针角度（单位：度）；
//! - 时间单位为 T，1 T = 1.875 / BPM 秒。

use crate::chart_fv::{Chart, DisappearEvent, MoveEvent, RotateEvent};
use crate::renderer::Renderer;

/// 判定线默认归一化位置（画面中心）。
const DEFAULT_POS: [f32; 2] = [0.5, 0.5];

/// 判定线线宽（像素）。
const LINE_WIDTH: f32 = 5.0;

/// 秒 -> T 的换算系数（每 BPM）：1 T = 1.875 / BPM 秒，故 1 秒 = BPM / 1.875 T。
const T_PER_SECOND: f32 = 1.0 / 1.875;

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

/// 按当前音乐时间（秒）绘制谱面的所有判定线。
///
/// `time` 为音乐播放时间（秒），内部用 `chart.offset` 校正为谱面时间。
pub fn render(renderer: &mut Renderer, chart: &Chart, time: f32) {
    let size = renderer.window_size();
    let (w, h) = (size[0], size[1]);
    let chart_time = time - chart.offset;
    // 判定线横跨整屏：用对角线长度保证任意旋转都能盖住画面
    let line_len = (w * w + h * h).sqrt() * 2.0;

    for line in &chart.judge_line_list {
        // 谱面时间（秒）换算到该判定线的时间单位 T（每线只算一次）
        let time_t = chart_time * line.bpm * T_PER_SECOND;

        // 位置：一次二分定位，再按 formatVersion 解包
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

        // 逆时针角度（度）
        let angle = match locate(&line.judge_line_rotate_events, time_t) {
            Some((e, f)) => lerp(e.start, e.end, f),
            None => 0.0,
        };

        // 透明度
        let alpha = match locate(&line.judge_line_disappear_events, time_t) {
            Some((e, f)) => lerp(e.start, e.end, f),
            None => 1.0,
        }
        .clamp(0.0, 1.0);

        if alpha <= 0.0 {
            continue;
        }

        // 归一化 / 中心坐标 -> 世界坐标（原点在画面中心，y 向上，单位为像素）
        let center = if chart.format_version == 1 || chart.format_version == 3 {
            [(pos[0] - 0.5) * w, (pos[1] - 0.5) * h]
        } else {
            [pos[0] * 0.1 * h, pos[1] * 0.1 * h]
        };

        renderer.draw_line(
            center,
            line_len,
            angle,
            LINE_WIDTH,
            [255.0, 255.0, 255.0, alpha],
        );
    }
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
