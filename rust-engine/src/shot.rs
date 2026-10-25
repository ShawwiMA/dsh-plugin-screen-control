//! 截图坐标登记表：把「截图里的像素坐标」自动换算成「屏幕物理像素」。
//!
//! 为什么需要：`win_capture` 会按 `max_dim` 缩放（默认长边 1600），于是模型在
//! 返回图上量出来的坐标（比如预览图里的 (800,764)）和真正能点的屏幕坐标差了
//! 一个系数。以前这个系数全靠人算（"截图 1400 宽 ×1.8286"），算错一个数整条线
//! 就画歪、整块区域的点击就落到别处；窗口一移动或换块区域，系数还得重算。
//!
//! 现在 `win_capture` 返回 `coord.shot_id` 和换算参数，动作工具传 `shot_id`
//! 之后所有坐标字段都按**该截图的像素**解释，由引擎换算成屏幕坐标。
//!
//! 生命周期：10 分钟 TTL、最多 32 张（超了淘汰最旧的）。这和 `snapshot_id`
//! 的 5 秒/元素快照不同——截图坐标是"看着图规划动作"用的，得活得久一点。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{Rect, ShotInfo};
use crate::snapshot::now_ms;

/// 默认存活时间。截图用于"看图规划"，一个规划回合通常几十秒，给足 10 分钟。
pub const SHOT_TTL_MS: u64 = 600_000;
/// 最多保留多少张。超了淘汰最旧的：内存里每张只存几个数字，上限只为防泄漏。
const MAX_SHOTS: usize = 32;

#[derive(Debug, Clone)]
pub struct Shot {
    pub info: ShotInfo,
    created_ms: i64,
}

impl Shot {
    /// 图片像素 → 屏幕物理像素。
    ///
    /// x/y 两个方向**分别**换算：缩放按长边等比，但窗口宽高比和缩略图不一定严格相等，
    /// 一个系数套两个轴会在边缘累积出几个像素的偏移。
    pub fn to_screen(&self, ix: f64, iy: f64) -> (i32, i32) {
        let i = &self.info;
        let sx = i.screen_rect.x as f64 + ix * i.scale_x;
        let sy = i.screen_rect.y as f64 + iy * i.scale_y;
        (sx.round() as i32, sy.round() as i32)
    }

    /// 图片里的长度（半径、笔宽…）→ 屏幕长度，两轴取平均。
    pub fn to_screen_len(&self, len: f64) -> f64 {
        len * (self.info.scale_x + self.info.scale_y) / 2.0
    }

    pub fn is_expired(&self) -> bool {
        now_ms() - self.created_ms >= self.info.ttl_ms as i64
    }

    pub fn remaining_ms(&self) -> i64 {
        (self.info.ttl_ms as i64 - (now_ms() - self.created_ms)).max(0)
    }
}

#[derive(Default)]
struct Store {
    shots: HashMap<u64, Shot>,
    next_id: u64,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

/// 登记一张截图，返回给调用方的换算信息（含 `shot_id`）。
pub fn register(hwnd: Option<u64>, rect: Rect, image_w: u32, image_h: u32) -> ShotInfo {
    let scale_x = if image_w > 0 { rect.w as f64 / image_w as f64 } else { 1.0 };
    let scale_y = if image_h > 0 { rect.h as f64 / image_h as f64 } else { 1.0 };
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    purge_locked(&mut s);
    // 容量满时淘汰最旧的（id 最小通常也最旧；重开进程后 id 从 1 开始）。
    if s.shots.len() >= MAX_SHOTS {
        if let Some(oldest) = s.shots.values().min_by_key(|x| x.created_ms).map(|x| x.info.shot_id) {
            s.shots.remove(&oldest);
        }
    }
    s.next_id += 1;
    let info = ShotInfo {
        shot_id: s.next_id,
        hwnd,
        screen_rect: rect,
        image_w,
        image_h,
        scale_x,
        scale_y,
        ttl_ms: SHOT_TTL_MS,
    };
    s.shots.insert(
        info.shot_id,
        Shot { info: info.clone(), created_ms: now_ms() },
    );
    info
}

/// 取一张还在有效期内的截图登记。
pub fn get_valid(id: u64) -> EngineResult<Shot> {
    let s = store().lock().unwrap_or_else(|e| e.into_inner());
    let shot = s.shots.get(&id).cloned().ok_or_else(|| {
        EngineError::invalid(format!(
            "shot_id={id} 不存在（引擎重启过，或这张截图已被淘汰）。重新 win_capture 拿新的 shot_id"
        ))
    })?;
    if shot.is_expired() {
        return Err(EngineError::invalid(format!(
            "shot_id={id} 已过期（存活 {}s，截图后太久没动作）。重新 win_capture",
            SHOT_TTL_MS / 1000
        )));
    }
    Ok(shot)
}

pub fn purge() {
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    purge_locked(&mut s);
}

fn purge_locked(s: &mut Store) {
    let now = now_ms();
    s.shots.retain(|_, v| now - v.created_ms < v.info.ttl_ms as i64);
}

pub fn len() -> usize {
    let s = store().lock().unwrap_or_else(|e| e.into_inner());
    s.shots.len()
}

pub fn clear_all() {
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    s.shots.clear();
}

/// 保留这个依赖，提醒 TTL 单位是毫秒（`Duration` 在别处也用于快照，避免重复 import）。
#[allow(dead_code)]
const _TTL_UNIT: Duration = Duration::from_millis(SHOT_TTL_MS);

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: -9, y: 0, w: 1298, h: 1609 }
    }

    #[test]
    fn maps_image_pixel_to_screen() {
        let info = register(Some(1), rect(), 1291, 1600);
        let shot = get_valid(info.shot_id).expect("刚登记应当有效");
        // 图片左上角 = 区域左上角
        assert_eq!(shot.to_screen(0.0, 0.0), (-9, 0));
        // 图片右下角 = 区域右下角
        assert_eq!(shot.to_screen(1291.0, 1600.0), (-9 + 1298, 1609));
        // 中间：800 * (1298/1291) ≈ 804.3 → 屏幕 x ≈ -9+804 = 795
        let (x, _) = shot.to_screen(800.0, 0.0);
        assert!((x - 795).abs() <= 1, "x={x}");
    }

    #[test]
    fn length_scales_by_average() {
        let info = register(None, rect(), 1291, 1600);
        let shot = get_valid(info.shot_id).unwrap();
        let len = shot.to_screen_len(100.0);
        assert!((len - 100.5).abs() < 1.0, "len={len}");
    }

    #[test]
    fn unknown_id_is_an_error() {
        clear_all();
        assert!(get_valid(999_999).is_err());
    }

    #[test]
    fn clear_all_empties_registry() {
        register(None, rect(), 100, 100);
        clear_all();
        assert_eq!(len(), 0);
    }
}
