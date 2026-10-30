//! `win_wait`：条件等待。三种条件，**没有**纯 sleep。
//!
//! - `window`：轮询窗口出现（能用 `FindWindowW` 就用，用不了退回 `EnumWindows` 过滤）。
//! - `image`：在指定区域里轮询做模板匹配（纯 Rust 实现，见 [`Matcher`]）。
//! - `text`：轮询截图 + Windows.Media.Ocr，在识别文本里找子串。
//!
//! 三条铁律：
//!
//! 1. **永远带 deadline**。超时返回 `matched=false, timed_out=true`，不抛错、不死等——
//!    "没等到"是正常业务结果，调用方拿它做分支，而不是当成失败去重试。
//! 2. **参数错要立刻报**，不能等 10 秒超时了才说"参数不合法"。
//! 3. **超时要能解释**。轮询期间的失败原因（截图失败 / OCR 不可用）写进 `last_error`，
//!    模板匹配没到阈值时把"最像的那一次得分"写进 `score` 并给 warning——
//!    否则"没等到"和"一直在报错所以没等到"看起来一模一样。

use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{
    CaptureBackend, Point, Rect, WaitCondition, WaitParams, WaitResult, WinInfo, WindowFilter,
    defaults,
};
use crate::{capture, dpi, ocr, ocr_worker, window};

/// 图片文件字节上限：再大的"模板"不可能是屏幕上一小块。
const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 解码后的像素数上限（1 亿像素）。超出直接拒绝，不解码进内存。
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

