//! `win_web_cdp`：走 Chrome DevTools Protocol，**不做模拟点击**。
//!
//! 网页交互必须走 CDP：模拟点击在网页上等于"闭着眼睛点像素"，页面一滚动、
//! 弹层一遮、分辨率一变就全错。CDP 是浏览器自己暴露的调试协议，命令是
//! 语义化的（`Page.navigate`、`Runtime.evaluate`），结果就是 JSON。
//!
//! 为什么手写 WebSocket 而不引 tungstenite：这条通道只需要"发一条命令、收一条
//! 响应"，为此拉一整个异步 WS 栈（还得和现有 current_thread 运行时对接）不划算，
//! 而且编译环境不一定能下 crate。手写客户端 + 阻塞 `std::net` 更可控：全程带
//! deadline，超时一律返回 `TIMEOUT`，绝不死等。
//!
//! 本工具**只做最小闭环**：建连 → 发命令 → 把原始 JSON 扔回去。
//! 不解析 DOM、不封装任何 CDP 域，返回值的语义由调用方（模型）自己理解。

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::error::{EngineError, EngineResult};

/// 单条 HTTP 响应体的上限，防止对端灌爆内存。
const MAX_HTTP_BODY: usize = 4 * 1024 * 1024;

/// HTTP 响应头的上限（正常就几百字节，超过就是对面在灌垃圾）。
const MAX_HTTP_HEAD: usize = 64 * 1024;
/// 单条 WebSocket 帧的上限。
const MAX_FRAME_BYTES: u64 = 8 * 1024 * 1024;

/// 一次 CDP 调用的结果（纯数据，serde 直接给 TS）。
pub struct CdpOutcome {
    pub host: String,
    pub port: u16,
    pub ws_url: String,
    pub target: Option<Value>,
    pub id: u64,
    pub result: Option<Value>,
    pub raw: Value,
    pub events: Vec<Value>,
    pub elapsed_ms: u64,
}

/// 调试端口上的目标列表：`GET /json/list`。
pub fn list_targets(host: &str, port: u16, timeout: Duration) -> EngineResult<Value> {
    let (status, body) = http_request(host, port, "GET", "/json/list", timeout)?;
    if status != 200 {
        return Err(EngineError::other(format!(
            "GET http://{host}:{port}/json/list 返回 HTTP {status}：{body}"
        )));
    }
    serde_json::from_str(&body).map_err(|e| {
        EngineError::other(format!("目标列表不是合法 JSON（{e}）：{}", truncate(&body, 400)))
    })
}

/// 新建标签页：`PUT /json/new?url=...`（新版 Chrome 只接受 PUT）。
pub fn new_target(host: &str, port: u16, url: &str, timeout: Duration) -> EngineResult<Value> {
    let path = format!("/json/new?url={}", percent_encode(url));
    let (status, body) = http_request(host, port, "PUT", &path, timeout)?;
    if status != 200 {
        return Err(EngineError::other(format!(
            "PUT http://{host}:{port}{path} 返回 HTTP {status}：{body}"
        )));
    }
    serde_json::from_str(&body).map_err(|e| {
        EngineError::other(format!("新建目标返回的不是合法 JSON（{e}）：{}", truncate(&body, 400)))
    })
}

