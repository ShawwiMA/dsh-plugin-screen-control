//! 动作生效验证：动作**前**存一张基准图，动作**后**轮询比对，交出"到底有没有变化"。
//!
//! 为什么必须有：0.1.x 的动作工具只保证"指令投递出去了"，目标程序收没收到、界面有没有
//! 反应，一律不知道。2026-10-24 绝区零实测就是这样——`win_click` 报成功，游戏里一点反应
//! 都没有（自绘 UI 把事件吞了），调用方只能靠自己再截一张图人肉核对。**报"成功"却没发生，
//! 比报错更贵**，所以做成动作工具的共同可选项：传 `verify` 就自动验，不传零开销。
//!
//! 两种判定：
//! - `pixels`（默认）：动作点周围一块区域（或指定 region）画面变化比例 ≥ 阈值。
//!   注意**动画/视频类目标本来就一直在动**，这类场景要把 threshold 调大，否则永远"生效"。
//! - `window`：动作前不存在的窗口出现了。点开对话框/菜单/弹层用这个，比像素稳得多。
//!
//! 基准图用 `capture::capture(Auto)`：后台窗口靠 PrintWindow，拿不到内容再回退 BitBlt。

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;

use crate::capture::{self, RawImage};
use crate::protocol::{
    defaults, CaptureBackend, EffectParams, EffectResult, Rect, VerifyMode, WindowFilter,
};
use crate::{dpi, window};

/// 动作前的现场快照，动作结束后交给 [`finish`]。
pub struct Baseline {
    target: HWND,
    params: EffectParams,
    mode: VerifyMode,
    region: Option<Rect>,
    img: Option<RawImage>,
    /// window 模式：动作前是否**已经**存在匹配窗口。已经存在的窗口不能算"这次动作的效果"。
    window_existed: bool,
    warnings: Vec<String>,
}

/// 动作前调用：抓基准图 / 记录窗口现场。
///
/// `anchor` 是动作发生的屏幕坐标（click 的点、drag 的终点、drag_path 的起点…），
/// 用来决定默认比对区域；没有坐标（如 press）就退回整窗。
pub fn begin(target: HWND, params: &EffectParams, anchor: Option<(i32, i32)>) -> Baseline {
    let mode = params.mode.unwrap_or(VerifyMode::Pixels);
    let mut warnings: Vec<String> = Vec::new();

    if window::is_minimized(target) {
        warnings.push("目标窗口处于最小化状态，画面比对可能永远看不到变化".into());
    }

    if mode == VerifyMode::Window {
        let window_existed = find_window(params)
            .map(|hits| !hits.is_empty())
            .unwrap_or_else(|e| {
                warnings.push(format!("列举窗口失败：{e}"));
                false
            });
        return Baseline {
            target,
            params: params.clone(),
            mode,
            region: None,
            img: None,
            window_existed,
            warnings,
        };
    }

    let region = resolve_region(target, params, anchor, &mut warnings);
    let img = match capture::capture(Some(target), Some(region), CaptureBackend::Auto) {
        Ok(img) => Some(img),
        Err(e) => {
            warnings.push(format!("基准图抓取失败，已跳过验证：{e}"));
            None
        }
    };
    Baseline { target, params: params.clone(), mode, region: Some(region), img, window_existed: false, warnings }
}

