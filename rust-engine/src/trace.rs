//! 图像 → 路径：把一张线稿/截图里的线条变成可直接画的笔顺。
//!
//! 为什么不让模型看图报坐标：模型只能一个 token 一个点地吐，几百个点既贵又慢还容易画歪
//! （2026-10-24 实测：模型自己组织 300 点画一条弧，耗时是引擎一次调用的上百倍）。
//! 坐标这件事交给程序：二值化 → 细化成骨架 → 沿骨架提笔顺 → 简化 → 等距重采样。
//! 模型只负责"画哪张图、分几笔"，剩下的由这里出。
//!
//! 输出坐标是**图片像素**，配 `win_capture` 拿到的 `shot_id` 可以直接喂给
//! `win_drag_path`（它会在动手前一次性换算成屏幕坐标）。
//!
//! 算法取舍：
//! - **细化用 Zhang-Suen**：4px 粗的笔迹细化成 1px 骨架，避免同一条线抽出两条平行笔顺。
//! - **笔顺按"节点切段"**：骨架里度数 != 2 的像素是端点/交叉点，从它们出发沿链走；
//!   剩下的全度数 2 的连通块就是闭环，单独成笔。
//! - **简化用 Douglas-Peucker**：人手画的线本来就有毛刺，如实复现反而难看；
//!   压掉 1~1.5px 的抖动再重采样，画出来比原图更干净。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TraceParams {
    /// 二值化阈值（0~255）。不给就用 Otsu 自动算。
    pub threshold: Option<u8>,
    /// true = 亮色是墨（黑影上的白线）。默认 false（深色是墨）。
    pub invert: Option<bool>,
    /// 只处理图片内这块区域（图片像素）。
    pub crop: Option<[u32; 4]>,
    /// 丢掉面积小于它的连通块（压掉噪点/签名水印），默认 16。
    pub min_area: Option<u32>,
    /// Douglas-Peucker 容差（图片像素），默认 1.2。0 = 不简化。
    pub smooth: Option<f64>,
    /// 输出笔顺的重采样间距（图片像素），默认 2.0。
    pub step_px: Option<f64>,
    /// 最多输出几笔（按长度从长到短取），默认 64。
    pub max_strokes: Option<usize>,
    /// 首尾距离 < step*1.5 时闭合，默认 true。
    pub close_loops: Option<bool>,
    /// 丢弃长度小于它的笔顺（像素），默认 8。
    /// 作用是清掉骨架上的残渣：Zhang-Suen 在斜边上会留台阶，贪心走法没走到的碎像素
    /// 会变成几个像素的短线，不丢就会画出莫名其妙的毛刺。
    pub min_len_px: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct TracedStroke {
    pub points: Vec<[f64; 2]>,
    pub length_px: f64,
    pub closed: bool,
}

#[derive(Debug, Clone)]
pub struct Traced {
    pub strokes: Vec<TracedStroke>,
    pub image_w: u32,
    pub image_h: u32,
    pub threshold: u8,
    pub ink_px: u32,
    pub skeleton_px: u32,
    pub dropped_blobs: u32,
    pub trace_ms: u64,
    pub warnings: Vec<String>,
}