/// 发一条 CDP 命令并等它的响应。
///
/// `ws_url` 为空时先 `GET /json/list`，挑第一个 `type == "page"` 的目标。
pub fn send_command(
    host: &str,
    port: u16,
    ws_url: Option<&str>,
    method: &str,
    params: Option<&Value>,
    timeout: Duration,
    include_events: bool,
) -> EngineResult<CdpOutcome> {
    let started = Instant::now();
    let deadline = started + timeout;

    let (ws_url, target) = match ws_url {
        Some(u) => (u.to_string(), None),
        None => {
            let targets = list_targets(host, port, remaining(deadline)?)?;
            let picked = pick_page_target(&targets).ok_or_else(|| {
                EngineError::not_found(format!(
                    "{host}:{port} 上没有可用的 page 目标（/json/list 返回 {}）。\
                     请确认浏览器是用 --remote-debugging-port={port} 启动的，且至少开着一个标签页。",
                    truncate(&targets.to_string(), 400)
                ))
            })?;
            let url = picked
                .get("webSocketDebuggerUrl")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    EngineError::other("目标里没有 webSocketDebuggerUrl 字段（浏览器返回异常）")
                })?
                .to_string();
            (url, Some(picked))
        }
    };

    let (ws_host, ws_port, ws_path) = parse_ws_url(&ws_url)?;
    let mut stream = ws_connect(&ws_host, ws_port, &ws_path, remaining(deadline)?)?;

    let message_id: u64 = 1;
    let payload = json!({
        "id": message_id,
        "method": method,
        "params": params.cloned().unwrap_or_else(|| json!({})),
    })
    .to_string();
    write_frame(&mut stream, payload.as_bytes(), 0x1)?;

    let mut events: Vec<Value> = Vec::new();
    let mut fragment: Vec<u8> = Vec::new();
    loop {
        let budget = remaining(deadline)?;
        stream.set_read_timeout(Some(budget.max(Duration::from_millis(1)))).ok();
        let (opcode, payload) = read_frame(&mut stream)?;
        let fin = opcode & 0x80 != 0;
        match opcode & 0x0f {
            // 文本帧 / 续帧：CDP 全是文本 JSON。
            0x1 | 0x2 => {
                fragment.clear();
                fragment.extend_from_slice(&payload);
                if !fin {
                    continue;
                }
            }
            0x0 => {
                fragment.extend_from_slice(&payload);
                if !fin {
                    continue;
                }
            }
            // ping：必须回 pong，否则对端会掐连接。
            0x9 => {
                write_frame(&mut stream, &payload, 0xA)?;
                continue;
            }
            0xA => continue,
            0x8 => {
                return Err(EngineError::other(
                    "CDP 对端主动关闭了 WebSocket（通常是浏览器退出或标签页被关）",
                ));
            }
            _ => continue,
        }

        let text = String::from_utf8_lossy(&fragment).to_string();
        let value: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| Value::String(text.clone()));

        if value.get("id").and_then(Value::as_u64) == Some(message_id) {
            let elapsed = started.elapsed().as_millis() as u64;
            if let Some(err) = value.get("error") {
                return Err(EngineError::other(format!(
                    "CDP 命令 {method} 返回错误：{}",
                    truncate(&err.to_string(), 600)
                )));
            }
            return Ok(CdpOutcome {
                host: host.to_string(),
                port,
                ws_url,
                target,
                id: message_id,
                result: value.get("result").cloned(),
                raw: value,
                events,
                elapsed_ms: elapsed,
            });
        }
        // 事件帧（没有 id、有 method）：默认丢掉，只要调用方要才带回。
        if value.get("method").is_some() {
            if include_events && events.len() < 50 {
                events.push(value);
            }
            continue;
        }
    }
}

/// 从 `/json/list` 结果里挑一个页面目标。
fn pick_page_target(targets: &Value) -> Option<Value> {
    let array = targets.as_array()?;
    array
        .iter()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .or_else(|| array.first())
        .cloned()
}

