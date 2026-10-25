//! 路径几何：把「一句话形状」展开成**等距点列**。
//!
//! 为什么单独一层：2026-10-24 真机实测（画图里画同一个圆），同一套注入路径下
//! 6 段直弦肉眼明显折角（拟合 r=118.4，径向误差均值 1.31 / 最大 4.47px），
//! 300 点采样才贴近真圆（r=120.6，1.15 / 3.77px）。折角不是"点不够密"，而是
//! **点必须按弧长均匀分布**：按参数 t 均匀采样的贝塞尔在曲率大处点疏、控制点
//! 附近点挤，落到屏幕上会被笔宽放大成抖动；而且点距不均时，按总时长均分时间
//! 会让笔速忽快忽慢。
//!
//! 所以这里所有形状都先密采样成折线，再按累计弧长重采样成等距点列。
//!
//! 坐标约定：与屏幕一致，**y 轴向下**。角度以度为单位，0° 指向 +x（右），
//! 角度增大在屏幕上表现为**顺时针**（因为 y 翻了过来）。`ccw=true` 时方向反转。
//!
//! 全部是纯函数，不碰 Windows API，方便单测（见文件尾 `tests`）。

/// 密采样时相邻两点的最大角度（度）。0.5° 对半径 1000px 的圆是 ~8.7px 弦，
/// 足够让后续 [`resample`] 的插值误差远小于 1px。
const ARC_DENSE_DEG: f64 = 0.5;

/// 贝塞尔密采样的段数。256 段对 3 阶曲线足够，重采样会再按弧长抹平。
const BEZIER_SAMPLES: usize = 256;

/// 圆弧：圆心 + 半径 + 起止角（度）。
///
/// `start_deg == end_deg` 视为整圆 360°。方向由 `ccw` 决定：
/// - `ccw=false`（默认）：角度增大方向（屏幕上是顺时针）
/// - `ccw=true`：角度减小方向
pub fn arc(cx: f64, cy: f64, r: f64, start_deg: f64, end_deg: f64, ccw: bool) -> Vec<(f64, f64)> {
    let r = r.abs().max(0.5);
    let mut sweep = end_deg - start_deg;
    if sweep == 0.0 {
        sweep = if ccw { -360.0 } else { 360.0 };
    } else if ccw && sweep > 0.0 {
        sweep -= 360.0;
    } else if !ccw && sweep < 0.0 {
        sweep += 360.0;
    }
    let steps = ((sweep.abs() / ARC_DENSE_DEG).ceil() as usize).clamp(2, 4000);
    let mut out = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let ang = (start_deg + sweep * t).to_radians();
        out.push((cx + r * ang.cos(), cy + r * ang.sin()));
    }
    out
}

/// 整圆。`start_deg` 可以给一个起笔角，闭口落在同一个点上。
pub fn circle(cx: f64, cy: f64, r: f64, start_deg: f64, ccw: bool) -> Vec<(f64, f64)> {
    let sweep = if ccw { -360.0 } else { 360.0 };
    arc(cx, cy, r, start_deg, start_deg + sweep, ccw)
}

/// 任意阶（1~4）贝塞尔，de Casteljau 求值，密采样后返回折线。
pub fn bezier(ctrl: &[(f64, f64)]) -> Vec<(f64, f64)> {
    match ctrl.len() {
        0 => return Vec::new(),
        1 => return vec![ctrl[0]],
        _ => {}
    }
    let mut out = Vec::with_capacity(BEZIER_SAMPLES + 1);
    for i in 0..=BEZIER_SAMPLES {
        let t = i as f64 / BEZIER_SAMPLES as f64;
        let mut pts: Vec<(f64, f64)> = ctrl.to_vec();
        while pts.len() > 1 {
            pts = pts
                .windows(2)
                .map(|w| {
                    (
                        w[0].0 + (w[1].0 - w[0].0) * t,
                        w[0].1 + (w[1].1 - w[0].1) * t,
                    )
                })
                .collect();
        }
        out.push(pts[0]);
    }
    out
}

/// 折线总长（像素）。
pub fn length(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum()
}

