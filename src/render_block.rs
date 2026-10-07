//! 块（`blockAreaList`）渲染。
//!
//! 依据 `E:\studyMyGame\dump\blockArea\docs`（Phigros 4.0.1 逆向笔记 `behavior.md`
//! 与 `code/PreviewBlockControl.decompiled.cs`）：
//! - 块时间字段单位为**秒**（不是判定线事件的 T）；
//! - 块是单位 quad：`localPosition` 为矩形中心，`localScale` 为矩形宽高；
//! - 百分比坐标 `P`：`0` 为左下角、`1` 为右上角；
//! - 每帧顺序：Scale → Rotation → Movement；
//! - 缓动为 15 张表，`3/6/9/13` 恒 0、`14` 恒 1、`12` 分段。

use std::sync::OnceLock;

use crate::chart_fv::{BlockArea, BlockMoveEvent, BlockRotateEvent, BlockScaleEvent, Point};
use crate::renderer::{
    Renderer, PHASE_DISABLED_NORMAL, PHASE_DISABLED_SUBTRACT, PHASE_NORMAL, PHASE_READY_NORMAL,
    PHASE_READY_SUBTRACT, PHASE_SUBTRACT,
};

/// 减块的透明度（`.rodata` `0xC261F0`）。
const SUBTRACT_ALPHA: f32 = 0.1;
/// `disabledBlockReadyDuration` / `disabledBlockShowDuration`（秒）。
const READY_DURATION: f32 = 0.5;

/// 绘制给定时间（秒，已扣除关卡 offset）下的全部块。
///
/// `shader_time` 是着色器 `_Time`（Unity 实时时钟，与音乐/谱面时钟无关），
/// 而 `time`（nowTime）只用于阶段判定与事件插值——两者不可混用。
pub fn render(renderer: &mut Renderer, blocks: &[BlockArea], time: f32, shader_time: f32) {
    // 供块着色器做位移/呼吸动画（对应 Unity `_Time.x/.y`）
    renderer.set_time(shader_time);

    // 用渲染像素尺寸作为「视口世界尺寸」，这样 AnchorToWorld 直接得到
    // 与渲染器一致的像素坐标（原点居中、y 向上）。
    let screen = renderer.render_size();

    for block in blocks {
        // 隐藏阶段：出现前 / 消失后
        if time < block.appear_time || time >= block.disappear_time {
            continue;
        }

        // 基础几何（`UpdateBlocksTransform`）
        let bl = anchor_to_world(block.bottom_left_percentage, screen);
        let tr = anchor_to_world(block.top_right_percentage, screen);
        let original_size = [tr[0] - bl[0], tr[1] - bl[1]];
        let original_center = [(bl[0] + tr[0]) * 0.5, (bl[1] + tr[1]) * 0.5];

        // Scale → Rotation → Movement
        let (size, center) = update_scale(block, time, original_size, original_center, screen);
        let (angle, center) = update_rotation(block, time, center, screen);
        let center = update_movement(block, time, original_center, center, screen);

        // 阶段 -> 遮罩 RT。`disableTime ≤ τ < disappearTime` 退回 disabled（残留窗口）。
        let phase = if block.is_subtract {
            if time < block.enable_time - READY_DURATION {
                PHASE_DISABLED_SUBTRACT
            } else if time < block.enable_time {
                PHASE_READY_SUBTRACT
            } else if time < block.disable_time {
                PHASE_SUBTRACT
            } else {
                PHASE_DISABLED_SUBTRACT
            }
        } else if time < block.enable_time - READY_DURATION {
            PHASE_DISABLED_NORMAL
        } else if time < block.enable_time {
            PHASE_READY_NORMAL
        } else if time < block.disable_time {
            PHASE_NORMAL
        } else {
            PHASE_DISABLED_NORMAL
        };

        renderer.draw_block(
            center,
            size[0].abs(),
            size[1].abs(),
            angle,
            sprite_color(block, time),
            phase,
        );
    }
}