/// 动作后调用：轮询到"变化出现"或超时。
pub fn finish(b: Baseline) -> EffectResult {
    let timeout = Duration::from_millis(
        b.params
            .timeout_ms
            .unwrap_or(defaults::EFFECT_TIMEOUT_MS)
            .min(defaults::EFFECT_MAX_TIMEOUT_MS),
    );
    let interval = Duration::from_millis(
        b.params.interval_ms.unwrap_or(defaults::EFFECT_INTERVAL_MS).max(20),
    );
    let started = Instant::now();
    let mut warnings = b.warnings.clone();
    let mut polls: u32 = 0;
    let mut best_diff = 0.0_f64;

    match b.mode {
        VerifyMode::Window => {
            if let Err(e) = validate_window_filter(&b.params) {
                return skipped(b.mode, b.region, warnings, e);
            }
            loop {
                polls += 1;
                match find_window(&b.params) {
                    Ok(hits) => {
                        if !hits.is_empty() && !b.window_existed {
                            let hit = &hits[0];
                            return EffectResult {
                                checked: true,
                                changed: true,
                                mode: b.mode.as_str().to_string(),
                                diff_ratio: 0.0,
                                polls,
                                waited_ms: started.elapsed().as_millis() as u64,
                                region: None,
                                detail: Some(format!(
                                    "出现新窗口 hwnd={} 标题={:?} 类名={}",
                                    hit.hwnd, hit.title, hit.class_name
                                )),
                                warnings,
                            };
                        }
                    }
                    Err(e) => warnings.push(format!("轮询窗口失败：{e}")),
                }
                if started.elapsed() >= timeout {
                    break;
                }
                std::thread::sleep(interval);
            }
            let detail = if b.window_existed {
                Some("动作前该窗口就已经存在，无法据此判定生效".to_string())
            } else {
                Some(format!("{}ms 内没有出现匹配窗口", timeout.as_millis()))
            };
            EffectResult {
                checked: true,
                changed: false,
                mode: b.mode.as_str().to_string(),
                diff_ratio: 0.0,
                polls,
                waited_ms: started.elapsed().as_millis() as u64,
                region: None,
                detail,
                warnings,
            }
        }
        VerifyMode::Pixels => {
            let (Some(base), Some(region)) = (b.img.as_ref(), b.region) else {
                return skipped(b.mode, b.region, warnings, "基准图缺失".to_string());
            };
            let threshold = b.params.threshold.unwrap_or(defaults::EFFECT_THRESHOLD).clamp(0.0, 1.0);
            let mut capture_failed = false;
            loop {
                polls += 1;
                match capture::capture(Some(b.target), Some(region), CaptureBackend::Auto) {
                    Ok(img) => {
                        let ratio = diff_ratio(&base.rgba, &img.rgba);
                        best_diff = best_diff.max(ratio);
                        if ratio >= threshold {
                            return EffectResult {
                                checked: true,
                                changed: true,
                                mode: b.mode.as_str().to_string(),
                                diff_ratio: ratio,
                                polls,
                                waited_ms: started.elapsed().as_millis() as u64,
                                region: Some(region),
                                detail: Some(format!(
                                    "变化像素 {:.2}% ≥ 阈值 {:.2}%",
                                    ratio * 100.0,
                                    threshold * 100.0
                                )),
                                warnings,
                            };
                        }
                    }
                    Err(e) => {
                        if !capture_failed {
                            warnings.push(format!("轮询抓图失败：{e}"));
                            capture_failed = true;
                        }
                    }
                }
                if started.elapsed() >= timeout {
                    break;
                }
                std::thread::sleep(interval);
            }
            EffectResult {
                checked: true,
                changed: false,
                mode: b.mode.as_str().to_string(),
                diff_ratio: best_diff,
                polls,
                waited_ms: started.elapsed().as_millis() as u64,
                region: Some(region),
                detail: Some(format!(
                    "{}ms 内最大变化 {:.2}%，未达阈值 {:.2}%",
                    timeout.as_millis(),
                    best_diff * 100.0,
                    threshold * 100.0
                )),
                warnings,
            }
        }
    }
}

fn skipped(mode: VerifyMode, region: Option<Rect>, mut warnings: Vec<String>, why: String) -> EffectResult {
    warnings.push(format!("未执行验证：{why}"));
    EffectResult {
        checked: false,
        changed: false,
        mode: mode.as_str().to_string(),
        diff_ratio: 0.0,
        polls: 0,
        waited_ms: 0,
        region,
        detail: None,
        warnings,
    }
}