/// 按累计弧长重采样成等距点列。
///
/// 首点保持原样；每隔 `step` 取一点；**末点精确等于原始终点**（形状闭合靠它，
/// 浮点取整差 1px 在闭合圆上肉眼可见缺口）。
pub fn resample(pts: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    let step = step.max(0.5);
    if pts.len() < 2 {
        return pts.to_vec();
    }
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(pts.len());
    out.push(pts[0]);
    // 距离上一个输出点已经走过的弧长。
    let mut carry = 0.0_f64;
    for w in pts.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        let seg = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        if seg <= f64::EPSILON {
            continue;
        }
        let mut pos = step - carry;
        while pos <= seg {
            let t = pos / seg;
            out.push((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
            pos += step;
        }
        carry = seg - (pos - step);
    }
    let last = *pts.last().unwrap();
    let last_out = *out.last().unwrap();
    let d = ((last.0 - last_out.0).powi(2) + (last.1 - last_out.1).powi(2)).sqrt();
    if d > 0.5 {
        out.push(last);
    } else if d > f64::EPSILON {
        *out.last_mut().unwrap() = last;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn circle_is_closed_and_round() {
        let pts = circle(500.0, 400.0, 120.0, 0.0, false);
        let first = pts.first().unwrap();
        let last = pts.last().unwrap();
        assert!(close(first.0, last.0, 1e-6) && close(first.1, last.1, 1e-6), "圆必须闭合");
        for p in &pts {
            let r = ((p.0 - 500.0).powi(2) + (p.1 - 400.0).powi(2)).sqrt();
            assert!(close(r, 120.0, 1e-6), "半径偏差 {}", r - 120.0);
        }
    }

    #[test]
    fn arc_direction_follows_ccw() {
        // 0° -> 90°，默认方向角度增大：终点在 (+r, +r 方向) 即 (cx+r, cy) 到 (cx, cy+r)。
        let pts = arc(100.0, 100.0, 50.0, 0.0, 90.0, false);
        let last = *pts.last().unwrap();
        assert!(close(last.0, 100.0, 1e-6) && close(last.1, 150.0, 1e-6), "默认方向终点 {:?}", last);
        // 反向：0° 到 90° 逆时针走 270°，终点相同但中途走的是上半圈（y < 圆心）。
        let pts = arc(100.0, 100.0, 50.0, 0.0, 90.0, true);
        let last = *pts.last().unwrap();
        assert!(close(last.0, 100.0, 1e-6) && close(last.1, 150.0, 1e-6), "反向终点 {:?}", last);
        let mid = pts[pts.len() / 2];
        assert!(mid.1 < 80.0, "逆时针应经过上半圈，中点 y={}", mid.1);
    }

    #[test]
    fn bezier_endpoints_exact() {
        let ctrl = [(0.0, 0.0), (30.0, 90.0), (90.0, 90.0), (120.0, 0.0)];
        let pts = bezier(&ctrl);
        assert_eq!(*pts.first().unwrap(), ctrl[0]);
        assert_eq!(*pts.last().unwrap(), ctrl[3]);
        // 曲线中点应高于两端连线（控制点在上方）。
        let mid = pts[pts.len() / 2];
        assert!(mid.1 > 20.0, "中点 y={}", mid.1);
    }

    #[test]
    fn resample_is_evenly_spaced() {
        let pts = vec![(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)];
        let out = resample(&pts, 10.0);
        assert_eq!(*out.first().unwrap(), (0.0, 0.0));
        assert_eq!(*out.last().unwrap(), (100.0, 100.0));
        let mut min = f64::MAX;
        let mut max: f64 = 0.0;
        for w in out.windows(2) {
            let d = ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
            min = min.min(d);
            max = max.max(d);
        }
        assert!(min >= 9.99 && max <= 10.01, "步长范围 {min}..{max}");
        assert!(close(length(&out), 200.0, 1e-6), "总长 {}", length(&out));
    }

    #[test]
    fn resample_degenerate_input_is_safe() {
        assert_eq!(resample(&[], 2.0).len(), 0);
        assert_eq!(resample(&[(1.0, 2.0)], 2.0).len(), 1);
        let same = vec![(5.0, 5.0), (5.0, 5.0), (5.0, 5.0)];
        let out = resample(&same, 2.0);
        assert_eq!(out.len(), 1, "全重合点应只剩一个");
    }
}
