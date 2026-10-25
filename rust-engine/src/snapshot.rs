//! 快照表：`snapshot_id` -> 元素列表。
//!
//! 存在的意义是解决"观察"和"操作"之间的时间差：TS 侧先 win_observe 拿到 uid，
//! 再由模型决定点哪个；这中间窗口可能已经重排。契约规定过期就报 STALE_SNAPSHOT，
//! 而不是让引擎拿旧坐标硬点——点错地方的代价远大于让模型重新观察一次。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{ElementInfo, Rect};

/// 一份快照的全部内容。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: u64,
    /// 观察时的根窗口，动作方法可以在没给 hwnd 时复用它。
    pub hwnd: u64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub elements: Vec<ElementInfo>,
    pub truncated: bool,
}

impl Snapshot {
    /// 是否已过期。用系统墙钟而不是 Instant，因为 expires_at_ms 要放进 JSON 给 TS 侧看，
    /// 两边必须是同一个时间基准。
    pub fn is_expired(&self) -> bool {
        now_ms() > self.expires_at_ms
    }

    /// 距过期还有多少毫秒（已过期为负）。
    pub fn remaining_ms(&self, now: i64) -> i64 {
        self.expires_at_ms - now
    }
}

/// 全局快照表。
///
/// 为什么用全局而不是把 Engine 状态传进每个函数：快照本质上就是进程级的
/// 会话状态，而 main.rs 的分发函数是同步的（每个请求一次性处理完），
/// 传一个 &mut Engine 只是把同样的东西绕一圈。用 parking_lot::Mutex 是因为
/// 分发可能在 tokio 的多线程运行时上跑，std 的 Mutex 中毒会污染整条链路。
struct Store {
    next_id: u64,
    items: HashMap<u64, Snapshot>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store { next_id: 1, items: HashMap::new() }))
}

/// 当前墙钟毫秒。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 存入一份新快照，返回它的 id。
pub fn insert(hwnd: u64, elements: Vec<ElementInfo>, ttl: Duration, truncated: bool) -> Snapshot {
    let created = now_ms();
    let expires = created + ttl.as_millis() as i64;
    let mut guard = store().lock();
    let id = guard.next_id;
    guard.next_id = id.wrapping_add(1).max(1); // 极端情况下也不返回 0
    let snap = Snapshot {
        id,
        hwnd,
        created_at_ms: created,
        expires_at_ms: expires,
        elements,
        truncated,
    };
    guard.items.insert(id, snap.clone());
    snap
}

/// 按 id 取快照，并做 TTL 校验。
///
/// 三种失败分开报，方便 TS 侧区分"要重新 observe"和"你参数写错了"：
/// - 表里没有该 id：STALE_SNAPSHOT（可能被 purge 了，也可能是别的引擎实例生成的）；
/// - 已过期：STALE_SNAPSHOT，message 里带过期了多久；
/// - id 为 0：INVALID_ARGS。
pub fn get_valid(id: u64) -> EngineResult<Snapshot> {
    if id == 0 {
        return Err(EngineError::invalid("snapshot_id 不能为 0"));
    }
    let now = now_ms();
    let guard = store().lock();
    let snap = guard
        .items
        .get(&id)
        .cloned()
        .ok_or_else(|| EngineError::stale(format!("snapshot {id} 不存在，请重新 observe")))?;
    if snap.is_expired() {
        let over = now - snap.expires_at_ms;
        return Err(EngineError::stale(format!("snapshot {id} 已过期 {over}ms，请重新 observe")));
    }
    Ok(snap)
}

/// 按 uid 在快照里找元素，同时返回所属快照。
///
/// uid 只在同一份快照内唯一，所以必须连 snapshot_id 一起给——单独一个 uid
/// 是无法解析的，这也是契约要求 `snapshot_id? + uid?` 成对出现的原因。
pub fn resolve_element(snapshot_id: u64, uid: u64) -> EngineResult<(Snapshot, ElementInfo)> {
    let snap = get_valid(snapshot_id)?;
    let elem = snap
        .elements
        .iter()
        .find(|e| e.uid == uid)
        .cloned()
        .ok_or_else(|| {
            EngineError::stale(format!(
                "snapshot {snapshot_id} 里没有 uid={uid}（该快照共 {} 个元素），请重新 observe",
                snap.elements.len()
            ))
        })?;
    Ok((snap, elem))
}

