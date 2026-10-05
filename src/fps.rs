//! 简单的帧率统计。

use std::time::Instant;

/// 帧率统计器（指数滑动平均，读数更平稳）。
pub struct Fps {
    /// 上一次记录的时间点。
    last: Instant,
    /// 当前帧率（每秒帧数）。
    value: f32,
    /// 是否已完成第一次采样。
    initialized: bool,
}

impl Default for Fps {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            value: 0.0,
            initialized: false,
        }
    }
}

impl Fps {
    /// 创建统计器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一帧并更新帧率；每帧调用一次。
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;

        if dt <= 0.0 {
            return;
        }

        let instant = 1.0 / dt;
        if self.initialized {
            // 指数平滑，避免数字剧烈跳动
            const SMOOTHING: f32 = 0.1;
            self.value += (instant - self.value) * SMOOTHING;
        } else {
            self.value = instant;
            self.initialized = true;
        }
    }

    /// 当前帧率（每秒帧数）。
    pub fn value(&self) -> f32 {
        self.value
    }
}
