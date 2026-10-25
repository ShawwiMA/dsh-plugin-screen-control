//! 命名管道服务端：NDJSON 逐行读写。
//!
//! 契约要点（docs/IPC.md 传输层）：
//! - 引擎是**服务端**，管道名由 TS 壳生成并作为 `--pipe` 传进来；
//! - UTF-8，一行一个 JSON 对象，`\n` 结尾，**不允许内嵌换行**；
//! - `--once` 表示处理完一个连接就退出，用于冒烟测试。
//!
//! 为什么用 tokio 的 NamedPipeServer 而不是 std + 线程：
//! 引擎要在同一线程上处理 UIA（COM 线程亲和）。用 tokio 的 current_thread
//! 运行时，accept 与每条请求都在主线程上跑，COM 单元天然正确；
//! 换成 std + spawn 线程就得自己保证 COM 初始化/反初始化成对出现。

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::error::{EngineError, EngineResult};
use crate::protocol::{Frame, Request, defaults};

/// 单条请求的处理函数签名。main.rs 把自己的分发器传进来，
/// ipc 模块因此不需要知道任何业务方法名。
pub type Handler = fn(u64, &str, &serde_json::Map<String, serde_json::Value>) -> Frame;

/// 常驻模式：不断接受连接，每个连接顺序处理请求。
pub async fn serve_forever(pipe_name: &str, handler: Handler) -> EngineResult<()> {
    let mut server = create_server(pipe_name)?;
    loop {
        // 先等连接，再重新创建一个实例给下一个客户端——命名管道是"一个实例
        // 服务一个客户端"的模型，必须提前创建好下一个实例，否则客户端会收到
        // ERROR_PIPE_BUSY。
        server.connect().await.map_err(|e| EngineError::other(format!("管道 connect 失败: {e}")))?;
        let connected = server;
        server = create_server(pipe_name)?;

        // 单线程运行时下，这里用 block_on 风格直接 await：
        // 同一时刻只服务一个连接。TS 壳本来也只会开一个连接，
        // 并发连接在协议层没有定义（id 是单调递增的单序列）。
        if let Err(e) = serve_connection(connected, handler).await {
            crate::log_event(crate::protocol::LogLevel::Warn, format!("连接处理结束：{e}"));
        }
    }
}

/// `--once` 模式：接受一个连接，处理完该连接的所有请求后退出。
pub async fn serve_once(pipe_name: &str, handler: Handler) -> EngineResult<()> {
    let server = create_server(pipe_name)?;
    crate::log_event(
        crate::protocol::LogLevel::Info,
        format!("--once：等待客户端连接 {pipe_name}"),
    );
    server.connect().await.map_err(|e| EngineError::other(format!("管道 connect 失败: {e}")))?;
    crate::log_event(crate::protocol::LogLevel::Info, "--once：客户端已连接，开始处理请求");
    serve_connection(server, handler).await
}

fn create_server(pipe_name: &str) -> EngineResult<NamedPipeServer> {
    ServerOptions::new()
        // 首实例必须先创建，之后才能 connect；重复创建同名首实例会失败。
        .first_pipe_instance(false)
        // 消息模式 vs 字节模式：这里用**字节模式**（默认），因为 NDJSON 需要
        // 自己按 \n 切行；消息模式下每次 write 的边界才对应对端的一次 read，
        // 分包策略会变复杂。
        .create(pipe_name)
        .map_err(|e| EngineError::other(format!("创建命名管道 {pipe_name} 失败: {e}")))
}