/// 入口：RGBA 图 → 笔顺。
pub fn trace_rgba(rgba: &[u8], width: u32, height: u32, p: &TraceParams) -> Traced {
    let started = std::time::Instant::now();
    let mut warnings = Vec::new();
    let w = width as usize;
    let h = height as usize;

    // 1) 灰度 + 裁剪
    let (x0, y0, cw, ch) = match p.crop {
        Some([cx, cy, cw, chh]) => {
            let x0 = (cx as usize).min(w);
            let y0 = (cy as usize).min(h);
            let cw = (cw as usize).min(w - x0).max(1);
            let ch = (chh as usize).min(h - y0).max(1);
            (x0, y0, cw, ch)
        }
        None => (0, 0, w, h),
    };
    let mut gray = vec![0u8; cw * ch];
    for y in 0..ch {
        let src = (y0 + y) * w + x0;
        let dst = y * cw;
        for x in 0..cw {
            let o = (src + x) * 4;
            let r = rgba[o] as u32;
            let g = rgba[o + 1] as u32;
            let b = rgba[o + 2] as u32;
            // Rec.601 亮度；整数运算避免浮点开销。
            gray[dst + x] = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
        }
    }

    // 2) 阈值 + 二值化（1 = 墨）
    //
    // 判据用 `<=` 而不是 `<`：Otsu 在"白底 + 少量纯黑线"的图上会算出阈值 0，
    // 而线的像素值正好是 0，用 `<` 会把整条线判成背景（2026-10-24 单测抓出来的）。
    let invert = p.invert.unwrap_or(false);
    let threshold = p.threshold.unwrap_or_else(|| otsu(&gray)).clamp(1, 254);
    let mut mask: Vec<u8> = gray
        .iter()
        .map(|&v| {
            let ink = if invert { v > threshold } else { v <= threshold };
            if ink { 1 } else { 0 }
        })
        .collect();

    // 3) 去小连通块（8 邻接）
    let min_area = p.min_area.unwrap_or(16) as usize;
    let dropped_blobs = if min_area > 1 { remove_small_blobs(&mut mask, cw, ch, min_area) } else { 0 };

    // 4) 细化成 1px 骨架
    let ink_px = mask.iter().filter(|&&v| v == 1).count() as u32;
    thin(&mut mask, cw, ch);
    let skeleton_px = mask.iter().filter(|&&v| v == 1).count() as u32;
    if skeleton_px > 200_000 {
        warnings.push(format!(
            "骨架像素 {skeleton_px} 偏多，线条可能过密：加大 smooth / min_area，或先裁剪区域"
        ));
    }

    // 5) 提笔顺 → 接回断开的段 → 丢掉"贴着别人走的"冗余像素
    let raw = extract_strokes(&mask, cw, ch);
    let raw = merge_strokes(raw, 3.0);
    let mut raw = drop_redundant(raw, 3.0);

    // 6) 简化 + 重采样 + 闭合判定
    let smooth = p.smooth.unwrap_or(1.2).max(0.0);
    let step = p.step_px.unwrap_or(2.0).max(0.5);
    let close_loops = p.close_loops.unwrap_or(true);
    let mut strokes: Vec<TracedStroke> = Vec::new();
    for pts in raw.drain(..) {
        if pts.len() < 2 {
            continue;
        }
        let simplified = if smooth > 0.0 { rdp(&pts, smooth) } else { pts };
        if simplified.len() < 2 {
            continue;
        }
        let mut resampled = crate::path::resample(&simplified, step);
        if resampled.len() < 2 {
            continue;
        }
        let mut closed = false;
        if close_loops && resampled.len() > 3 {
            let a = resampled[0];
            let b = *resampled.last().unwrap();
            if ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() <= step * 1.5 {
                closed = true;
                // 闭合笔顺末点直接对齐首点，避免"差一点点"的接缝。
                *resampled.last_mut().unwrap() = a;
            }
        }
        let length = crate::path::length(&resampled);
        let min_len = p.min_len_px.unwrap_or(8.0).max(0.0);
        if length < min_len {
            continue; // 骨架残渣
        }
        let points: Vec<[f64; 2]> = resampled.iter().map(|p| [p.0, p.1]).collect();
        strokes.push(TracedStroke { points, length_px: length, closed });
    }

    // 7) 长笔画优先（先画主干再补细节），并限量
    strokes.sort_by(|a, b| b.length_px.partial_cmp(&a.length_px).unwrap_or(std::cmp::Ordering::Equal));
    let max_strokes = p.max_strokes.unwrap_or(64).clamp(1, 512);
    if strokes.len() > max_strokes {
        warnings.push(format!("笔顺 {} 笔超过上限 {max_strokes}，只保留最长的 {max_strokes} 笔", strokes.len()));
        strokes.truncate(max_strokes);
    }

    // 坐标换算：加上裁剪偏移，让输出始终是**原图坐标**。
    for s in strokes.iter_mut() {
        for pt in s.points.iter_mut() {
            pt[0] += x0 as f64;
            pt[1] += y0 as f64;
        }
    }

    Traced {
        strokes,
        image_w: width,
        image_h: height,
        threshold,
        ink_px,
        skeleton_px,
        dropped_blobs,
        trace_ms: started.elapsed().as_millis() as u64,
        warnings,
    }
}

