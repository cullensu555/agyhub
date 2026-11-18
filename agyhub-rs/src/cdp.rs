//! CDP（Chrome DevTools Protocol）客户端。
//!
//! # 这个模块替代了两个外部 node 脚本
//!
//! 原来有两件事靠 node 脚本做：
//!
//! | 脚本 | 做什么 |
//! |---|---|
//! | `agy-ssh-unlock.cjs` | 注入钩子，把 `GetMendelFlags` 响应里 `enable-ssh-remote` 的 `disabled:true` 删掉，让桌面版出现 SSH 入口 |
//! | `stop-zh.cjs` | 连 CDP 清掉中文注入 + 刷页，让界面回到英文 |
//!
//! 它们本质是同一件事：**连上 Antigravity 的调试端口，往渲染进程注入/执行 JS**。
//!
//! 用 node 脚本的代价是 App 分发出去之后目标机器必须有 node。
//! 现在自己实现，App 就是真正的单文件。
//!
//! # Antigravity 的调试端口
//!
//! 从 `~/Library/Application Support/Antigravity/DevToolsActivePort`
//! 第一行读到。这个端口每次重启都可能变，所以每次操作前都要重读，
//! 不能缓存。
//!
//! # 协议要点
//!
//! - 连 `http://127.0.0.1:<port>/json/version` 拿 `webSocketDebuggerUrl`
//! - 用 `Target.attachToTarget` + `flatten:true` 拿到 sessionId
//! - 之后所有命令带上 sessionId
//! - 命令是请求/响应式的：发 `{id, method, params}`，等 `{id, result}`

use std::time::Duration;

use serde_json::{json, Value};

/// 一个 CDP 连接（裸 TCP + 手写 WebSocket 帧）。
pub struct Cdp {
    stream: tokio::net::TcpStream,
    seq: u64,
    /// 读缓冲：WS 帧可能跨 TCP 包，要自己攒
    readbuf: Vec<u8>,
}

/// 一个可注入的页面。
pub struct PageTarget {
    pub id: String,
    pub url: String,
}

impl Cdp {
    /// 连上 Antigravity 的调试端口。
    pub async fn connect() -> Result<Self, String> {
        let port = read_debug_port()?;

        let version = http_get_json(&format!("http://127.0.0.1:{}/json/version", port)).await?;
        let ws_url = version
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "调试端口没有返回 webSocketDebuggerUrl".to_string())?;