/// 把 `ws://host:port/path` 拆开。
fn parse_ws_url(raw: &str) -> EngineResult<(String, u16, String)> {
    let rest = raw
        .strip_prefix("ws://")
        .ok_or_else(|| EngineError::invalid(format!("ws_url 必须以 ws:// 开头：{raw}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| EngineError::invalid(format!("ws_url 端口不合法：{raw}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn http_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    timeout: Duration,
) -> EngineResult<(u16, String)> {
    let addr = format!("{host}:{port}");
    let socket = resolve(&addr)?;
    let mut stream = TcpStream::connect_timeout(&socket, timeout).map_err(|e| connect_err(&addr, e))?;
    stream.set_write_timeout(Some(timeout)).ok();

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\
         Connection: close\r\nUser-Agent: dsh-screen-control\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| EngineError::other(format!("向 {addr} 发 HTTP 请求失败: {e}")))?;

    // 为什么不能 read_to_end：Chromium 的 DevTools 端口即使收到 `Connection: close`
    // **也不主动关连接**，read_to_end 会一直卡到读超时才报错——而此时完整的 200 响应
    // 其实早就收全了（真机自测抓到的：list_targets 报 10060 超时，浏览器侧却是 200）。
    // 正确做法是按 HTTP 自己的分帧读：Content-Length / chunked；两者都没有时才回退到
    // "读到 EOF 或读超时为止"。
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let head_end = read_head(&mut stream, &mut buf, &addr, deadline)?;
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = read_body(&mut stream, &mut buf, head_end, &head, &addr, deadline)?;
    Ok((status, body))
}

/// 读到响应头结束（`\r\n\r\n`）为止，返回头的结束位置（含空行）。
fn read_head(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    addr: &str,
    deadline: Instant,
) -> EngineResult<usize> {
    let mut chunk = [0u8; 8 * 1024];
    loop {
        if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
            return Ok(pos + 4);
        }
        if buf.len() > MAX_HTTP_HEAD {
            return Err(EngineError::other(format!("{addr} 的 HTTP 响应头超过 {MAX_HTTP_HEAD} 字节")));
        }
        match read_some(stream, &mut chunk, addr, deadline)? {
            Some(0) => return Err(EngineError::other(format!("{addr} 在响应头结束前关闭了连接"))),
            None => return Err(EngineError::timeout(format!("读 {addr} 的 HTTP 响应头超时"))),
            Some(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// 按响应头把响应体读完，返回 body 文本。
fn read_body(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    head_end: usize,
    head: &str,
    addr: &str,
    deadline: Instant,
) -> EngineResult<String> {
    let lower = head.to_ascii_lowercase();
    let mut chunk = [0u8; 8 * 1024];

    if let Some(len) = content_length(&lower) {
        if len > MAX_HTTP_BODY {
            return Err(EngineError::other(format!(
                "{addr} 的响应体声明 {len} 字节，超过上限 {MAX_HTTP_BODY}"
            )));
        }
        let total = head_end + len;
        while buf.len() < total {
            match read_some(stream, &mut chunk, addr, deadline)? {
                Some(0) => {
                    return Err(EngineError::other(format!(
                        "{addr} 的响应体不完整：声明 {len} 字节，只收到 {} 字节",
                        buf.len().saturating_sub(head_end)
                    )))
                }
                None => {
                    return Err(EngineError::timeout(format!(
                        "读 {addr} 的响应体超时：声明 {len} 字节，只收到 {} 字节",
                        buf.len().saturating_sub(head_end)
                    )))
                }
                Some(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        return Ok(String::from_utf8_lossy(&buf[head_end..total]).to_string());
    }

    if lower.contains("transfer-encoding:") && lower.contains("chunked") {
        loop {
            if let Some(decoded) = decode_chunked(&buf[head_end..]) {
                return Ok(String::from_utf8_lossy(&decoded).to_string());
            }
            if buf.len() - head_end > MAX_HTTP_BODY {
                return Err(EngineError::other(format!("{addr} 的分块响应超过上限 {MAX_HTTP_BODY}")));
            }
            match read_some(stream, &mut chunk, addr, deadline)? {
                Some(0) => return Err(EngineError::other(format!("{addr} 的分块响应不完整（连接已关闭）"))),
                None => return Err(EngineError::timeout(format!("读 {addr} 的分块响应超时"))),
                Some(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    // 既没有 Content-Length 也不是 chunked：只能读到对端关闭为止，读超时即当作结束。
    loop {
        match read_some(stream, &mut chunk, addr, deadline)? {
            Some(0) | None => break,
            Some(n) => {
                if buf.len() - head_end > MAX_HTTP_BODY {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf[head_end..]).to_string())
}

/// 读一段数据。`Ok(Some(0))` = 对端关闭；`Ok(None)` = 本次读超时（不是错，
/// 含义由分帧决定：声明了长度的场景下就是"响应体没读完"）。
fn read_some(
    stream: &mut TcpStream,
    chunk: &mut [u8],
    addr: &str,
    deadline: Instant,
) -> EngineResult<Option<usize>> {
    stream.set_read_timeout(Some(remaining(deadline)?)).ok();
    loop {
        match stream.read(chunk) {
            Ok(n) => return Ok(Some(n)),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(None)
            }
            Err(e) => return Err(EngineError::other(format!("读 {addr} 的 HTTP 响应失败: {e}"))),
        }
    }
}

/// 从已小写的响应头里取 `content-length`。
fn content_length(lower_head: &str) -> Option<usize> {
    lower_head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 解析 chunked 响应体；还没收全就返回 `None`（trailer 一律忽略）。
fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        if pos >= raw.len() {
            return None;
        }
        let line_end = raw[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
        let size_text = String::from_utf8_lossy(&raw[pos..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        let data_start = line_end + 2;
        let data_end = data_start.checked_add(size)?;
        if raw.len() < data_end + 2 {
            return None;
        }
        out.extend_from_slice(&raw[data_start..data_end]);
        pos = data_end + 2;
    }
}

fn resolve(addr: &str) -> EngineResult<std::net::SocketAddr> {
    let mut iter = addr
        .to_socket_addrs()
        .map_err(|e| EngineError::invalid(format!("地址 {addr} 解析失败: {e}")))?;
    iter.next()
        .ok_or_else(|| EngineError::invalid(format!("地址 {addr} 解析不到任何 IP")))
}

fn connect_err(addr: &str, e: std::io::Error) -> EngineError {
    EngineError::other(format!(
        "连不上 {addr}（{e}）。浏览器没开远程调试端口时就是这样：\
         请用 --remote-debugging-port=<port> --user-data-dir=<临时目录> 启动 Chromium/Electron，\
         或用 win_list_windows 确认目标程序在跑。"
    ))
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

fn ws_connect(host: &str, port: u16, path: &str, timeout: Duration) -> EngineResult<TcpStream> {
    let addr = format!("{host}:{port}");
    let socket = resolve(&addr)?;
    let mut stream = TcpStream::connect_timeout(&socket, timeout).map_err(|e| connect_err(&addr, e))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    // Sec-WebSocket-Key 只要求"每次握手唯一、不可预测"，用系统 CSPRNG 生成 16 字节。
    let key = base64(&random_bytes_16());
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| EngineError::other(format!("WebSocket 握手请求发送失败: {e}")))?;

    // 握手响应必须逐个字节读：一旦多读就会吃掉紧跟其后的帧数据。
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(EngineError::other("WebSocket 握手响应头超过 8KB，放弃"));
        }
        stream
            .read_exact(&mut byte)
            .map_err(|e| EngineError::other(format!("读 WebSocket 握手响应失败: {e}")))?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    if !head.starts_with("HTTP/1.1 101") {
        return Err(EngineError::other(format!(
            "WebSocket 握手被拒绝：{}",
            head.lines().next().unwrap_or("").trim()
        )));
    }
    Ok(stream)
}

/// 写一个帧（客户端发出的帧必须加掩码，RFC 6455 硬要求）。
fn write_frame(stream: &mut TcpStream, payload: &[u8], opcode: u8) -> EngineResult<()> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = random_bytes_16();
    let mask = [mask[0], mask[1], mask[2], mask[3]];
    frame.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    stream
        .write_all(&frame)
        .map_err(|e| EngineError::other(format!("发送 WebSocket 帧失败: {e}")))?;
    stream.flush().ok();
    Ok(())
}

/// 读一个帧。返回 `(opcode 含 FIN 位, payload)`。
fn read_frame(stream: &mut TcpStream) -> EngineResult<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    read_exact_mapped(stream, &mut header)?;
    let fin = header[0] & 0x80;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7f) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        read_exact_mapped(stream, &mut ext)?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        read_exact_mapped(stream, &mut ext)?;
        len = u64::from_be_bytes(ext);
    }
    if len > MAX_FRAME_BYTES {
        return Err(EngineError::other(format!(
            "WebSocket 帧 {len} 字节，超过上限 {MAX_FRAME_BYTES}，放弃"
        )));
    }
    let mut mask = [0u8; 4];
    if masked {
        read_exact_mapped(stream, &mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    read_exact_mapped(stream, &mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((fin | opcode, payload))
}

/// `read_exact` 的包装：把"读超时"翻译成 TIMEOUT，别让调用方去猜 io 错误码。
fn read_exact_mapped(stream: &mut TcpStream, buf: &mut [u8]) -> EngineResult<()> {
    stream.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            EngineError::timeout("等待 CDP 响应超时（对端一直没回话）")
        }
        std::io::ErrorKind::UnexpectedEof => {
            EngineError::other("CDP 连接在对端回话前被关闭")
        }
        _ => EngineError::other(format!("读 WebSocket 帧失败: {e}")),
    })
}

fn remaining(deadline: Instant) -> EngineResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| EngineError::timeout("CDP 调用超时（deadline 已到）"))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 16 字节系统随机数（BCryptGenRandom）。
fn random_bytes_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    // SAFETY: BCryptGenRandom 只往 out 写 16 字节，指针与长度匹配；
    // 传空的算法句柄 + BCRYPT_USE_SYSTEM_PREFERRED_RNG 是文档允许的系统 RNG 用法。
    let status = unsafe {
        windows_sys::Win32::Security::Cryptography::BCryptGenRandom(
            std::ptr::null_mut(),
            out.as_mut_ptr(),
            out.len() as u32,
            windows_sys::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        // 极端情况下的兜底：用时间 + 地址熵凑一个，绝不 panic、不静默返回全 0
        // （全 0 的掩码不是错，但如果整个 key 都是 0 就太规律了）。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        let mix = (now ^ (pid << 32)) as u128;
        out.copy_from_slice(&mix.to_le_bytes());
    }
    out
}

/// 标准 base64（带 `=` 补齐）。
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// 只编码 URL 查询参数里必须编码的字符。
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// 起一个假 HTTP 服务器：收一条请求 → 回一段固定响应 → **不关连接**。
    /// 不关连接是重点：Chromium 的 DevTools 端口就是这个脾气，旧实现（read_to_end）
    /// 只有在对方关连接时才能跑通，所以这里必须照着真机的行为来。
    fn sticky_server(response: &'static str, hold_ms: u64) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("拿本地端口").port();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut req = [0u8; 4096];
                let _ = sock.read(&mut req);
                let _ = sock.write_all(response.as_bytes());
                let _ = sock.flush();
                thread::sleep(Duration::from_millis(hold_ms));
            }
        });
        port
    }

    #[test]
    fn content_length_response_does_not_need_eof() {
        // 真机抓到的 bug 就是这条：Chromium 收到 Connection: close 也不关连接，
        // 旧实现必然卡到读超时，把一份完整的 200 响应报成 ENGINE_ERROR。
        let port = sticky_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            2_000,
        );
        let started = Instant::now();
        let (status, body) = http_request("127.0.0.1", port, "GET", "/json/list", Duration::from_secs(3))
            .expect("不该等 EOF 才算读完");
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
        assert!(
            started.elapsed() < Duration::from_millis(1_000),
            "不能等读超时才返回：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn chunked_response_is_decoded() {
        let port = sticky_server(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            2_000,
        );
        let (status, body) = http_request("127.0.0.1", port, "GET", "/json/list", Duration::from_secs(3))
            .expect("分块响应要能解出来");
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn response_without_length_ends_at_eof() {
        // 没有 Content-Length 也不是 chunked：只能读到你关连接，超时即结束。
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("拿本地端口").port();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut req = [0u8; 4096];
                let _ = sock.read(&mut req);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\n\r\nplain");
            }
        });
        let (status, body) = http_request("127.0.0.1", port, "GET", "/json/version", Duration::from_secs(3))
            .expect("以 EOF 结束的响应要能读");
        assert_eq!(status, 200);
        assert_eq!(body, "plain");
    }

    #[test]
    fn truncated_body_is_an_error_not_a_short_success() {
        let port = sticky_server("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort", 2_000);
        let got = http_request("127.0.0.1", port, "GET", "/json/list", Duration::from_millis(800));
        assert!(got.is_err(), "声明 100 字节只收到 5 字节，必须报错");
        let message = got.expect_err("上面已经断言是 Err");
        assert!(format!("{message}").contains("响应体"), "错误信息要说清是响应体不完整：{message}");
    }

    #[test]
    fn chunk_parser_handles_partial_and_extension() {
        assert_eq!(decode_chunked(b"5\r\nhello\r\n0\r\n\r\n").as_deref(), Some(&b"hello"[..]));
        assert_eq!(decode_chunked(b"5;ext=1\r\nhello\r\n0\r\n\r\n").as_deref(), Some(&b"hello"[..]));
        assert_eq!(decode_chunked(b"5\r\nhel"), None, "没收全要返回 None 而不是半截数据");
    }

    #[test]
    fn head_parser_reads_content_length_case_insensitively() {
        assert_eq!(content_length("http/1.1 200 ok\r\ncontent-length: 42\r\n"), Some(42));
        assert_eq!(content_length("http/1.1 200 ok\r\nconnection: close\r\n"), None);
    }
}