/// Otsu 阈值：让前景/背景类间方差最大。
fn otsu(gray: &[u8]) -> u8 {
    let mut hist = [0u32; 256];
    for &v in gray {
        hist[v as usize] += 1;
    }
    let total: u64 = gray.len() as u64;
    if total == 0 {
        return 128;
    }
    let sum_all: u64 = hist.iter().enumerate().map(|(i, &c)| i as u64 * c as u64).sum();
    let mut sum_b = 0u64;
    let mut w_b = 0u64;
    let mut best_var = -1.0f64;
    let mut best = 128u8;
    for t in 0..256usize {
        w_b += hist[t] as u64;
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as u64 * hist[t] as u64;
        let m_b = sum_b as f64 / w_b as f64;
        let m_f = (sum_all - sum_b) as f64 / w_f as f64;
        let var = w_b as f64 * w_f as f64 * (m_b - m_f).powi(2);
        if var > best_var {
            best_var = var;
            best = t as u8;
        }
    }
    best
}

/// 删掉面积小于 `min_area` 的 8 连通块，返回删掉的块数。
fn remove_small_blobs(mask: &mut [u8], w: usize, h: usize, min_area: usize) -> u32 {
    let mut seen = vec![false; mask.len()];
    let mut dropped = 0u32;
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..mask.len() {
        if mask[start] == 0 || seen[start] {
            continue;
        }
        let mut comp = Vec::new();
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            comp.push(i);
            for j in neighbours8(i, w, h) {
                if mask[j] == 1 && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        if comp.len() < min_area {
            for i in comp {
                mask[i] = 0;
            }
            dropped += 1;
        }
    }
    dropped
}

/// 8 邻接下标。
fn neighbours8(i: usize, w: usize, h: usize) -> Vec<usize> {
    let x = (i % w) as i32;
    let y = (i / w) as i32;
    let mut out = Vec::with_capacity(8);
    for dy in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = x + dx;
            let ny = y + dy;
            if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                out.push(ny as usize * w + nx as usize);
            }
        }
    }
    out
}

fn degree_of(mask: &[u8], i: usize, w: usize, h: usize) -> u8 {
    neighbours8(i, w, h).iter().filter(|&&j| mask[j] == 1).count() as u8
}

/// Zhang-Suen 细化。把粗笔迹化成 1px 骨架，去掉"一条线抽成两条平行线"的问题。
fn thin(mask: &mut [u8], w: usize, h: usize) {
    let mut changed = true;
    while changed {
        changed = false;
        for step in 0..2 {
            let mut del: Vec<usize> = Vec::new();
            for y in 1..h.saturating_sub(1) {
                for x in 1..w.saturating_sub(1) {
                    let i = y * w + x;
                    if mask[i] == 0 {
                        continue;
                    }
                    let p2 = mask[i - w] as u8;
                    let p3 = mask[i - w + 1] as u8;
                    let p4 = mask[i + 1] as u8;
                    let p5 = mask[i + w + 1] as u8;
                    let p6 = mask[i + w] as u8;
                    let p7 = mask[i + w - 1] as u8;
                    let p8 = mask[i - 1] as u8;
                    let p9 = mask[i - w - 1] as u8;
                    let b = p2 + p3 + p4 + p5 + p6 + p7 + p8 + p9;
                    if !(2..=6).contains(&b) {
                        continue;
                    }
                    let a = trans(p2, p3)
                        + trans(p3, p4)
                        + trans(p4, p5)
                        + trans(p5, p6)
                        + trans(p6, p7)
                        + trans(p7, p8)
                        + trans(p8, p9)
                        + trans(p9, p2);
                    if a != 1 {
                        continue;
                    }
                    if step == 0 {
                        if p2 * p4 * p6 != 0 || p4 * p6 * p8 != 0 {
                            continue;
                        }
                    } else if p2 * p4 * p8 != 0 || p2 * p6 * p8 != 0 {
                        continue;
                    }
                    del.push(i);
                }
            }
            if !del.is_empty() {
                changed = true;
                for i in del {
                    mask[i] = 0;
                }
            }
        }
    }
}

fn trans(a: u8, b: u8) -> u8 {
    if a == 0 && b == 1 { 1 } else { 0 }
}

