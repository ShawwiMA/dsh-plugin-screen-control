//! 录制：把用户的真实鼠标轨迹录下来，用于"看一遍人手怎么画，然后一模一样重放"。
//!
//! 为什么用轮询而不是低级鼠标钩子（WH_MOUSE_LL）：钩子要自己起消息泵线程，而引擎是
//! 单线程 tokio；更麻烦的是钩子回调里不能做重活、超时会被系统直接摘掉（表现为"录着
//! 录着就断了"）。120Hz 轮询对人手足够（人关节最快也就几十 Hz），且完全不改变系统状态。
//!
//! 录制期间**引擎不服务别的请求**（一条请求一行 NDJSON）：录的时候不该有人同时注入，
//! 否则录到的轨迹会被自己的注入污染。
//!
//! 按键状态用 `GetAsyncKeyState` 而不是监听消息：不依赖窗口焦点，用户点哪个程序都录得到。

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::error::EngineResult;
use crate::protocol::{RecordParams, RecordResult, RecordedStroke};

const VK_LBUTTON: i32 = 0x01;
const DEFAULT_DURATION_MS: u64 = 5_000;
const MAX_DURATION_MS: u64 = 60_000;
const DEFAULT_HZ: u32 = 120;
const DEFAULT_MAX_POINTS: usize = 20_000;
/// 少于这个点数的笔不记录（手抖误触）。
const MIN_STROKE_POINTS: usize = 2;

pub fn record(p: &RecordParams) -> EngineResult<RecordResult> {
    let duration = Duration::from_millis(
        p.duration_ms.unwrap_or(DEFAULT_DURATION_MS).clamp(100, MAX_DURATION_MS),
    );
    let hz = p.hz.unwrap_or(DEFAULT_HZ).clamp(20, 500);
    let interval = Duration::from_micros(1_000_000 / hz as u64);
    let max_points = p.max_points.unwrap_or(DEFAULT_MAX_POINTS).min(200_000);

    let started = Instant::now();
    let mut strokes: Vec<RecordedStroke> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut samples: u32 = 0;
    let mut cur: Option<RecordedStroke> = None;
    let mut hit_limit = false;
    // max_points 是**整次录制**的上限，不是单笔：旧实现只比当前笔的长度，
    // 多笔时总点数可以远超上限（60s × 500Hz 能到 3 万），下游回放要按这个规模备内存。
    let mut total_pts: usize = 0;

    while started.elapsed() < duration {
        if crate::cancel::is_cancelled() {
            return Err(crate::cancel::cancelled("win_record"));
        }
        let tick_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: pt 是栈上已初始化的 POINT；GetAsyncKeyState 只读键盘状态。
        let pos_ok = unsafe { GetCursorPos(&mut pt) } != 0;
        let down = (unsafe { GetAsyncKeyState(VK_LBUTTON) } as u16 & 0x8000) != 0;

        if pos_ok {
            samples += 1;
            let now = [pt.x as f64, pt.y as f64];
            match (&mut cur, down) {
                (None, true) => {
                    cur = Some(RecordedStroke { points: vec![now], times_ms: vec![tick_ms] });
                    total_pts += 1;
                }
                (Some(s), true) => {
                    let last = *s.points.last().unwrap();
                    if (last[0] - now[0]).abs() > 0.01 || (last[1] - now[1]).abs() > 0.01 {
                        s.points.push(now);
                        s.times_ms.push(tick_ms);
                        total_pts += 1;
                    }
                }
                (Some(_), false) => {
                    if let Some(s) = cur.take() {
                        if s.points.len() >= MIN_STROKE_POINTS {
                            strokes.push(s);
                        }
                    }
                }
                (None, false) => {}
            }
            if total_pts >= max_points {
                hit_limit = true;
                break;
            }
        }
        std::thread::sleep(interval);
    }
    // 录到时间上限时按键可能还按着：这一笔也要留下，别丢。
    if let Some(s) = cur.take() {
        if s.points.len() >= MIN_STROKE_POINTS {
            strokes.push(s);
        }
    }

    if hit_limit {
        warnings.push(format!("采样总点数达到上限 {max_points}（整次录制），提前结束"));
    }
    if strokes.is_empty() {
        warnings.push(
            "没有录到任何按住左键的轨迹：录制期间要真的按住鼠标画画/拖动（不按键的移动不算一笔）"
                .to_string(),
        );
    }

    // 把时间轴平移到"第一个点 = 0ms"：录过头的等待时间不该让重放先干等。
    if let Some(first_t) = strokes.first().and_then(|s| s.times_ms.first()).copied() {
        for s in strokes.iter_mut() {
            for t in s.times_ms.iter_mut() {
                *t -= first_t;
            }
        }
    }

    let total_px: f64 = strokes
        .iter()
        .map(|s| crate::path::length(&s.points.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>()))
        .sum();

    Ok(RecordResult {
        duration_ms: started.elapsed().as_millis() as u64,
        samples,
        hz,
        strokes,
        total_px,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_sane() {
        assert!(DEFAULT_HZ >= 60, "默认采样率太低，录不出顺滑轨迹");
        assert!(MAX_DURATION_MS >= 60_000, "上限至少要支持一分钟");
        assert!(DEFAULT_MAX_POINTS > 1_000, "上限太小，正常一笔就会顶到");
    }

    #[test]
    fn time_shift_makes_first_point_zero() {
        let mut strokes = vec![
            RecordedStroke { points: vec![[1.0, 2.0], [3.0, 4.0]], times_ms: vec![500.0, 520.0] },
            RecordedStroke { points: vec![[5.0, 6.0]], times_ms: vec![900.0] },
        ];
        if let Some(first_t) = strokes.first().and_then(|s| s.times_ms.first()).copied() {
            for s in strokes.iter_mut() {
                for t in s.times_ms.iter_mut() {
                    *t -= first_t;
                }
            }
        }
        assert_eq!(strokes[0].times_ms, vec![0.0, 20.0]);
        assert_eq!(strokes[1].times_ms, vec![400.0]);
    }
}