/// 元素的屏幕矩形（快照里存的就是屏幕像素，这里只是把语义标清楚）。
pub fn element_screen_rect(elem: &ElementInfo) -> Rect {
    elem.rect
}

/// 删除所有过期快照，并顺带清理进程名缓存。
///
/// 调用时机：每次处理完一个请求之后（而不是起后台定时器）。
/// 理由：这是常驻进程，但请求是稀疏的；按请求清理既不会漏，也不用引入额外任务，
/// 更不需要担心 panic=abort 下定时器线程没收尾。
pub fn purge() {
    let now = now_ms();
    let mut guard = store().lock();
    guard.items.retain(|_, snap| snap.expires_at_ms >= now);
    // 表本身可能被大量一次性快照撑大，超过阈值就整体丢弃最旧的一半，
    // 简单粗暴但足够——快照本来就是短命对象。
    if guard.items.len() > 256 {
        let mut ids: Vec<(u64, i64)> = guard.items.iter().map(|(k, v)| (*k, v.created_at_ms)).collect();
        ids.sort_by_key(|(_, created)| *created);
        let drop_count = ids.len() / 2;
        for (id, _) in ids.into_iter().take(drop_count) {
            guard.items.remove(&id);
        }
    }
    drop(guard);
    crate::window::clear_process_cache();
}

/// 清空全部快照。`--once` 模式处理完连接后调用，让重复冒烟测试之间互不干扰。
pub fn clear_all() {
    let mut guard = store().lock();
    guard.items.clear();
}

/// 当前快照数量，只用于日志。
pub fn len() -> usize {
    store().lock().items.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ElementSource;

    fn elem(uid: u64) -> ElementInfo {
        ElementInfo {
            uid,
            name: format!("e{uid}"),
            control_type: "Button".into(),
            rect: Rect { x: 0, y: 0, w: 10, h: 10 },
            is_enabled: true,
            is_password: false,
            depth: 1,
            source: ElementSource::ChildWindow,
            hwnd: Some(1000 + uid),
        }
    }

    #[test]
    fn insert_and_resolve() {
        let snap = insert(42, vec![elem(1), elem(2)], Duration::from_millis(5_000), false);
        let (got, e) = resolve_element(snap.id, 2).expect("uid 2 应该能解析");
        assert_eq!(got.hwnd, 42);
        assert_eq!(e.uid, 2);
    }

    /// 历史遗留用例，已标记忽略，不再算失败。两个原因：
    ///
    /// 1. `snapshot_ttl_ms = 0` 在契约上不可达——`main.rs` 的 `win_observe` 直接
    ///    拒绝 0（返回 INVALID_ARGS），所以这条路径只有测试能构造出来；
    /// 2. 就算构造出来也不确定：TTL=0 时 `expires_at_ms == created_at_ms`，
    ///    而过期判定是严格大于（`now > expires_at`），断言成立与否取决于
    ///    insert 和 resolve 之间墙钟有没有跳过一个毫秒——是竞态，不是契约。
    ///
    /// 真实过期路径由下面的 `expired_after_ttl_elapses` 覆盖。
    #[test]
    #[ignore = "TTL=0 契约上不可达（main.rs 拒绝 ttl=0），且 now==expires_at 时判定依赖墙钟跳动"]
    fn expired_is_stale() {
        // TTL 给 0，插入即过期。
        let snap = insert(1, vec![elem(1)], Duration::from_millis(0), false);
        let err = resolve_element(snap.id, 1).unwrap_err();
        assert_eq!(err.code(), crate::error::ErrorCode::StaleSnapshot);
    }

    #[test]
    fn expired_after_ttl_elapses() {
        // 走真实路径：TTL 明确走完之后再解析，必须报 STALE_SNAPSHOT。
        // 不复用 TTL=0，避免落到 now == expires_at 的边界上。
        let snap = insert(1, vec![elem(1)], Duration::from_millis(1), false);
        std::thread::sleep(Duration::from_millis(20));
        let err = resolve_element(snap.id, 1).unwrap_err();
        assert_eq!(err.code(), crate::error::ErrorCode::StaleSnapshot);
    }

    #[test]
    fn unknown_uid_is_stale() {
        let snap = insert(1, vec![elem(1)], Duration::from_millis(5_000), false);
        let err = resolve_element(snap.id, 99).unwrap_err();
        assert_eq!(err.code(), crate::error::ErrorCode::StaleSnapshot);
    }
}