/// 丢掉"和更长的笔顺贴在一起"的碎片。
///
/// 典型来源：45° 斜边在细化后留下台阶，贪心走法沿主环走掉之后，剩下的台阶像素连成
/// 一条 14px 的斜线，正好压在圆环拐角上（单测实测：一个圆额外多出 4 条 14px 斜线）。
/// 判据是"有 80% 的点落在更长笔顺的 tol 范围内"，不需要知道它为什么产生的。
fn drop_redundant(mut strokes: Vec<Vec<(f64, f64)>>, tol: f64) -> Vec<Vec<(f64, f64)>> {
    use std::collections::HashSet;
    let span = |s: &Vec<(f64, f64)>| -> f64 {
        // 用点数当长度代理就够了：这里只排序，不需要精确弧长。
        s.len() as f64
    };
    strokes.sort_by(|a, b| span(b).partial_cmp(&span(a)).unwrap_or(std::cmp::Ordering::Equal));
    let cell = tol.max(1.0);
    let key = |p: (f64, f64)| ((p.0 / cell).floor() as i32, (p.1 / cell).floor() as i32);

    let mut kept: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut occupied: HashSet<(i32, i32)> = HashSet::new();
    for s in strokes {
        if !kept.is_empty() {
            let covered = s
                .iter()
                .filter(|p| {
                    let (kx, ky) = key(**p);
                    // 查 3×3 邻域：点在相邻格子里也算贴着
                    (-1..=1).any(|dx| {
                        (-1..=1).any(|dy| occupied.contains(&(kx + dx, ky + dy)))
                    })
                })
                .count();
            if covered * 10 >= s.len() * 8 {
                continue;
            }
        }
        for p in &s {
            occupied.insert(key(*p));
        }
        kept.push(s);
    }
    kept
}

/// 把端点相邻的笔顺接起来。
///
/// 为什么会断开：骨架在斜边上会有台阶，贪心走法在台阶处可能提前收尾，一条圆环被切成
/// 几段弧（单测实测 5 段）。这些段的端点相距 1~2px，直接按"端点距离 ≤ tol 且走向连续"
/// 拼接即可复原，比在细化阶段硬拗更稳，对真实线稿的断线同样有效。
fn merge_strokes(mut strokes: Vec<Vec<(f64, f64)>>, tol: f64) -> Vec<Vec<(f64, f64)>> {
    let dist = |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let mut changed = true;
    while changed {
        changed = false;
        'outer: for i in 0..strokes.len() {
            for j in 0..strokes.len() {
                if i == j || strokes[j].len() < 2 {
                    continue;
                }
                let a_last = *strokes[i].last().unwrap();
                let a_first = strokes[i][0];
                let b_first = strokes[j][0];
                let b_last = *strokes[j].last().unwrap();
                // 四种接法：尾-头 / 尾-尾 / 头-头 / 头-尾
                let (join, reverse_b, at_tail) = if dist(a_last, b_first) <= tol {
                    (true, false, true)
                } else if dist(a_last, b_last) <= tol {
                    (true, true, true)
                } else if dist(a_first, b_first) <= tol {
                    (true, false, false)
                } else if dist(a_first, b_last) <= tol {
                    (true, true, false)
                } else {
                    (false, false, false)
                };
                if !join {
                    continue;
                }
                let mut tail = strokes[j].clone();
                if reverse_b {
                    tail.reverse();
                }
                if at_tail {
                    strokes[i].extend_from_slice(&tail[1.min(tail.len())..]);
                } else {
                    // 接到头部：把 i 接在 tail 后面
                    tail.extend_from_slice(&strokes[i][1.min(strokes[i].len())..]);
                    strokes[i] = tail;
                }
                strokes.remove(j);
                changed = true;
                break 'outer;
            }
        }
    }
    strokes
}