/// 处理一个已连接客户端的所有请求，直到对端关闭或出错。
async fn serve_connection(pipe: NamedPipeServer, handler: Handler) -> EngineResult<()> {
    // 只保留一个可变绑定：NamedPipeServer 同时是 AsyncRead + AsyncWrite，
    // 读写都用它，不需要（也不能）拆成两半——拆开反而会引入借用分裂，
    // 而单连接顺序处理本来就不需要真正的全双工。
    let mut pipe = pipe;

    loop {
        let line = match read_ndjson_line(&mut pipe).await? {
            Some(line) => line,
            None => break, // 对端正常关闭
        };
        if line.trim().is_empty() {
            // 空行是合法的分隔噪声（有些客户端会多写一个 \n），跳过而不是报错。
            continue;
        }

        let frame = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handler(req.id, &req.method, &req.params),
            Err(e) => {
                // 连 id 都解析不出来时只能回 id=0。协议规定 id 是正整数，
                // 所以 0 是安全的哨兵值，TS 侧看到即知"这条请求废了"。
                Frame::Err {
                    id: 0,
                    ok: false,
                    error: crate::protocol::RpcError::new(
                        crate::error::ErrorCode::InvalidArgs,
                        format!("请求不是合法 JSON 或缺少 id/method: {e}"),
                    ),
                }
            }
        };

        write_frame(&mut pipe, &frame).await?;
    }

    // 对端断开前把写缓冲刷干净，否则最后一个响应可能丢掉。
    let _ = pipe.shutdown().await;
    Ok(())
}

/// 从管道读取一行（不含结尾换行）。
///
/// 为什么手写而不是 `BufReader::read_line`：BufReader 需要拿到 stream 的
/// 所有权，而我们在同一个循环里还要写响应。手写一个字节一个字节地读到
/// `\n`，用 Vec 累积，逻辑简单且不用拆分 stream。
///
/// 返回 Ok(None) 表示对端在行中间就关闭了（EOF）。
async fn read_ndjson_line(pipe: &mut NamedPipeServer) -> EngineResult<Option<String>> {
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];

    loop {
        match pipe.read(&mut byte).await {
            Ok(0) => {
                // EOF：如果缓冲区里还有半行，说明对端写了一半就断了。
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(EngineError::other("对端在行中间关闭了管道"));
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    // 去掉可能的 \r（Windows 客户端可能给 CRLF）。
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    let line = String::from_utf8(buf)
                        .map_err(|e| EngineError::invalid(format!("请求不是合法 UTF-8: {e}")))?;
                    return Ok(Some(line));
                }
                buf.push(byte[0]);
                if buf.len() > defaults::MAX_LINE_BYTES {
                    return Err(EngineError::invalid(format!(
                        "单行请求超过 {} 字节上限，疑似协议错乱",
                        defaults::MAX_LINE_BYTES
                    )));
                }
            }
            Err(e) => return Err(EngineError::other(format!("读管道失败: {e}"))),
        }
    }
}

/// 写出一帧：序列化成一行 JSON，补 `\n`，一次写完。
///
/// 为什么要一次性写出：分两次 write（内容 + 换行）在并发客户端下可能被
/// 交错，但对端只有一个，所以这里主要还是为了少一次系统调用。
async fn write_frame(pipe: &mut NamedPipeServer, frame: &Frame) -> EngineResult<()> {
    let mut line = serde_json::to_string(frame)?;
    // 协议不允许内嵌换行；serde_json 默认不会把字符串里的 \n 写成字面换行，
    // 但为了彻底放心，这里断言一次（release 下是空操作）。
    debug_assert!(!line.contains('\n'), "响应帧里不允许出现换行");
    line.push('\n');

    pipe.write_all(line.as_bytes())
        .await
        .map_err(|e| EngineError::other(format!("写管道失败: {e}")))?;
    pipe.flush().await.map_err(|e| EngineError::other(format!("刷管道失败: {e}")))?;
    Ok(())
}

/// 主动发一条事件帧。
///
/// 目前只在需要"引擎主动通知 TS"时使用（例如截图后端从 printwindow 退到 bitblt）。
/// 注意：事件帧没有 id，TS 侧只记日志、不回响应。
pub async fn send_event(pipe: &mut NamedPipeServer, level: crate::protocol::LogLevel, message: &str) -> EngineResult<()> {
    let frame = Frame::Event(crate::protocol::EventFrame::log(level, message));
    write_frame(pipe, &frame).await
}

/// 这个函数当前没有调用点，保留是因为它的存在解释了"为什么不用 BufReader"。
/// 真正生效的读取逻辑在 read_ndjson_line。
#[allow(dead_code)]
fn _reader_type_note(r: BufReader<NamedPipeServer>) -> BufReader<NamedPipeServer> {
    r
}
