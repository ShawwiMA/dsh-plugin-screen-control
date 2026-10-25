//! `win_find`：一次性屏幕定位（模板匹配，不轮询）。
//!
//! 与 `win_wait(condition="image")` 的分工：
//! - `win_find` = "现在这一帧里它在哪、有多像"，**只搜一次**，立即返回；
//! - `win_wait` = "等它出现"，带 deadline 轮询。
//! 两者共用 `wait::Matcher` 的加载/匹配实现（分数语义、缩放规则、后端选择完全一致），
//! 不各写一份——两份实现迟早会在"为什么 wait 能等到、find 找不到"上跑偏。
//!
//! 坐标约定：模板匹配是在**截图局部坐标**里做的，对外返回屏幕物理像素，
//! 换算规则就是 `origin + 匹配偏移`，origin 的取值与 `win_capture`/`win_ocr` 完全一致：
//! region > 窗口矩形 > 虚拟桌面。

use std::time::Instant;

use windows_sys::Win32::Foundation::HWND;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{CaptureBackend, FindParams, FindResult, Point, Rect, defaults};
use crate::wait::{Matcher, to_gray};
use crate::{capture, dpi, window};

/// 执行一次模板匹配。`found=false` 不是错误：它是正常结果，best 候选位置/得分照样返回。
pub fn find(p: &FindParams) -> EngineResult<FindResult> {
    let started = Instant::now();
    let mut warnings: Vec<String> = Vec::new();

    // 模板先加载：文件不存在/解码失败要在截图之前报，省一次整屏截图。
    let matcher = Matcher::load(&p.template, p.scale.unwrap_or(1.0))?;

    // 阈值也要先校验：0..=1 之外的配置错误如果静默成"永远找不到"，是最难查的那类。
    let threshold = match p.min_score {
        Some(v) if !(0.0..=1.0).contains(&v) => {
            return Err(EngineError::invalid(format!(
                "min_score 必须在 0..=1 之间，收到 {v}"
            )));
        }
        Some(v) => v,
        None => defaults::WAIT_MIN_SCORE,
    };

    let (hwnd, region) = resolve_region(p.hwnd, p.region, &mut warnings)?;
    if matcher.tpl.w > region.w as u32 || matcher.tpl.h > region.h as u32 {
        return Err(EngineError::invalid(format!(
            "模板 {}x{} 比搜索区域 {}x{} 还大：模板必须是区域里的一小块（先用 win_capture 截同一块再裁）",
            matcher.tpl.w, matcher.tpl.h, region.w, region.h
        )));
    }

    // 后端与 win_wait(image) 完全一致：Auto = PrintWindow 优先、全黑回退 BitBlt。
    // 不一致的话，同一个模板"wait 能等到、find 找不到"会变成最费解的一类 bug。
    let raw = capture::capture(hwnd, Some(region), CaptureBackend::Auto)?;
    let gray = to_gray(&raw.rgba);
    let Some(found) = matcher.best(&gray, raw.width, raw.height)? else {
        return Err(EngineError::other(
            "模板匹配没有产生候选位置（模板比这一帧截图还大？）",
        ));
    };

    // Found 只给中心点；左上角可以精确还原（best() 里加的就是 w/2、h/2 的整数除）。
    let offset_x = found.cx - matcher.tpl.w as i32 / 2;
    let offset_y = found.cy - matcher.tpl.h as i32 / 2;
    let x = region.x + offset_x;
    let y = region.y + offset_y;
    let w = matcher.tpl.w;
    let h = matcher.tpl.h;

    Ok(FindResult {
        found: found.score >= threshold,
        score: found.score,
        x,
        y,
        w,
        h,
        cx: x + w as i32 / 2,
        cy: y + h as i32 / 2,
        origin: Point { x: region.x, y: region.y },
        searched_region: region,
        elapsed_ms: started.elapsed().as_millis() as u64,
        warnings,
    })
}