/// 沿骨架提笔顺。
///
/// 用"贪心 + 惯性"走法，而不是"按度数切节点"：Zhang-Suen 出来的骨架在斜边上会留下
/// 台阶（一个像素同时有正交和斜向邻居 → 度数 3），按节点切会把一个圆切成几十段
/// （2026-10-24 单测实测：圆被切成 64 段）。改成每次挑**方向最接近当前前进方向**的
/// 未访问邻居继续走，交叉点自然直穿、环路自然绕一圈成一笔，也更贴近人画线的方式。
fn extract_strokes(mask: &[u8], w: usize, h: usize) -> Vec<Vec<(f64, f64)>> {
    let mut used = vec![false; mask.len()];
    let mut strokes: Vec<Vec<(f64, f64)>> = Vec::new();
    let idx_to_pt = |i: usize| ((i % w) as f64, (i / w) as f64);

    // 度数小的优先当起点（端点最优先），这样开放线不会从中间被劈成两半。
    let mut degrees = vec![0u8; mask.len()];
    let mut order: Vec<usize> = Vec::new();
    for i in 0..mask.len() {
        if mask[i] == 1 {
            degrees[i] = degree_of(mask, i, w, h);
            order.push(i);
        }
    }
    order.sort_by_key(|&i| degrees[i]);

    for &start in &order {
        if used[start] {
            continue;
        }
        used[start] = true;
        let mut path = vec![start];
        let mut cur = start;
        let mut dir: Option<(i32, i32)> = None;
        loop {
            let mut best: Option<(usize, i32, i32)> = None;
            let mut best_score = f64::NEG_INFINITY;
            for k in neighbours8(cur, w, h) {
                if mask[k] == 0 || used[k] {
                    continue;
                }
                let dx = (k % w) as i32 - (cur % w) as i32;
                let dy = (k / w) as i32 - (cur / w) as i32;
                let score = match dir {
                    None => 0.0,
                    Some((px, py)) => {
                        let n = ((dx * dx + dy * dy) as f64).sqrt() * ((px * px + py * py) as f64).sqrt();
                        if n < 1e-9 { 0.0 } else { (dx * px + dy * py) as f64 / n }
                    }
                };
                if score > best_score {
                    best_score = score;
                    best = Some((k, dx, dy));
                }
            }
            match best {
                Some((k, dx, dy)) => {
                    used[k] = true;
                    path.push(k);
                    dir = Some((dx, dy));
                    cur = k;
                }
                None => break,
            }
        }
        if path.len() < 2 {
            continue;
        }
        // 终点和起点相邻 → 视为闭合，补回首点让接缝对齐。
        let first = path[0];
        let last = *path.last().unwrap();
        let adjacent = neighbours8(last, w, h).contains(&first);
        let mut pts: Vec<(f64, f64)> = path.iter().map(|&i| idx_to_pt(i)).collect();
        if adjacent {
            pts.push(idx_to_pt(first));
        }
        strokes.push(pts);
    }
    strokes
}

/// Douglas-Peucker 简化（迭代实现，避免深递归爆栈）。
fn rdp(points: &[(f64, f64)], eps: f64) -> Vec<(f64, f64)> {
    if points.len() < 3 || eps <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let (mut dmax, mut index) = (0.0f64, first);
        for i in (first + 1)..last {
            let d = perpendicular(points[i], points[first], points[last]);
            if d > dmax {
                dmax = d;
                index = i;
            }
        }
        if dmax > eps {
            keep[index] = true;
            stack.push((first, index));
            stack.push((index, last));
        }
    }
    points.iter().enumerate().filter(|(i, _)| keep[*i]).map(|(_, p)| *p).collect()
}