/// `DisabledBlockShow`：首次进入画面且已在生效窗口内时，按 `showDuration` 淡入。
/// 普通 `(1,1,1,0)→(1,1,1,1)`，减块 `(1,0,1,0.1)→(1,1,1,0.1)`。
fn sprite_color(block: &BlockArea, now: f32) -> [f32; 4] {
    let in_window = block.enable_time <= block.appear_time && block.appear_time < block.disable_time;
    if in_window {
        let k = ((now - block.appear_time) / READY_DURATION).clamp(0.0, 1.0);
        if block.is_subtract {
            [1.0, k, 1.0, SUBTRACT_ALPHA]
        } else {
            [1.0, 1.0, 1.0, k]
        }
    } else if block.is_subtract {
        [1.0, 1.0, 1.0, SUBTRACT_ALPHA]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

/// 百分比坐标 -> 像素坐标（视口中心为原点，y 向上）。
fn anchor_to_world(anchor: Point, screen: [f32; 2]) -> [f32; 2] {
    [(anchor.x - 0.5) * screen[0], (anchor.y - 0.5) * screen[1]]
}

/// 绕锚点缩放。
fn scale_around(point: [f32; 2], anchor: [f32; 2], step_x: f32, step_y: f32) -> [f32; 2] {
    [
        anchor[0] + (point[0] - anchor[0]) * step_x,
        anchor[1] + (point[1] - anchor[1]) * step_y,
    ]
}

/// 绕锚点旋转（逆时针，`delta_deg` 单位度）。
fn rotate_around(point: [f32; 2], anchor: [f32; 2], delta_deg: f32) -> [f32; 2] {
    let rad = delta_deg * 0.017453_292;
    let (sin, cos) = rad.sin_cos();
    let dx = point[0] - anchor[0];
    let dy = point[1] - anchor[1];
    [
        anchor[0] + (dx * cos - dy * sin),
        anchor[1] + (dx * sin + dy * cos),
    ]
}

/// `Mathf.Epsilon`（Unity 最小正浮点，denormal）。
const MATHF_EPSILON: f32 = 1.401_298e-45;

/// 退化时返回 `1.0` 的安全除法。
fn safe_div(numerator: f32, denominator: f32) -> f32 {
    let eps = (denominator.abs() * 1e-6).max(8.0 * MATHF_EPSILON);
    if denominator.abs() < eps {
        1.0
    } else {
        numerator / denominator
    }
}

/// `FindCurrentEventIndex<T>`（VA `0x1F9A2DC`）：返回最后一个 `time <= now` 的事件，
/// 索引范围 `[-1, Count-2]`（`-1` = 早于首事件；`Count-2` = 最后一个事件对）。
///
/// - `now` 语义为 `progressControl.nowTime`（本帧固定值，C# 在循环体内重读同一帧值，等价）；
/// - 时间戳重复时**选最靠后的那个**（用 `t > now` 严格大于，相等则继续前进）；
/// - 从头线性扫描，无二分。
fn find_current<T>(events: &[T], now: f32, time: impl Fn(&T) -> f32) -> i32 {
    if events.is_empty() {
        return -1;
    }
    let mut i: i32 = -1;
    loop {
        let t = time(&events[(i + 1) as usize]);
        if t > now {
            return i;
        }
        i += 1;
        if (i as usize) + 2 >= events.len() {
            return i;
        }
    }
}

/// `CalculateEasedProgress(cur, next, ...)` 的裸除法 `(now - cur) / (next - cur)`。
///
/// `behavior.md` §2.3：原版 `CalculateEasedProgress` **无除零保护**，相邻事件时间相等时
/// 进度为 `±Inf`/`NaN`，并要求「复现时应显式处理这一情形（例如相等时直接取 `next` 的值）」。
/// 这里相等时返回 `1.0`，经 `ease(1.0)` 与 `lerp` 后恰好取到 `next` 的值，即文档建议的退化行为。
fn progress(now: f32, cur: f32, next: f32) -> f32 {
    if next > cur {
        (now - cur) / (next - cur)
    } else {
        1.0
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 缓动表（`behavior.md` §2.1）：15 张 × 101 点，与 `GetEase.Instantiation` 一致。
///
/// - `E[0] = u`；
/// - `E[idx] = u^n`、`E[idx+1] = 1-(1-u)^n`，`idx ∈ {1,4,7,10}`、`n = idx/3+2`；
/// - `E[12]` 由 `E[10]`/`E[11]` 隔点降采样折半，`50…57` 为零断点（`47…49` 在
///   原版是构建期越界读堆垃圾，此处按公式对源索引钳制到 `100` 处理）；
/// - `E[3/6/9/13] = 0`、`E[14] = 1`。
fn ease_tables() -> &'static [[f32; 101]; 15] {
    static TABLES: OnceLock<[[f32; 101]; 15]> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = [[0.0f32; 101]; 15];
        for i in 0..=100 {
            t[0][i] = i as f32 / 100.0;
        }
        for idx in [1usize, 4, 7, 10] {
            let n = (idx / 3 + 2) as i32;
            for i in 0..=100 {
                let u = i as f32 / 100.0;
                t[idx][i] = u.powi(n);
                t[idx + 1][i] = 1.0 - (1.0 - u).powi(n);
            }
        }
        for j in 0..=49usize {
            let k = (8 + 2 * j).min(100);
            t[12][j] = t[10][k] * 0.5;
            if 58 + j <= 100 {
                t[12][58 + j] = t[11][k] * 0.5 + 0.5;
            }
        }
        t[12][100] = 1.0;
        for i in 0..=100 {
            t[14][i] = 1.0;
        }
        t
    })
}

/// `GetEase.GetEaseWithProgress`（VA `0x1CAE190`）：查表 + 线性插值。
///
/// 对齐 `behavior.md` §2.2：
/// - `NaN`：原版经 ARM64 饱和转换 + `csel` 得到 `INT_MIN`（符号位为 1）→ 命中 `i < 0` 分支返回
///   `table[0]`；此处直接返回 `table[0]`，等价且安全；
/// - `i >= 100` → `table[100]`，`i < 0` → `table[0]`；
/// - 否则 `b + f * (a - b)`（即 `Mathf.Lerp(table[i], table[i+1], f)`）。
///
/// `ease_type` 越界：原版两个入口都先 `EaseInfos.Count > type`，越界直接抛
/// `IndexOutOfRangeException`。此处 clamp 到 `[0,14]` 以避免 panic（安全偏离，谱面正常值均在内）。
fn ease(ease_type: i32, progress: f32) -> f32 {
    let tables = ease_tables();
    let table = &tables[ease_type.clamp(0, 14) as usize];
    if progress.is_nan() {
        return table[0];
    }
    let s = progress * 100.0;
    let i = s as i32;
    if i >= 100 {
        return table[100];
    }
    if i < 0 {
        return table[0];
    }
    let b = table[i as usize];
    let a = table[i as usize + 1];
    let f = s - i as f32;
    b + f * (a - b)
}

/// `UpdateScale`：返回 `(size, center)`。
///
/// 结构对齐 `decompiled.cs` 的 `UpdateScale`：
/// 1) 对 `index` 之前的每个事件，用 `SafeDiv(e[i+1].scale, e[i].scale)` 绕 `e[i].anchor` 缩放 `center`；
/// 2) `index >= Count-1` 时 `size = ev[index].scale * originalSize`，否则用当前事件的双轴插值比例
///    再绕 `cur.anchor` 缩放一次，`size = interp * originalSize`。
///
/// `index == -1`（`now` 早于首事件）：`behavior.md` §2.3 指出原版此处会越界读取（`Count==1`），
/// 并要求「复现时应显式处理」。这里显式回退到原始几何，属文档认可的降级，而非原版越界行为。
fn update_scale(
    block: &BlockArea,
    now: f32,
    original_size: [f32; 2],
    base_center: [f32; 2],
    screen: [f32; 2],
) -> ([f32; 2], [f32; 2]) {
    let ev = &block.scale_events;
    if ev.is_empty() {
        return (original_size, base_center);
    }
    let index = find_current(ev, now, |e: &BlockScaleEvent| e.time);
    // 显式处理 `index == -1`（文档 §2.3 要求的单元素越界情形），返回原始几何。
    if index == -1 {
        return (original_size, base_center);
    }

    let mut p = base_center;
    // 历史事件累积：不加 `Clamp`（原版 `UpdateScale` 的历史循环不做 `Clamp01`）。
    for i in 0..index as usize {
        let (e0, e1) = (&ev[i], &ev[i + 1]);
        let a = anchor_to_world(e0.anchor, screen);
        p = scale_around(
            p,
            a,
            safe_div(e1.scale.x, e0.scale.x),
            safe_div(e1.scale.y, e0.scale.y),
        );
    }

    if index >= ev.len() as i32 - 1 {
        let s = ev[index as usize].scale;
        ([s.x * original_size[0], s.y * original_size[1]], p)
    } else {
        let (cur, next) = (&ev[index as usize], &ev[index as usize + 1]);
        let t = progress(now, cur.time, next.time);
        let interp = [
            lerp(cur.scale.x, next.scale.x, ease(cur.ease_type_x, t).clamp(0.0, 1.0)),
            lerp(cur.scale.y, next.scale.y, ease(cur.ease_type_y, t).clamp(0.0, 1.0)),
        ];
        let a = anchor_to_world(cur.anchor, screen);
        p = scale_around(
            p,
            a,
            safe_div(interp[0], cur.scale.x),
            safe_div(interp[1], cur.scale.y),
        );
        (
            [interp[0] * original_size[0], interp[1] * original_size[1]],
            p,
        )
    }
}

/// `UpdateRotation`：返回 `(角度, center)`。
///
/// 结构对齐 `decompiled.cs` 的 `UpdateRotation`：对 `index` 之前的每个事件累积
/// `e[i+1].rotation - e[i].rotation`（绕 `e[i].anchor`），再叠加当前事件（插值）的 `Δrotation`。
/// `rotation` 为**绝对角**（直接赋给 `eulerAngles.z`）。
///
/// `index == -1`：同 [`update_scale`]，文档 §2.3 要求显式处理，这里回退到 `(0, center)`。
fn update_rotation(
    block: &BlockArea,
    now: f32,
    center: [f32; 2],
    screen: [f32; 2],
) -> (f32, [f32; 2]) {
    let ev = &block.rotate_events;
    if ev.is_empty() {
        return (0.0, center);
    }
    let index = find_current(ev, now, |e: &BlockRotateEvent| e.time);
    // 显式处理 `index == -1`（文档 §2.3）。
    if index == -1 {
        return (0.0, center);
    }

    let mut p = center;
    // 历史事件累积：不加 `Clamp`（原版历史循环不做 `Clamp01`）。
    for i in 0..index as usize {
        let (e0, e1) = (&ev[i], &ev[i + 1]);
        let a = anchor_to_world(e0.anchor, screen);
        p = rotate_around(p, a, e1.rotation - e0.rotation);
    }

    if index >= ev.len() as i32 - 1 {
        (ev[index as usize].rotation, p)
    } else {
        let (cur, next) = (&ev[index as usize], &ev[index as usize + 1]);
        let t = ease(cur.ease_type, progress(now, cur.time, next.time)).clamp(0.0, 1.0);
        let rotation = lerp(cur.rotation, next.rotation, t);
        let a = anchor_to_world(cur.anchor, screen);
        p = rotate_around(p, a, rotation - cur.rotation);
        (rotation, p)
    }
}

/// `UpdateMovement`：返回移动后的 `center`。
///
/// 对齐 `decompiled.cs`：`center += InterpolateMoveEvent(index) - originalCenter`，
/// `index == -1` 时（`i != -1` 守卫，原版 `UpdateMovement` **有**该守卫）原样返回 `center`。
fn update_movement(
    block: &BlockArea,
    now: f32,
    original_center: [f32; 2],
    center: [f32; 2],
    screen: [f32; 2],
) -> [f32; 2] {
    let ev = &block.move_events;
    if ev.is_empty() {
        return center;
    }
    let index = find_current(ev, now, |e: &BlockMoveEvent| e.time);
    if index == -1 {
        return center;
    }

    let target = interpolate_move(ev, index as usize, now, screen);
    [
        center[0] + target[0] - original_center[0],
        center[1] + target[1] - original_center[1],
    ]
}

/// 插值移动事件目标位置（像素）。
///
/// 对齐 `decompiled.cs` 的 `InterpolateMoveEvent`：`index >= Count-1` 时直接返回
/// `AnchorToWorld(ev[index].endPosition)`；否则用 `easeTypeX`/`easeTypeY` 分别求双轴进度，
/// 插值 `endPosition` 后转世界坐标。
///
/// `behavior.md` §2.3：正常路径下 `index` 最大为 `Count-2`，`next` 始终存在；时间越过最后一个
/// 事件后进度 `p > 1` 由 `Clamp01` 收敛到 `1`，输出恰好等于 `events[Count-1]`——不是跳过插值。
fn interpolate_move(
    ev: &[BlockMoveEvent],
    index: usize,
    now: f32,
    screen: [f32; 2],
) -> [f32; 2] {
    if index >= ev.len() - 1 {
        return anchor_to_world(ev[index].end_position, screen);
    }
    let (cur, next) = (&ev[index], &ev[index + 1]);
    let t = progress(now, cur.time, next.time);
    let v = Point {
        x: lerp(cur.end_position.x, next.end_position.x, ease(cur.ease_type_x, t).clamp(0.0, 1.0)),
        y: lerp(cur.end_position.y, next.end_position.y, ease(cur.ease_type_y, t).clamp(0.0, 1.0)),
    };
    anchor_to_world(v, screen)
}