        // 不用 tokio-tungstenite 的 connect_async。
        //
        // 实测：在本机这个环境下 connect_async 会静默挂住（不返回、
        // 不报错、超时也不触发），而同样的端点用 node 的 WebSocket
        // 连是正常的。为降低不确定性，这里直接手写 WS 握手 + 帧编解码。
        //
        // CDP 只用文本帧，且都是小消息，帧格式很简单：
        //   byte0: FIN|RSV|opcode
        //   byte1: MASK|payload_len
        //   载荷 >125 字节时，长度用 2 或 8 字节扩展
        // 客户端发出的帧必须掩码（mask），服务端返回的不掩码。
        let addr = ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| "只支持 ws://".to_string())?;
        let (host_port, path) = match addr.find('/') {
            Some(i) => (&addr[..i], &addr[i..]),
            None => (addr, "/"),
        };

        let mut stream = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(host_port),
        )
        .await
        .map_err(|_| "连 CDP 端口超时".to_string())?
        .map_err(|e| format!("连 CDP 端口失败: {}", e))?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 握手。Key 用固定值即可（CDP 不校验内容，只按规范回显签名）。
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let req = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            path, host_port, key
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("WS 握手写失败: {}", e))?;

        // 读到 \r\n\r\n 为止
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(15), stream.read(&mut tmp))
                .await
                .map_err(|_| "WS 握手超时".to_string())?
                .map_err(|e| format!("WS 握手读失败: {}", e))?;
            if n == 0 {
                return Err("WS 握手期间连接被关闭".into());
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if buf.len() > 65536 {
                return Err("WS 握手响应过大".into());
            }
        }

        let head = String::from_utf8_lossy(&buf);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!(
                "WS 升级被拒: {}",
                head.lines().next().unwrap_or("")
            ));
        }

        Ok(Cdp {
            stream,
            seq: 0,
            readbuf: Vec::new(),
        })
    }

    /// 列出所有页面类型的 target。
    pub async fn list_pages(&self) -> Result<Vec<PageTarget>, String> {
        let port = read_debug_port()?;
        let list = http_get_json_array(&format!("http://127.0.0.1:{}/json/list", port)).await?;

        let mut out = Vec::new();
        for t in list {
            let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty != "page" {
                continue;
            }
            let url = t
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 只关心 Antigravity 的本地页面。
            // about:blank / devtools:// 之类的跳过，
            // 但新窗口刚开时 URL 可能是空的 —— 那也要放行，
            // 因为导航完成后就是我们要的页面了。
            if !url.is_empty()
                && !url.starts_with("http://127.0.0.1")
                && !url.starts_with("https://127.0.0.1")
                && !url.starts_with("http://localhost")
                && !url.starts_with("https://localhost")
            {
                continue;
            }
            let id = t
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                out.push(PageTarget { id, url });
            }
        }
        Ok(out)
    }

    /// 附加到某个 target，返回 sessionId。
    pub async fn attach(&mut self, target_id: &str) -> Result<String, String> {
        let r = self
            .send("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None)
            .await?;
        r.get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "attach 没返回 sessionId".to_string())
    }

    /// 发一条命令，等它的响应。
    pub async fn send(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        self.seq += 1;
        let id = self.seq;

        let mut payload = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            payload["sessionId"] = json!(sid);
        }

        self.write_text(&payload.to_string()).await?;

        // 等自己的响应：中间可能夹杂别的事件（id 不匹配），跳过。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                return Err(format!("{} 超时", method));
            }
            let text = match tokio::time::timeout(remain, self.read_text()).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(format!("{} 超时", method)),
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("{} 报错: {}", method, err));
                }
                return Ok(v.get("result").cloned().unwrap_or(json!({})));
            }
        }
    }

    /// 发一个文本帧。
    async fn write_text(&mut self, text: &str) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;

        let payload = text.as_bytes();
        let mut frame = Vec::with_capacity(payload.len() + 14);

        // byte0: FIN=1, opcode=1(文本)
        frame.push(0x81);

        // byte1: MASK=1（客户端必须掩码）+ 长度
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }

        // 掩码键（固定值即可，不要求随机）
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }

        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| format!("WS 写失败: {}", e))
    }

    /// 读一个文本帧（跳过 ping/pong/close）。
    async fn read_text(&mut self) -> Result<String, String> {
        use tokio::io::AsyncReadExt;

        loop {
            // 尝试从缓冲里解一个完整帧
            if let Some((opcode, payload, used)) = parse_frame(&self.readbuf) {
                self.readbuf.drain(..used);
                match opcode {
                    0x1 => {
                        return String::from_utf8(payload).map_err(|e| format!("帧非 UTF8: {}", e))
                    }
                    0x8 => return Err("服务端关闭了连接".into()),
                    0x9 => {
                        // ping → 回 pong（这里简化：忽略，CDP 很少用）
                        continue;
                    }
                    _ => continue,
                }
            }

            // 缓冲里没有完整帧，继续读
            let mut tmp = [0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(15), self.stream.read(&mut tmp))
                .await
                .map_err(|_| "WS 读超时".to_string())?
                .map_err(|e| format!("WS 读失败: {}", e))?;
            if n == 0 {
                return Err("WS 连接已关闭".into());
            }
            self.readbuf.extend_from_slice(&tmp[..n]);
        }
    }

    /// 在页面里执行 JS，返回结果值。
    ///
    /// ⚠️ JS 抛异常时 CDP **不会**返回 error，而是在 result 里带
    /// `exceptionDetails`。原来没检查这个字段，脚本有语法错误也被当成
    /// "注入成功"—— SSH 解锁钩子里残留了一个 node 模板占位符
    /// `${JSON.stringify(TARGET_FLAG)}`，每次都是 SyntaxError，
    /// 却一直报告"已给 N 个页面装好钩子"。
    pub async fn eval(&mut self, session_id: &str, expr: &str) -> Result<Value, String> {
        let r = self
            .send(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
                Some(session_id),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let desc = ex
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .or_else(|| ex.get("text").and_then(|t| t.as_str()))
                .unwrap_or("未知异常");
            let first = desc.lines().next().unwrap_or(desc);
            return Err(format!("页面脚本异常: {}", first));
        }
        Ok(r.get("result")
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 断开某个会话（不影响页面本身）。
    pub async fn detach(&mut self, session_id: &str) {
        let _ = self
            .send("Target.detachFromTarget", json!({ "sessionId": session_id }), None)
            .await;
    }

    /// 刷新页面。
    pub async fn reload(&mut self, session_id: &str) -> Result<(), String> {
        self.send("Page.reload", json!({ "ignoreCache": false }), Some(session_id))
            .await
            .map(|_| ())
    }
}

/// 从缓冲里解一个 WebSocket 帧。
///
/// 返回 (opcode, payload, 消耗字节数)。数据不完整时返回 None。
fn parse_frame(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0F;
    let masked = (buf[1] & 0x80) != 0;
    let mut len = (buf[1] & 0x7F) as usize;
    let mut idx = 2;

    if len == 126 {
        if buf.len() < idx + 2 {
            return None;
        }
        len = u16::from_be_bytes([buf[idx], buf[idx + 1]]) as usize;
        idx += 2;
    } else if len == 127 {
        if buf.len() < idx + 8 {
            return None;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[idx..idx + 8]);
        len = u64::from_be_bytes(b) as usize;
        idx += 8;
    }

    let mut mask = None;
    if masked {
        if buf.len() < idx + 4 {
            return None;
        }
        mask = Some([buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]]);
        idx += 4;
    }

    if buf.len() < idx + len {
        return None;
    }

    let mut payload = buf[idx..idx + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }

    Some((opcode, payload, idx + len))
}