fn perpendicular(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在 RGBA 画布上画一条粗线（简单的整数 Bresenham + 笔宽）。
    fn canvas(w: usize, h: usize) -> Vec<u8> {
        vec![255u8; w * h * 4]
    }

    fn plot(c: &mut [u8], w: usize, x: i32, y: i32, width: i32) {
        for dy in -width..=width {
            for dx in -width..=width {
                let px = x + dx;
                let py = y + dy;
                if px < 0 || py < 0 {
                    continue;
                }
                let i = (py as usize * w + px as usize) * 4;
                if i + 3 < c.len() {
                    c[i] = 0;
                    c[i + 1] = 0;
                    c[i + 2] = 0;
                    c[i + 3] = 255;
                }
            }
        }
    }

    fn line(c: &mut [u8], w: usize, a: (i32, i32), b: (i32, i32), width: i32) {
        let steps = ((b.0 - a.0).abs()).max((b.1 - a.1).abs()).max(1);
        for s in 0..=steps {
            let t = s as f64 / steps as f64;
            let x = (a.0 as f64 + (b.0 - a.0) as f64 * t).round() as i32;
            let y = (a.1 as f64 + (b.1 - a.1) as f64 * t).round() as i32;
            plot(c, w, x, y, width);
        }
    }

    fn gray_from(c: &[u8]) -> (Vec<u8>, u32, u32) {
        let _ = c;
        (vec![], 0, 0)
    }

    #[test]
    fn single_thick_line_becomes_one_stroke() {
        let (w, h) = (200usize, 200usize);
        let mut c = canvas(w, h);
        line(&mut c, w, (20, 100), (180, 100), 2);
        let t = trace_rgba(&c, w as u32, h as u32, &TraceParams::default());
        assert_eq!(t.strokes.len(), 1, "一条线只该出一条笔顺");
        let s = &t.strokes[0];
        assert!(s.length_px > 140.0 && s.length_px < 175.0, "长度 {}", s.length_px);
        // 笔顺应当覆盖整条线
        let xs: Vec<f64> = s.points.iter().map(|p| p[0]).collect();
        let min = xs.iter().cloned().fold(f64::MAX, f64::min);
        let max = xs.iter().cloned().fold(f64::MIN, f64::max);
        assert!(min < 25.0 && max > 175.0, "范围 {min}..{max}");
    }

    #[test]
    fn circle_is_closed_single_stroke() {
        let (w, h) = (200usize, 200usize);
        let mut c = canvas(w, h);
        for a in 0..3600 {
            let t = a as f64 * std::f64::consts::PI / 1800.0;
            let x = 100.0 + 60.0 * t.cos();
            let y = 100.0 + 60.0 * t.sin();
            plot(&mut c, w, x.round() as i32, y.round() as i32, 1);
        }
        let t = trace_rgba(&c, w as u32, h as u32, &TraceParams::default());
        assert_eq!(t.strokes.len(), 1, "圆应当只出一笔，实际 {}", t.strokes.len());
        let s = &t.strokes[0];
        assert!(s.closed, "圆应当被判定为闭合");
        // 周长 ≈ 2πr ≈ 377
        assert!(s.length_px > 330.0 && s.length_px < 420.0, "长度 {}", s.length_px);
    }

    #[test]
    fn small_noise_is_dropped() {
        let (w, h) = (80usize, 80usize);
        let mut c = canvas(w, h);
        line(&mut c, w, (10, 40), (70, 40), 1);
        plot(&mut c, w, 40, 10, 1); // 单个噪点（9px）
        let mut p = TraceParams::default();
        p.min_area = Some(16);
        let t = trace_rgba(&c, w as u32, h as u32, &p);
        assert_eq!(t.dropped_blobs, 1, "噪点应当被丢掉");
        assert_eq!(t.strokes.len(), 1);
    }

    #[test]
    fn output_uses_original_image_coordinates() {
        let (w, h) = (100usize, 100usize);
        let mut c = canvas(w, h);
        line(&mut c, w, (50, 20), (50, 80), 1);
        let mut p = TraceParams::default();
        p.crop = Some([40, 10, 20, 80]);
        let t = trace_rgba(&c, w as u32, h as u32, &p);
        let s = &t.strokes[0];
        let xs: Vec<f64> = s.points.iter().map(|q| q[0]).collect();
        let avg = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((avg - 50.0).abs() < 2.0, "裁剪后坐标应回到原图坐标系，实际均值 {avg}");
    }

    #[test]
    fn rdp_keeps_endpoints_and_corners() {
        let pts = vec![(0.0, 0.0), (10.0, 0.1), (20.0, 0.0), (30.0, 0.0), (40.0, 20.0)];
        let out = rdp(&pts, 1.0);
        assert_eq!(out.first().unwrap(), &(0.0, 0.0));
        assert_eq!(out.last().unwrap(), &(40.0, 20.0));
        assert!(out.len() < pts.len());
        assert!(out.len() >= 3, "至少保留起点、拐点、终点，实际 {}", out.len());
    }

    #[test]
    fn otsu_splits_black_and_white() {
        let mut g = vec![250u8; 500];
        g.extend(vec![10u8; 500]);
        let t = otsu(&g);
        assert!((10..250).contains(&t), "阈值 {t}");
        // 阈值必须能把黑像素判成前景（这里就是 `<=` 与 `<` 的分界线）。
        assert!(10u8 <= t, "阈值 {t} 会把黑像素漏掉");
    }
}