pub fn wait(p: &WaitParams) -> EngineResult<WaitResult> {
    let timeout_ms = p
        .timeout_ms
        .unwrap_or(defaults::WAIT_TIMEOUT_MS)
        .min(defaults::WAIT_MAX_TIMEOUT_MS);
    let interval = Duration::from_millis(p.interval_ms.unwrap_or(defaults::WAIT_INTERVAL_MS).max(20));

    // 参数先校验：等 10 秒之后再说"你参数写错了"是最招人烦的失败方式。
    let mut warnings: Vec<String> = Vec::new();
    let mut matcher: Option<Matcher> = None;
    match p.condition {
        WaitCondition::Window => {
            if p.title_contains.is_none() && p.class_name.is_none() && p.pid.is_none() {
                return Err(EngineError::invalid(
                    "wait_for_window 至少要给 title_contains / class_name / pid 之一，否则等于等所有窗口",
                ));
            }
            if p.timeout_ms.is_some() && timeout_ms != p.timeout_ms.unwrap_or(0) {
                warnings.push(format!(
                    "timeout_ms 被夹到上限 {}ms（请求 {}ms）",
                    defaults::WAIT_MAX_TIMEOUT_MS,
                    p.timeout_ms.unwrap_or(0)
                ));
            }
        }
        WaitCondition::Image => {
            let path = p
                .template
                .as_deref()
                .ok_or_else(|| EngineError::invalid("wait_for_image 必须给 template（模板 PNG 路径）"))?;
            matcher = Some(Matcher::load(path, p.template_scale.unwrap_or(1.0))?);
            let (_, region) = target_region(p)?;
            let m = matcher.as_ref().unwrap();
            if m.tpl.w > region.w as u32 || m.tpl.h > region.h as u32 {
                return Err(EngineError::invalid(format!(
                    "模板 {}x{} 比等待区域 {}x{} 还大：模板必须是区域里的一小块",
                    m.tpl.w, m.tpl.h, region.w, region.h
                )));
            }
        }
        WaitCondition::Text => {
            if p.text.as_deref().unwrap_or("").is_empty() {
                return Err(EngineError::invalid("wait_for_text 必须给非空的 text"));
            }
            if !ocr::COMPILED {
                // 没有 OCR 就没有这条能力，直接说不支持，别让它空转到超时。
                return Err(EngineError::unsupported(
                    "本构建没有编译 Windows OCR（--features ocr，默认开启），wait_for_text 不可用；\
                     请改用模型自己的视觉能力看图，或等一张图用 wait_for_image。",
                ));
            }
        }
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut polls: u32 = 0;
    let mut last_error: Option<String> = None;
    let mut best_score: Option<f64> = None;
    let mut hit: Option<Hit> = None;

    loop {
        polls += 1;
        // 取消位：调用方 aborted 时控制管道会写进来，这里停在轮询边界退出。
        if crate::cancel::is_cancelled() {
            return Err(crate::cancel::cancelled("win_wait"));
        }
        match poll_once(p, matcher.as_mut(), &mut best_score) {
            Ok(Some(found)) => {
                hit = Some(found);
                break;
            }
            Ok(None) => {}
            Err(e) => last_error = Some(e.to_string()),
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // 睡到"间隔"和"剩余时间"里更小的那个：最后一段不会白睡过头。
        std::thread::sleep(interval.min(deadline - now));
    }

    let waited_ms = timeout_ms.saturating_sub(
        deadline
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    );
    let matched = hit.is_some();
    let hit = hit.unwrap_or_default();

    if !matched {
        if p.condition == WaitCondition::Image {
            if let Some(score) = best_score {
                let threshold = p.min_score.unwrap_or(defaults::WAIT_MIN_SCORE);
                warnings.push(format!(
                    "最像的一次得分 {score:.4}，低于阈值 {threshold:.4}：\
                     模板可能不是同一后端/同一缩放截出来的图（win_capture 默认会把长边缩到 1600）"
                ));
            }
        }
        if let Some(err) = &last_error {
            warnings.push(format!("轮询期间最后一次失败：{err}"));
        }
    }

    Ok(WaitResult {
        condition: p.condition.as_str().to_string(),
        matched,
        timed_out: !matched,
        waited_ms,
        polls,
        timeout_ms,
        window: hit.window,
        match_point: hit.match_point,
        score: hit.score.or(best_score),
        matched_text: hit.matched_text,
        ocr_text: hit.ocr_text,
        last_error,
        warnings,
    })
}

/// 一次轮询命中后带回来的东西。字段全是 Option：三种条件各填各的。
#[derive(Default)]
struct Hit {
    window: Option<WinInfo>,
    match_point: Option<Point>,
    score: Option<f64>,
    matched_text: Option<String>,
    ocr_text: Option<String>,
}

fn poll_once(
    p: &WaitParams,
    matcher: Option<&mut Matcher>,
    best_score: &mut Option<f64>,
) -> EngineResult<Option<Hit>> {
    match p.condition {
        WaitCondition::Window => poll_window(p),
        WaitCondition::Image => poll_image(p, matcher.expect("image 条件下 matcher 必然已就绪"), best_score),
        WaitCondition::Text => poll_text(p),
    }
}

// ---------------------------------------------------------------------------
// wait_for_window
// ---------------------------------------------------------------------------

fn poll_window(p: &WaitParams) -> EngineResult<Option<Hit>> {
    let filter = build_filter(p);

    // FindWindowW 快速路径：类名 + 标题都给全了的时候直接问系统，省一次 EnumWindows。
    // 结果仍要用过滤器复核一遍——FindWindowW 是精确匹配，而调用方的
    // title_contains 是子串语义，两者不是一回事，不能直接信它的返回值。
    if let (Some(class), Some(title)) = (p.class_name.as_deref(), p.title_contains.as_deref()) {
        let cls = wide(class);
        let ttl = wide(title);
        // SAFETY: 两个 PCWSTR 都指向本函数内已 NUL 结尾的缓冲，生命周期覆盖本次调用。
        let hwnd = unsafe { FindWindowW(cls.as_ptr(), ttl.as_ptr()) };
        if !hwnd.is_null() {
            let target = window::to_u64(hwnd);
            if let Some(info) = window::list_windows(&filter)?.into_iter().find(|w| w.hwnd == target) {
                return Ok(Some(Hit { window: Some(info), ..Default::default() }));
            }
        }
    }

    let found = window::list_windows(&filter)?.into_iter().next();
    Ok(found.map(|info| Hit { window: Some(info), ..Default::default() }))
}

fn build_filter(p: &WaitParams) -> WindowFilter {
    WindowFilter {
        title_contains: p.title_contains.clone(),
        class_name: p.class_name.clone(),
        pid: p.pid,
        // win_wait 只认 标题/类名/PID 三种条件，这里**故意**不接进程名过滤：
        // 等窗口时 PID 已经从 tasklist 拿到了，够用；按进程名列窗口是 win_list_windows 的活，
        // 同一个语义在两边各维护一份迟早会跑偏。
        process_name: None,
        // 默认只认可见窗口：隐藏的"幽灵窗口"出现不算出现。
        visible_only: Some(p.visible_only.unwrap_or(true)),
        top_level_only: Some(true),
    }
}

// ---------------------------------------------------------------------------
// wait_for_image
// ---------------------------------------------------------------------------

fn poll_image(
    p: &WaitParams,
    matcher: &mut Matcher,
    best_score: &mut Option<f64>,
) -> EngineResult<Option<Hit>> {
    let (hwnd, region) = target_region(p)?;
    // Auto = 先 PrintWindow 再回退 Bitblt。为什么不用纯 Bitblt：GPU 渲染的窗口
    // （Chromium/Electron/微信 4.0）用窗口 DC 做 BitBlt 会得到一张纯黑图
    // （2026-10-16 实测：截 DSH 的 Electron 窗口整幅 stddev=0），模板匹配直接失效。
    // PrintWindow(PW_RENDERFULLCONTENT) 拿得到这类窗口的真实内容，被遮挡时也有效。
    let raw = capture::capture(hwnd, Some(region), CaptureBackend::Auto)?;
    let gray = to_gray(&raw.rgba);
    let found = match matcher.best(&gray, raw.width, raw.height)? {
        Some(f) => f,
        None => return Ok(None),
    };
    let keep_best = best_score.map(|b| found.score > b).unwrap_or(true);
    if keep_best {
        *best_score = Some(found.score);
    }
    let threshold = p.min_score.unwrap_or(matcher.min_score);
    if found.score < threshold {
        return Ok(None);
    }
    Ok(Some(Hit {
        match_point: Some(Point { x: region.x + found.cx, y: region.y + found.cy }),
        score: Some(found.score),
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// wait_for_text
// ---------------------------------------------------------------------------

fn poll_text(p: &WaitParams) -> EngineResult<Option<Hit>> {
    let (hwnd, region) = target_region(p)?;
    let needle = p.text.clone().unwrap_or_default();
    let case_sensitive = p.case_sensitive.unwrap_or(false);

    // 和 win_ocr 一样走隔离子进程：wait(text) 是轮询，每次都在引擎进程里跑整屏
    // WinRT OCR，只要有一次崩掉整个引擎就没了。
    let outcome = ocr_worker::capture_and_recognize_isolated(
        hwnd.map(window::to_u64),
        Some(region),
        p.language.as_deref(),
        false,
        ocr_worker::DEFAULT_TIMEOUT_MS,
    )?;
    let haystack = if case_sensitive { outcome.text.clone() } else { outcome.text.to_lowercase() };
    let needle_cmp = if case_sensitive { needle.clone() } else { needle.to_lowercase() };

    if haystack.contains(&needle_cmp) {
        return Ok(Some(Hit {
            matched_text: Some(needle),
            ocr_text: Some(truncate(&outcome.text, 2000)),
            ..Default::default()
        }));
    }
    // 没命中时把"这一轮到底识别到了什么"当成本轮失败原因带出去：超时结果里
    // 就能看到 OCR 的实际输出，而不是一句干巴巴的"没等到"。
    Err(EngineError::other(format!(
        "OCR 文本里没有 {:?}；本轮识别到 {} 个字符：{}",
        needle,
        outcome.text.chars().count(),
        truncate(outcome.text.trim(), 300)
    )))
}

// ---------------------------------------------------------------------------
// 区域解析
// ---------------------------------------------------------------------------

/// 解析等待区域，规则与 `win_capture` 完全一致：region > 窗口矩形 > 虚拟桌面。
fn target_region(p: &WaitParams) -> EngineResult<(Option<HWND>, Rect)> {
    let hwnd = match p.hwnd {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };
    let region = match (p.region, hwnd) {
        (Some(r), Some(h)) => capture::clamp_region_to_window(h, r)?,
        (Some(r), None) => window::intersect(dpi::virtual_screen_rect(), r),
        (None, Some(h)) => dpi::window_rect(h)?,
        (None, None) => dpi::virtual_screen_rect(),
    };
    if region.is_empty() {
        return Err(EngineError::invalid(
            "等待区域为空（宽或高 <= 0）：region 给错了，或者窗口已最小化",
        ));
    }
    Ok((hwnd, region))
}

// ---------------------------------------------------------------------------
// 模板匹配
// ---------------------------------------------------------------------------

/// 灰度模板。
///
/// `pub(crate)`：`win_find`（find.rs）复用同一套模板加载/匹配，不另写一份——
/// 两份实现迟早会在分数语义或缩放规则上跑偏。
pub(crate) struct Template {
    pub(crate) gray: Vec<u8>,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// 一次匹配的最优解（相对搜索区域左上角）。
///
/// 只记中心点：`win_wait` 要的是中心点，`win_find` 要的左上角可以由
/// `cx - w/2`、`cy - h/2` 精确还原（`best()` 里就是这么加出来的）。
pub(crate) struct Found {
    /// 命中位置的模板中心（相对搜索区域左上角）。
    pub(crate) cx: i32,
    pub(crate) cy: i32,
    pub(crate) score: f64,
}

/// 模板匹配器。
///
/// 为什么不用现成的图像库：这条路径只要"一个模板在整块图里找最像的位置"，
/// 引 imageproc/openCV 这类库不划算。这里实现的是最直接的 SSD（差方和）
/// 暴力搜索 + 提前剪枝 + 自适应降采样，屏幕截图这种"同管线、同缩放"的场景
/// 匹配质量足够（完全相同的区域得分就是 1.0）。
///
/// 得分定义：`score = 1 - RMSE/255`，1.0 = 逐像素完全相同。
pub(crate) struct Matcher {
    pub(crate) tpl: Template,
    pub(crate) min_score: f64,
}

impl Matcher {
    pub(crate) fn load(path: &str, scale: f64) -> EngineResult<Self> {
        let p = Path::new(path);
        if !p.is_file() {
            return Err(EngineError::not_found(format!("模板文件不存在或不是文件: {path}")));
        }
        // 先卡文件大小再解码：解码一张 500MP 的 PNG 会把内存和 CPU 都吃满，
        // 而模板匹配本来就不需要这么大的图（屏幕区域再大也就几百万像素）。
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        if size > MAX_IMAGE_FILE_BYTES {
            return Err(EngineError::invalid(format!(
                "模板文件 {size} 字节，超过 {} 字节上限",
                MAX_IMAGE_FILE_BYTES
            )));
        }
        let img = image::open(p).map_err(|e| {
            EngineError::invalid(format!("模板图片解码失败（{path}）：{e}（只支持 PNG 等常见格式）"))
        })?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 {
            return Err(EngineError::invalid("模板图片尺寸为 0"));
        }
        if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
            return Err(EngineError::invalid(format!(
                "模板图片 {w}x{h} 超过 {MAX_IMAGE_PIXELS} 像素上限",
            )));
        }
        let gray = to_gray(rgba.as_raw());
        let tpl = if (scale - 1.0).abs() > f64::EPSILON {
            if !(0.05..=20.0).contains(&scale) {
                return Err(EngineError::invalid(format!(
                    "template_scale 必须在 0.05..=20 之间，收到 {scale}"
                )));
            }
            let nw = ((w as f64 * scale).round() as u32).max(1);
            let nh = ((h as f64 * scale).round() as u32).max(1);
            let resized = resize_nearest(&gray, w, h, nw, nh);
            Template { gray: resized, w: nw, h: nh }
        } else {
            Template { gray, w, h }
        };
        Ok(Self { tpl, min_score: defaults::WAIT_MIN_SCORE })
    }

    /// 在 `search`（灰度、`sw` x `sh`）里找模板最像的位置。返回**最优解**，
    /// 不在这里判阈值——调用方需要知道"差多少"，超时结果里才有可解释的分数。
    ///
    /// 两阶段：粗搜（降采样）只用来**挑候选点**，最终分数必须回到原分辨率上算。
    /// 为什么不能直接用降采样的分数：模板和搜索图是**各自独立**做块平均的，
    /// 模板裁切位置不是 k 的整数倍时两边的块网格错开，一个逐像素相同的模板也只能
    /// 拿到 0.83 分（2026-10-16 实测：同一张图裁出来的模板自比得分 0.826，
    /// 而 Python 全分辨率暴力搜同一点是 1.0000）。
    pub(crate) fn best(&self, search: &[u8], sw: u32, sh: u32) -> EngineResult<Option<Found>> {
        if self.tpl.w > sw || self.tpl.h > sh {
            return Err(EngineError::invalid(format!(
                "模板 {}x{} 比这一轮截到的区域 {sw}x{sh} 还大（窗口可能缩小了/被裁了）",
                self.tpl.w, self.tpl.h
            )));
        }

        let k = self.choose_k(sw, sh);
        let n = (self.tpl.w as u64) * (self.tpl.h as u64);

        // k == 1：不降采样，直接全图精确搜索（工作量已由 choose_k 的阈值兜住）。
        if k == 1 {
            let Some((ssd, x, y)) = self.scan_full(search, sw, sh) else {
                return Ok(None);
            };
            return Ok(Some(Found {
                cx: x as i32 + self.tpl.w as i32 / 2,
                cy: y as i32 + self.tpl.h as i32 / 2,
                score: score_of(ssd, n),
            }));
        }

        // 阶段一：粗搜，挑出前 24 个候选（粗坐标）。
        let (s_img, s_w, s_h) = downscale_avg(search, sw, sh, k);
        let (t_img, t_w, t_h) = downscale_avg(&self.tpl.gray, self.tpl.w, self.tpl.h, k);
        if t_w > s_w || t_h > s_h {
            return Ok(None);
        }
        let mut coarse: Vec<(u64, u32, u32)> = Vec::new();
        for y in 0..=(s_h - t_h) {
            for x in 0..=(s_w - t_w) {
                let ssd = ssd_at(&s_img, s_w, &t_img, t_w, t_h, x, y, u64::MAX);
                coarse.push((ssd, x, y));
            }
        }
        if coarse.is_empty() {
            return Ok(None);
        }
        coarse.sort_unstable();

        // 阶段二：每个候选周围 ±k 像素内做原分辨率精算，取真正最优。
        // 块平均的相位误差最大 k-1 像素，±k 一定覆盖得到。
        let mut best: Option<(u64, u32, u32)> = None;
        let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        for (_, cx, cy) in coarse.into_iter().take(24) {
            let fx = (cx * k) as i32;
            let fy = (cy * k) as i32;
            for dy in -(k as i32)..=(k as i32) {
                for dx in -(k as i32)..=(k as i32) {
                    let x = fx + dx;
                    let y = fy + dy;
                    if x < 0 || y < 0 {
                        continue;
                    }
                    let (x, y) = (x as u32, y as u32);
                    if x + self.tpl.w > sw || y + self.tpl.h > sh {
                        continue;
                    }
                    if !seen.insert((x, y)) {
                        continue;
                    }
                    let cut = best.map(|(b, _, _)| b).unwrap_or(u64::MAX);
                    let ssd = ssd_at(search, sw, &self.tpl.gray, self.tpl.w, self.tpl.h, x, y, cut);
                    if best.map(|(b, _, _)| ssd < b).unwrap_or(true) {
                        best = Some((ssd, x, y));
                    }
                }
            }
        }

        let Some((ssd, x, y)) = best else {
            return Ok(None);
        };
        Ok(Some(Found {
            cx: x as i32 + self.tpl.w as i32 / 2,
            cy: y as i32 + self.tpl.h as i32 / 2,
            score: score_of(ssd, n),
        }))
    }

    /// 全图精确搜索，返回 (ssd, x, y)。带"超过当前最优就放弃"的剪枝。
    fn scan_full(&self, search: &[u8], sw: u32, sh: u32) -> Option<(u64, u32, u32)> {
        let mut best: Option<(u64, u32, u32)> = None;
        for y in 0..=sh.saturating_sub(self.tpl.h) {
            for x in 0..=sw.saturating_sub(self.tpl.w) {
                let limit = best.map(|(b, _, _)| b).unwrap_or(u64::MAX);
                let ssd = ssd_at(search, sw, &self.tpl.gray, self.tpl.w, self.tpl.h, x, y, limit);
                if best.map(|(b, _, _)| ssd < b).unwrap_or(true) {
                    best = Some((ssd, x, y));
                }
            }
        }
        best
    }

    /// 自适应降采样因子：让"候选数 x 模板像素"不超过 3e7，同时保证粗模板至少 6px。
    fn choose_k(&self, sw: u32, sh: u32) -> u32 {
        let mut k: u32 = 1;
        while k < 8 {
            let cand = (sw / k).max(1) as u64 * (sh / k).max(1) as u64;
            let tplpx = (self.tpl.w / k).max(1) as u64 * (self.tpl.h / k).max(1) as u64;
            let too_small = self.tpl.w / k < 6 || self.tpl.h / k < 6;
            if cand * tplpx <= 30_000_000 || too_small {
                break;
            }
            k += 1;
        }
        k
    }
}

/// 一点处的 SSD，`cut` 是提前退出阈值（超过就不必算完）。
fn ssd_at(search: &[u8], sw: u32, tpl: &[u8], tw: u32, th: u32, x: u32, y: u32, cut: u64) -> u64 {
    let mut ssd: u64 = 0;
    for ty in 0..th {
        let srow = ((y + ty) * sw + x) as usize;
        let trow = (ty * tw) as usize;
        for tx in 0..tw {
            let d = search[srow + tx as usize] as i32 - tpl[trow + tx as usize] as i32;
            ssd += (d * d) as u64;
            if ssd >= cut {
                return ssd;
            }
        }
    }
    ssd
}

/// SSD -> 得分：`1 - RMSE/255`，1.0 表示逐像素完全相同。
fn score_of(ssd: u64, n: u64) -> f64 {
    let rmse = (ssd as f64 / n.max(1) as f64).sqrt();
    (1.0 - rmse / 255.0).clamp(0.0, 1.0)
}

pub(crate) fn to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba
        .chunks_exact(4)
        .map(|p| {
            ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114 + 500) / 1000) as u8
        })
        .collect()
}

/// 按 k x k 块取平均降采样。比最近邻抗锯齿（模板与搜索图用同一套规则，
/// 两边都被平均过，所以"完全相同"依然能得到 1.0 分）。
fn downscale_avg(src: &[u8], w: u32, h: u32, k: u32) -> (Vec<u8>, u32, u32) {
    let nw = (w / k).max(1);
    let nh = (h / k).max(1);
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for y in 0..nh {
        for x in 0..nw {
            let mut sum: u32 = 0;
            let mut count: u32 = 0;
            for dy in 0..k {
                let sy = y * k + dy;
                if sy >= h {
                    break;
                }
                for dx in 0..k {
                    let sx = x * k + dx;
                    if sx >= w {
                        break;
                    }
                    sum += src[(sy * w + sx) as usize] as u32;
                    count += 1;
                }
            }
            out.push(if count == 0 { 0 } else { (sum / count) as u8 });
        }
    }
    (out, nw, nh)
}

/// 最近邻缩放（只给 template_scale 用）。
fn resize_nearest(src: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for y in 0..nh {
        let sy = (y as u64 * h as u64 / nh as u64).min(h as u64 - 1) as u32;
        for x in 0..nw {
            let sx = (x as u64 * w as u64 / nw as u64).min(w as u64 - 1) as u32;
            out.push(src[(sy * w + sx) as usize]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// UTF-16 + NUL 结尾，给 FindWindowW 用。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// 离线单元测试
// ---------------------------------------------------------------------------
//
// 这些测试**不碰屏幕、不开窗口、不注入输入**：全部在内存里用合成像素验证算法
// （灰度换算、降采样、最近邻缩放、模板匹配定位与得分、参数校验的 fail-fast）。
// 真实窗口的端到端验证只留一次性整合脚本（scripts/dev-probe.mjs），
// 不再靠"反复拉真实窗口抓图调参"来验证算法。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    /// 造一张确定性的"假截图"：确定性伪随机纹理 + 一点低频渐变。
    ///
    /// 关键要求是**没有自相似**。第一版用的是周期棋盘格，结果是：同一块图案在图上
    /// 每隔 6 像素就出现一次，"匹配到另一处"是**合法**结果（那些位置逐像素相同），
    /// 断言"必须命中原位置"本身就是错的——测试当场报警，说明这个测试图不能当基准。
    /// 真实截图（文字、图标、控件边框）几乎不会整块重复，所以用一个强哈希纹理更接近实际。
    fn fake_screen(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let mut z = 0x9E37_79B9_7F4A_7C15u64
                    ^ (x as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9)
                    ^ (y as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 29;
                z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z ^= z >> 32;
                let noise = (z >> 24) as u8;
                let grad = ((x + y) % 64) as u8;
                v.push(noise.wrapping_add(grad));
            }
        }
        v
    }

    /// 从大图里裁一块（模拟"用户从截图里裁出来的模板"）。
    fn cut(src: &[u8], sw: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h) as usize);
        for dy in 0..h {
            for dx in 0..w {
                out.push(src[((y + dy) * sw + x + dx) as usize]);
            }
        }
        out
    }

    /// 直接用手上的灰度像素造模板，绕开 `Matcher::load` 的文件读取。
    fn matcher_of(gray: Vec<u8>, w: u32, h: u32) -> Matcher {
        Matcher { tpl: Template { gray, w, h }, min_score: defaults::WAIT_MIN_SCORE }
    }

    fn params(v: serde_json::Value) -> WaitParams {
        serde_json::from_value(v).expect("测试参数应当能反序列化")
    }

    /// 写一张确定性的临时 PNG，返回路径（测完自己删）。
    fn write_png(w: u32, h: u32) -> String {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([((x * 5) % 256) as u8, ((y * 7) % 256) as u8, 128, 255]);
        }
        let path = std::env::temp_dir().join(format!("dsh-wait-test-{w}x{h}.png"));
        img.save(&path).expect("写临时 PNG 失败");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn gray_and_score_are_known_values() {
        assert_eq!(to_gray(&[255, 255, 255, 255]), vec![255]);
        assert_eq!(to_gray(&[0, 0, 0, 255]), vec![0]);
        assert_eq!(to_gray(&[255, 0, 0, 255]), vec![76]); // 0.299 权重
        assert_eq!(to_gray(&[0, 255, 0, 255]), vec![150]); // 0.587 权重
        assert_eq!(to_gray(&[0, 0, 255, 255]), vec![29]); // 0.114 权重

        assert!((score_of(0, 100) - 1.0).abs() < 1e-12, "逐像素相同必须是 1.0");
        assert!(score_of(100 * 255 * 255, 100) < 1e-9, "差满量程应当是 0.0");
        assert!(score_of(9, 100) > score_of(400, 100), "差得越少分越高");
    }

    #[test]
    fn downscale_avg_known_values() {
        let (o, w, h) = downscale_avg(&[100u8; 16], 4, 4, 2);
        assert_eq!((w, h), (2, 2));
        assert_eq!(o, vec![100, 100, 100, 100], "常数图降采样后还是常数");

        let (o2, w2, h2) = downscale_avg(&[0, 0, 0, 4], 2, 2, 2);
        assert_eq!((w2, h2), (1, 1));
        assert_eq!(o2, vec![1]);

        // 尺寸不是 k 的整数倍：最后一列/行只按覆盖到的像素平均。
        let rows: Vec<u8> = vec![0, 10, 20, 0, 10, 20, 0, 10, 20, 0]; // 2 行 x 5 列
        let (o3, w3, h3) = downscale_avg(&rows, 5, 2, 2);
        assert_eq!((w3, h3), (2, 1));
        assert_eq!(o3, vec![(0 + 10 + 20 + 0) / 4, (20 + 0 + 10 + 20) / 4]);
    }

    #[test]
    fn resize_nearest_known_values() {
        let src: Vec<u8> = vec![1, 2, 3, 4]; // 2x2
        assert_eq!(resize_nearest(&src, 2, 2, 2, 2), src, "同尺寸必须原样返回");
        assert_eq!(
            resize_nearest(&src, 2, 2, 4, 4),
            vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4],
            "放大 2 倍：每个像素铺成 2x2"
        );
        assert_eq!(resize_nearest(&src, 2, 2, 1, 1), vec![1], "缩到 1x1 取左上角");
    }

    /// 关键回归测试（2026-10-16 实测 bug）：模板与搜索图**各自独立**做块平均
    /// 降采样时，裁切位置不是 k 的整数倍会让两边块网格错开，一个逐像素相同的
    /// 模板自比只有 0.826 分。修复方案是"粗搜只挑候选 + 回原分辨率精算"。
    /// 这里故意把模板裁在非 k 整数倍的位置，并要求 k > 1（真的走到降采样路径）。
    #[test]
    fn self_match_is_exact_even_when_downsampled() {
        let (sw, sh) = (900u32, 700u32);
        let screen = fake_screen(sw, sh);
        let (tw, th) = (61u32, 37u32);
        let (tx, ty) = (137u32, 91u32);
        let m = matcher_of(cut(&screen, sw, tx, ty, tw, th), tw, th);

        let k = m.choose_k(sw, sh);
        assert!(k > 1, "这张图必须走降采样路径，否则测不到相位问题（k={k}）");

        let f = m.best(&screen, sw, sh).unwrap().expect("同一张图裁出来的模板必须命中");
        assert!(f.score >= 0.9999, "自比得分 {} 应当接近 1.0（旧 bug 是 0.826）", f.score);
        assert_eq!(
            (f.cx, f.cy),
            (tx as i32 + tw as i32 / 2, ty as i32 + th as i32 / 2),
            "命中点必须是模板中心，不能有相位偏移"
        );
    }

    #[test]
    fn match_at_bottom_right_border() {
        let (sw, sh) = (300u32, 200u32);
        let screen = fake_screen(sw, sh);
        let (tw, th) = (24u32, 16u32);
        let (tx, ty) = (sw - tw, sh - th);
        let m = matcher_of(cut(&screen, sw, tx, ty, tw, th), tw, th);
        let f = m.best(&screen, sw, sh).unwrap().expect("右下角贴着边界的模板也要能找到");
        assert!(f.score >= 0.9999, "边界位置得分 {}", f.score);
        assert_eq!((f.cx, f.cy), (tx as i32 + 12, ty as i32 + 8));
    }

    #[test]
    fn noise_template_scores_below_threshold() {
        let (sw, sh) = (400u32, 300u32);
        let screen = fake_screen(sw, sh);
        let (tw, th) = (50u32, 40u32);
        let noise: Vec<u8> = (0..(tw * th)).map(|i| ((i * 37 + 11) % 256) as u8).collect();
        let m = matcher_of(noise, tw, th);

        let f = m.best(&screen, sw, sh).unwrap().expect("匹配器总要返回一个最优位置");
        assert!(
            f.score < defaults::WAIT_MIN_SCORE,
            "无关模板得分 {} 不该过阈值 {}（否则满屏误报）",
            f.score,
            defaults::WAIT_MIN_SCORE
        );
    }

    #[test]
    fn oversized_template_is_invalid_args() {
        let m = matcher_of(vec![0u8; 100], 10, 10);
        // Found 没实现 Debug，所以不用 unwrap_err()。
        let e = match m.best(&vec![0u8; 25], 5, 5) {
            Err(e) => e,
            Ok(_) => panic!("模板比搜索区域还大时应当报参数错"),
        };
        assert_eq!(e.code(), ErrorCode::InvalidArgs);
        assert!(e.to_string().contains("比这一轮截到的区域"), "{e}");
    }

    /// 参数校验必须在**开始轮询之前**完成：等 10 秒才说"参数写错了"是最差体验。
    #[test]
    fn wait_rejects_bad_params_before_polling() {
        let t = std::time::Instant::now();

        let e = wait(&params(serde_json::json!({"condition":"text"}))).unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs, "wait_for_text 空 text 应当报参数错: {e}");

        let e = wait(&params(serde_json::json!({"condition":"window"}))).unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs, "wait_for_window 无任何过滤条件应当报错: {e}");

        let e = wait(&params(serde_json::json!({
            "condition":"image","template":"C:/definitely/not/here.png","timeout_ms":30000
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::NotFound, "模板文件不存在应当报 NOT_FOUND: {e}");

        assert!(t.elapsed().as_millis() < 1000, "校验失败必须是立即返回，不能先睡一轮");
    }

    #[test]
    fn template_scale_and_region_size_are_checked() {
        let png = write_png(40, 30);

        // template_scale 会把模板缩到 20x15。
        let m = Matcher::load(&png, 0.5).unwrap();
        assert_eq!((m.tpl.w, m.tpl.h), (20, 15));
        // 缩放系数越界要在加载时就报参数错（Matcher 没实现 Debug，用 match 取错误）。
        match Matcher::load(&png, 0.0) {
            Err(e) => assert_eq!(e.code(), ErrorCode::InvalidArgs),
            Ok(_) => panic!("template_scale=0 应当报参数错"),
        }

        // 模板比等待区域还大：也是立刻报参数错，不进入轮询。
        let e = wait(&params(serde_json::json!({
            "condition":"image","template":png,"min_score":0.95,
            "region":{"x":0,"y":0,"w":20,"h":20}
        })))
        .unwrap_err();
        assert_eq!(e.code(), ErrorCode::InvalidArgs);
        assert!(e.to_string().contains("比等待区域"), "{e}");

        let _ = std::fs::remove_file(&png);
    }
}
