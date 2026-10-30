//! 长操作的取消位（GPT 审查第 27 条）。
//!
//! 为什么需要单独一套：引擎是**单线程串行**处理请求的，主管道在处理一个长操作
//! （拖拽路径、轮询等待、录制）时读不到第二条请求。所以取消走独立控制管道，
//! 由一个后台线程把它写进原子变量，长操作在自己的循环里轮询这个位、停在安全边界。
//!
//! 语义：一次只可能有一个长操作在跑（串行），所以用"当前请求 id + 被取消 id"
//! 两个原子量就够了；开始处理请求时 begin(id)，收到取消时 request(id)，
//! 长操作里 is_cancelled() 判断两者是否相等。

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::EngineError;

static CURRENT: AtomicU64 = AtomicU64::new(0);
static CANCELLED: AtomicU64 = AtomicU64::new(0);

/// 开始处理一条请求：清掉上一条的取消标记，记下当前 id。
pub fn begin(id: u64) {
    CURRENT.store(id, Ordering::SeqCst);
    CANCELLED.store(0, Ordering::SeqCst);
}

/// 收到取消请求（来自控制管道线程）。
///
/// 只认"当前正在跑的这条"：对已经结束的 id 发取消是无效的，不会影响下一条请求。
pub fn request(id: u64) {
    CANCELLED.store(id, Ordering::SeqCst);
}

/// 当前请求是否被取消。
pub fn is_cancelled() -> bool {
    let current = CURRENT.load(Ordering::SeqCst);
    current != 0 && CANCELLED.load(Ordering::SeqCst) == current
}

/// 取消的统一报错：外包成 TIMEOUT 错误码（IPC 契约只允许 7 个错误码，
/// 不新增第 8 个），message 里写清"是取消不是超时"，调用方按 message 前缀区分。
pub fn cancelled(op: &str) -> EngineError {
    EngineError::timeout(format!(
        "已取消：{op} 执行途中收到调用方的取消请求，已停在安全边界（不会留下按下的鼠标或键盘）"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 取消位是进程级全局状态，单测必须串行。
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cancel_only_affects_current_request() {
        let _guard = LOCK.lock().unwrap();

        begin(7);
        assert!(!is_cancelled(), "刚开始时不该是取消态");

        // 取消一个别的 id：不能影响当前请求（防止迟到的取消误伤下一条）。
        request(6);
        assert!(!is_cancelled(), "取消旧 id 不该影响新请求");

        request(7);
        assert!(is_cancelled(), "取消当前 id 必须生效");

        // 新请求开始：取消位被清掉。
        begin(8);
        assert!(!is_cancelled(), "begin 必须清掉上一条的取消位");

        begin(0);
        assert!(!is_cancelled(), "id=0 是无效请求，永远不算取消");
    }

    #[test]
    fn cancelled_error_is_timeout_with_clear_message() {
        let _guard = LOCK.lock().unwrap();
        let err = cancelled("win_drag_path");
        assert_eq!(err.code().as_str(), "TIMEOUT");
        assert!(err.to_string().contains("已取消"), "message 必须能区分取消与超时");
    }
}