/// 比对两张同尺寸 RGBA 图的差异比例。
///
/// 取样：每 3 个像素取 1 个（12 字节），三通道差值之和 > 24 才算"变了"——
/// 压掉 PrintWindow 的轻微合成噪声和抗锯齿抖动，避免把 1 个像素的抖动当生效。
fn diff_ratio(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len()) / 12;
    if n == 0 {
        return 0.0;
    }
    let mut diff = 0usize;
    for i in 0..n {
        let o = i * 12;
        let d = (a[o] as i32 - b[o] as i32).abs()
            + (a[o + 1] as i32 - b[o + 1] as i32).abs()
            + (a[o + 2] as i32 - b[o + 2] as i32).abs();
        if d > 24 {
            diff += 1;
        }
    }
    diff as f64 / n as f64
}

/// 比对区域：显式 region > 动作点 ±160px（夹到窗口内）> 整窗。
fn resolve_region(target: HWND, p: &EffectParams, anchor: Option<(i32, i32)>, warnings: &mut Vec<String>) -> Rect {
    let win = dpi::window_rect(target).unwrap_or_default();
    if let Some(r) = p.region {
        if r.is_empty() {
            warnings.push("verify.region 宽或高 <= 0，已退回默认区域".into());
        } else {
            return intersect(r, win);
        }
    }
    match anchor {
        Some((x, y)) => {
            let pad = defaults::EFFECT_REGION_PAD;
            let r = Rect { x: x - pad, y: y - pad, w: pad * 2, h: pad * 2 };
            intersect(r, win)
        }
        None => win,
    }
}

fn intersect(a: Rect, b: Rect) -> Rect {
    if b.is_empty() {
        return a;
    }
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    Rect { x: x0, y: y0, w: (x1 - x0).max(1), h: (y1 - y0).max(1) }
}

fn validate_window_filter(p: &EffectParams) -> Result<(), String> {
    if p.title_contains.is_none() && p.class_name.is_none() && p.pid.is_none() {
        return Err("window 模式必须给 title_contains / class_name / pid 之一".into());
    }
    Ok(())
}

fn find_window(p: &EffectParams) -> Result<Vec<crate::protocol::WinInfo>, String> {
    validate_window_filter(p)?;
    let filter = WindowFilter {
        title_contains: p.title_contains.clone(),
        class_name: p.class_name.clone(),
        pid: p.pid,
        process_name: None,
        visible_only: Some(true),
        top_level_only: Some(true),
    };
    window::list_windows(&filter).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_have_zero_diff() {
        let a = vec![10u8; 1200];
        assert_eq!(diff_ratio(&a, &a), 0.0);
    }

    #[test]
    fn fully_different_images_have_full_diff() {
        let a = vec![0u8; 1200];
        let b = vec![255u8; 1200];
        assert!((diff_ratio(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn small_noise_is_ignored() {
        // 三通道各差 8，和 24 —— 不 > 24，不算变化（压掉合成噪声）。
        let a = vec![100u8; 1200];
        let mut b = a.clone();
        for v in b.iter_mut() {
            *v = 108;
        }
        assert_eq!(diff_ratio(&a, &b), 0.0);
    }

    #[test]
    fn half_changed_is_half() {
        let a = vec![0u8; 1200];
        let mut b = a.clone();
        for i in (0..b.len()).step_by(24) {
            b[i] = 255;
        }
        // 每 24 字节（2 个取样点）改 1 个 → 0.5
        assert!((diff_ratio(&a, &b) - 0.5).abs() < 1e-9, "{}", diff_ratio(&a, &b));
    }

    #[test]
    fn intersect_clamps_to_window() {
        let r = intersect(Rect { x: -100, y: -100, w: 400, h: 400 }, Rect { x: 0, y: 0, w: 200, h: 200 });
        assert_eq!(r, Rect { x: 0, y: 0, w: 200, h: 200 });
    }

    #[test]
    fn intersect_keeps_region_when_window_unknown() {
        let r = Rect { x: 5, y: 6, w: 100, h: 120 };
        assert_eq!(intersect(r, Rect::default()), r);
    }
}