/// 读 Antigravity 的调试端口。
///
/// 每次都要重读：这个端口每次重启应用都会变，缓存会连到旧端口上。
pub fn read_debug_port() -> Result<u16, String> {
    crate::upstream::devtools_port()
        .ok_or_else(|| "Antigravity 未运行（读不到调试端口）".to_string())
}

/// 极简 HTTP GET，返回 JSON 对象。
async fn http_get_json(url: &str) -> Result<Value, String> {
    let body = http_get(url).await?;
    serde_json::from_str(&body).map_err(|e| format!("解析 JSON 失败: {}", e))
}

/// 极简 HTTP GET，返回 JSON 数组。
async fn http_get_json_array(url: &str) -> Result<Vec<Value>, String> {
    let body = http_get(url).await?;
    serde_json::from_str(&body).map_err(|e| format!("解析 JSON 失败: {}", e))
}

/// 极简 HTTP GET。
///
/// 为什么不用 reqwest：只是为了读两个本地调试端点，
/// 引入一个大依赖不值得（reqwest 会拉一堆 TLS 相关 crate）。
async fn http_get(url: &str) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "只支持 http".to_string())?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(host_port),
    )
    .await
    .map_err(|_| format!("连接 {} 超时", host_port))?
    .map_err(|e| format!("连接 {} 失败（Antigravity 可能已退出）: {}", host_port, e))?;

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host_port
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("写请求失败: {}", e))?;

    // ⚠️ 不能 read_to_end。
    //
    // 调试端口返回的响应带 Content-Length 但不关连接（keep-alive），
    // read_to_end 会一直等 EOF，永远不返回 ——
    // 表现就是"连 CDP 卡死"，上层看起来像程序无响应。
    // （实测：这就是 ssh-unlock 命令卡住不动的原因。）
    //
    // 改成：读到 Content-Length 指明的长度就够了；没有该头再靠 EOF。
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .map_err(|_| "读调试端口超时".to_string())?
            .map_err(|e| format!("读响应失败: {}", e))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        let text = String::from_utf8_lossy(&buf);
        if let Some(hdr_end) = text.find("\r\n\r\n") {
            let head = &text[..hdr_end];
            let body_len = buf.len() - (hdr_end + 4);
            // 找 Content-Length
            let want = head.lines().find_map(|l| {
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            });
            match want {
                Some(w) if body_len >= w => break,
                None => {
                    // 没有 Content-Length，靠 EOF；但加个上限防跑飞
                    if buf.len() > 1_048_576 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if buf.len() > 1_048_576 {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buf);
    match text.find("\r\n\r\n") {
        Some(i) => Ok(text[i + 4..].to_string()),
        None => Ok(text.into_owned()),
    }
}

/// 把 JSON 值安全地嵌入 JS 表达式。
pub fn js_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

