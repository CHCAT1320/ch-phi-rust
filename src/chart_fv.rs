//! Phigros 谱面格式（formatVersion 3）的类型定义。
//!
//! 对应 `Chart*.json` 文件，可用 serde 直接反序列化。

use serde::{Deserialize, Serialize};

/// 谱面根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chart {
    /// 格式版本（当前为 3）。
    pub format_version: i32,
    /// 音画偏移（秒）。
    pub offset: f32,
    /// 判定线列表。
    pub judge_line_list: Vec<JudgeLine>,
    /// 遮罩区域列表（部分谱面没有该字段）。
    #[serde(default)]
    pub block_area_list: Vec<BlockArea>,
}

/// 一条判定线及其所有事件与音符。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeLine {
    /// 该判定线的 BPM。
    pub bpm: f32,
    /// 线上方音符。
    pub notes_above: Vec<Note>,
    /// 线下方音符。
    pub notes_below: Vec<Note>,
    /// 速度事件。
    pub speed_events: Vec<SpeedEvent>,
    /// 判定线移动事件。
    pub judge_line_move_events: Vec<MoveEvent>,
    /// 判定线旋转事件。
    pub judge_line_rotate_events: Vec<RotateEvent>,
    /// 判定线消失（透明度）事件。
    pub judge_line_disappear_events: Vec<DisappearEvent>,
}

/// 音符。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    /// 音符类型：1 = Tap，2 = Drag，3 = Hold，4 = Flick。
    #[serde(rename = "type")]
    pub note_type: i32,
    /// 判定时间（单位：1/32 拍）。
    pub time: f32,
    /// 横向位置。
    pub position_x: f32,
    /// Hold 音符的持续时间（单位：1/32 拍）。
    pub hold_time: f32,
    /// 速度倍率。
    pub speed: f32,
    /// 下落位置。
    pub floor_position: f32,
}

/// 速度事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedEvent {
    /// 起始时间。
    pub start_time: f32,
    /// 结束时间。
    pub end_time: f32,
    /// 速度值。
    pub value: f32,
}

/// 判定线移动事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveEvent {
    /// 起始时间。
    pub start_time: f32,
    /// 结束时间。
    pub end_time: f32,
    /// 起点 x。
    pub start: f32,
    /// 终点 x。
    pub end: f32,
    /// 起点 y（`formatVersion=3`；v1 无此字段）。
    #[serde(default)]
    pub start2: f32,
    /// 终点 y（`formatVersion=3`；v1 无此字段）。
    #[serde(default)]
    pub end2: f32,
}

/// 判定线旋转事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RotateEvent {
    /// 起始时间。
    pub start_time: f32,
    /// 结束时间。
    pub end_time: f32,
    /// 起始角度（度，逆时针）。
    pub start: f32,
    /// 结束角度（度，逆时针）。
    pub end: f32,
}

/// 判定线消失（透明度）事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisappearEvent {
    /// 起始时间。
    pub start_time: f32,
    /// 结束时间。
    pub end_time: f32,
    /// 起始透明度。
    pub start: f32,
    /// 结束透明度。
    pub end: f32,
}

/// 二维坐标点。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    /// 横坐标。
    pub x: f32,
    /// 纵坐标。
    pub y: f32,
}

/// 遮罩区域（`blockAreaList` 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockArea {
    /// 右上角百分比坐标。
    pub top_right_percentage: Point,
    /// 左下角百分比坐标。
    pub bottom_left_percentage: Point,
    /// 出现时间。
    pub appear_time: f32,
    /// 启用时间。
    pub enable_time: f32,
    /// 禁用时间。
    pub disable_time: f32,
    /// 消失时间。
    pub disappear_time: f32,
    /// 是否为减集。
    pub is_subtract: bool,
    /// 旋转事件。
    pub rotate_events: Vec<BlockRotateEvent>,
    /// 移动事件。
    pub move_events: Vec<BlockMoveEvent>,
    /// 缩放事件。
    pub scale_events: Vec<BlockScaleEvent>,
}

/// 遮罩区域移动事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockMoveEvent {
    /// 目标位置。
    pub end_position: Point,
    /// 时间。
    pub time: f32,
    /// x 缓动类型。
    pub ease_type_x: i32,
    /// y 缓动类型。
    pub ease_type_y: i32,
}

/// 遮罩区域旋转事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockRotateEvent {
    /// 旋转锚点。
    pub anchor: Point,
    /// 时间。
    pub time: f32,
    /// 缓动类型。
    pub ease_type: i32,
    /// 旋转角度。
    pub rotation: f32,
}

/// 遮罩区域缩放事件。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockScaleEvent {
    /// 缩放锚点。
    pub anchor: Point,
    /// 时间。
    pub time: f32,
    /// x 缓动类型。
    pub ease_type_x: i32,
    /// y 缓动类型。
    pub ease_type_y: i32,
    /// 缩放比例。
    pub scale: Point,
}