/// 解析搜索区域：region > 窗口矩形 > 虚拟桌面。
///
/// 与 `win_capture` 的差别只有一点：region 越界时这里**求交并给 warning**，
/// 而不是直接报错——"给了一个稍微出界的区域"是常见调用方式，直接失败太苛刻；
/// 但调用方必须知道实际搜的是哪块，所以裁剪写进 warnings。交完为空才算参数错。
fn resolve_region(
    hwnd_raw: Option<u64>,
    region: Option<Rect>,
    warnings: &mut Vec<String>,
) -> EngineResult<(Option<HWND>, Rect)> {
    let hwnd = match hwnd_raw {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };
    let base = match hwnd {
        Some(h) => dpi::window_rect(h)?,
        None => dpi::virtual_screen_rect(),
    };
    if base.is_empty() {
        return Err(EngineError::invalid(
            "捕获区域为空（宽或高 <= 0）：窗口可能已最小化，或虚拟桌面取不到",
        ));
    }

    let Some(requested) = region else {
        return Ok((hwnd, base));
    };
    let clamped = window::intersect(base, requested);
    if clamped.is_empty() {
        return Err(EngineError::invalid(format!(
            "region ({},{},{},{}) 完全落在捕获区域 ({},{},{},{}) 之外",
            requested.x, requested.y, requested.w, requested.h, base.x, base.y, base.w, base.h
        )));
    }
    if clamped != requested {
        warnings.push(format!(
            "region ({},{},{},{}) 越界，已裁剪为 ({},{},{},{})",
            requested.x, requested.y, requested.w, requested.h, clamped.x, clamped.y, clamped.w,
            clamped.h
        ));
    }
    Ok((hwnd, clamped))
}

// ---------------------------------------------------------------------------
// 离线单元测试
// ---------------------------------------------------------------------------
//
// 只测参数校验与错误分流（不碰真实屏幕内容）：实时屏幕的像素会变，拿它断言
// "找到/找不到"会变成随机红灯。真实命中/未命中的端到端验证放在
// `scripts/acceptance.mjs`（用靶窗自己控制的画面）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn params(v: serde_json::Value) -> FindParams {
        serde_json::from_value(v).expect("测试参数应当能反序列化")
    }

    /// 写一张确定性的临时 PNG（40x30），文件名带 tag 避免测试并行时互相删文件。
    fn write_png(tag: &str) -> String {
        let mut img = image::RgbaImage::new(40, 30);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let path = std::env::temp_dir().join(format!("dsh-find-test-{tag}-40x30.png"));
        img.save(&path).expect("写临时 PNG 失败");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn missing_template_is_not_found() {
        let e = find(&params(serde_json::json!({
            "template": "C:/definitely/not/here/dsh-find-template.png"
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::NotFound, "模板文件不存在应当报 NOT_FOUND: {e}");
    }

    #[test]
    fn undecodable_template_is_invalid_args() {
        let path = std::env::temp_dir().join("dsh-find-test-not-an-image.png");
        std::fs::write(&path, b"this is definitely not a PNG").expect("写临时文件失败");
        let e = find(&params(serde_json::json!({
            "template": path.to_string_lossy()
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs, "解码失败应当是 INVALID_ARGS: {e}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn min_score_out_of_range_is_rejected_before_capture() {
        let png = write_png("bad-score");
        for bad in [-0.1, 1.5] {
            let e = find(&params(serde_json::json!({
                "template": png, "min_score": bad
            })))
            .unwrap_err();
            assert_eq!(e.code(), ErrorCode::InvalidArgs, "min_score={bad} 应当被拒: {e}");
            assert!(e.to_string().contains("min_score"), "{e}");
        }
        let _ = std::fs::remove_file(&png);
    }

    #[test]
    fn template_bigger_than_region_is_invalid_args() {
        let png = write_png("big-template"); // 40x30
        let e = find(&params(serde_json::json!({
            "template": png,
            "region": {"x": 0, "y": 0, "w": 10, "h": 10}
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs, "{e}");
        assert!(e.to_string().contains("比搜索区域"), "{e}");
        let _ = std::fs::remove_file(&png);
    }

    #[test]
    fn region_outside_capture_area_is_invalid_args() {
        let png = write_png("outside");
        let e = find(&params(serde_json::json!({
            "template": png,
            "region": {"x": 100000, "y": 100000, "w": 40, "h": 30}
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs, "{e}");
        assert!(e.to_string().contains("完全落在捕获区域"), "{e}");
        let _ = std::fs::remove_file(&png);
    }
}
