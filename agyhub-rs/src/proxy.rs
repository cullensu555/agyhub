//! 上游（language_server）的 HTTP/1.1 客户端。
//!
//! 为什么自己写而不用 reqwest：
//!   1. 只需要 GET/POST + 流式读 body，功能极窄；
//!   2. reqwest 默认会解压、会重试、会改头，这些「贴心」行为在反向代理里
//!      全是干扰 —— 我们要求字节级透明；
//!   3. 少一棵依赖树，编译更快、体积更小。
//!
//! 关键约束（沿用 Python 版的实测结论）：
//!   - 必须发 `Accept-Encoding: identity`。上游若返回 gzip，我们不解压，
//!     又得丢掉 content-encoding 头，浏览器就会拿到压缩字节流（乱码/截断）。
//!   - 必须发 `Connection: close`。每个请求一条连接，不复用，避免
//!     上游 keep-alive 的连接状态污染（上游对连接状态的处理不透明）。

use bytes::Bytes;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 上游永远在本机回环。
pub const UPSTREAM_HOST: &str = "127.0.0.1";

/// 响应头。
#[derive(Debug, Clone)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_chunked(&self) -> bool {
        self.get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
    }

    pub fn content_length(&self) -> Option<usize> {
        self.get("content-length").and_then(|v| v.trim().parse().ok())
    }

    pub fn is_html(&self) -> bool {
        self.get("content-type")
            .map(|v| v.to_ascii_lowercase().contains("text/html"))
            .unwrap_or(false)
    }
}

/// 一条到上游的连接。
pub struct Conn {
    sock: TcpStream,
    /// 已从 socket 读出、还没被消费的字节。
    /// 响应头和数据常常在同一个 TCP 段里到达，多读的部分必须留着，
    /// 不能丢 —— 丢了就少几个字节，表现为资源截断或 JSON 解析失败。
    buf: Vec<u8>,
    /// 已经读过响应头了吗（决定 buf 里的字节归谁）。
    head_done: bool,
}

impl Conn {
    pub async fn connect(port: u16) -> io::Result<Self> {
        let sock = TcpStream::connect((UPSTREAM_HOST, port)).await?;
        sock.set_nodelay(true).ok();
        Ok(Self {
            sock,
            buf: Vec::with_capacity(8192),
            head_done: false,
        })
    }

    /// 发一个请求。
    ///
    /// `headers` 来自浏览器（HTTP/2 伪头已由调用方剥离，但这里再兜一层）。
    pub async fn send_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        port: u16,
    ) -> io::Result<()> {
        let mut out = String::with_capacity(1024);
        out.push_str(&format!("{} {} HTTP/1.1\r\n", method, path));
        out.push_str(&format!("Host: {}:{}\r\n", UPSTREAM_HOST, port));

        for (k, v) in headers {
            let lk = k.to_ascii_lowercase();
            // h2 伪头（:method/:path/:authority/:scheme）只属于 h2 层，
            // 转给 HTTP/1.1 上游会造成 400。
            if lk.starts_with(':') {
                continue;
            }
            // 逐跳头 / 由我们自己重写的头，一律不转发。
            if matches!(
                lk.as_str(),
                "host"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "transfer-encoding"
                    | "upgrade"
                    | "content-length"
                    | "accept-encoding"
                    // h2 → h1 转换时这些是 h2 内部用的，转发无意义
                    | "te"
                    | "trailer"
            ) {
                continue;
            }
            out.push_str(&format!("{}: {}\r\n", k, v));
        }

        // 强制不压缩：我们不做解压。
        out.push_str("Accept-Encoding: identity\r\n");
        if !body.is_empty() {
            out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        out.push_str("Connection: close\r\n\r\n");

        self.sock.write_all(out.as_bytes()).await?;
        if !body.is_empty() {
            self.sock.write_all(body).await?;
        }
        self.sock.flush().await?;
        Ok(())
    }

    /// 读响应头。返回的状态码已剥离 reason phrase。
    pub async fn read_head(&mut self) -> io::Result<Head> {
        // 一路读到 \r\n\r\n
        loop {
            if let Some(pos) = find_subslice(&self.buf, b"\r\n\r\n") {
                let head_bytes: Vec<u8> = self.buf.drain(..pos + 4).collect();
                self.head_done = true;
                return parse_head(&head_bytes[..pos]);
            }
            let mut tmp = [0u8; 8192];
            let n = self.sock.read(&mut tmp).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "上游在响应头之前就关闭了连接",
                ));
            }
            self.buf.extend_from_slice(&tmp[..n]);
            // 防御：头部不该超过 1MB
            if self.buf.len() > 1024 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "响应头过大"));
            }
        }
    }

    /// 读下一段 body 字节。
    ///
    /// - `Ok(Some(b))`：有数据
    /// - `Ok(None)`：body 结束
    ///
    /// 支持 chunked、content-length、读到关闭 三种模式。
    pub async fn next_chunk(
        &mut self,
        mode: BodyMode,
    ) -> io::Result<Option<Bytes>> {
        match mode {
            BodyMode::Chunked => self.next_chunked().await,
            BodyMode::Length(remaining) => self.next_length(remaining).await,
            BodyMode::UntilClose => self.next_until_close().await,
        }
    }

    async fn fill(&mut self) -> io::Result<usize> {
        let mut tmp = [0u8; 16384];
        let n = self.sock.read(&mut tmp).await?;
        if n > 0 {
            self.buf.extend_from_slice(&tmp[..n]);
        }
        Ok(n)
    }

    async fn next_until_close(&mut self) -> io::Result<Option<Bytes>> {
        if !self.buf.is_empty() {
            return Ok(Some(Bytes::from(std::mem::take(&mut self.buf))));
        }
        let n = self.fill().await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(Bytes::from(std::mem::take(&mut self.buf))))
    }

    async fn next_length(&mut self, remaining: usize) -> io::Result<Option<Bytes>> {
        if remaining == 0 {
            return Ok(None);
        }
        while self.buf.is_empty() {
            if self.fill().await? == 0 {
                return Ok(None);
            }
        }
        let take = remaining.min(self.buf.len());
        let out: Vec<u8> = self.buf.drain(..take).collect();
        let _ = take;
        Ok(Some(Bytes::from(out)))
    }

    async fn next_chunked(&mut self) -> io::Result<Option<Bytes>> {
        // 先读 chunk size 行
        loop {
            if let Some(pos) = find_subslice(&self.buf, b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..pos + 2).collect();
                let line_str = String::from_utf8_lossy(&line[..pos]);
                // 允许 `1a;ext=1` 这种扩展
                let size_str = line_str.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16).unwrap_or(0);
                if size == 0 {
                    // trailer：读到空行为止，直接丢弃
                    loop {
                        if let Some(p) = find_subslice(&self.buf, b"\r\n") {
                            if p == 0 {
                                self.buf.drain(..2);
                                return Ok(None);
                            }
                            self.buf.drain(..p + 2);
                        } else if self.fill().await? == 0 {
                            return Ok(None);
                        }
                    }
                }
                // 读数据 + 尾部 CRLF
                while self.buf.len() < size + 2 {
                    if self.fill().await? == 0 {
                        return Ok(None);
                    }
                }
                let data: Vec<u8> = self.buf.drain(..size).collect();
                self.buf.drain(..2); // 丢掉 CRLF
                return Ok(Some(Bytes::from(data)));
            }
            if self.fill().await? == 0 {
                return Ok(None);
            }
        }
    }

    /// 读完整个 body（HTML 注入需要）。
    pub async fn read_all(&mut self, mode: BodyMode) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut m = mode;
        loop {
            match self.next_chunk(m).await? {
                Some(b) => {
                    if let BodyMode::Length(mut rem) = m {
                        rem = rem.saturating_sub(b.len());
                        m = BodyMode::Length(rem);
                    }
                    out.extend_from_slice(&b);
                }
                None => break,
            }
        }
        Ok(out)
    }
}

/// body 的传输模式。
#[derive(Debug, Clone, Copy)]
pub enum BodyMode {
    Chunked,
    Length(usize),
    UntilClose,
}

impl BodyMode {
    pub fn from_head(head: &Head) -> Self {
        if head.is_chunked() {
            BodyMode::Chunked
        } else if let Some(len) = head.content_length() {
            BodyMode::Length(len)
        } else {
            BodyMode::UntilClose
        }
    }
}

fn parse_head(raw: &[u8]) -> io::Result<Head> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    // 形如 `HTTP/1.1 200 OK`
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(200);

    let mut headers = Vec::new();
    for ln in lines {
        if ln.is_empty() {
            continue;
        }
        if let Some((k, v)) = ln.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Head { status, headers })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 发一个完整请求并读回完整响应（给内部用途，如 /__agy/* 之类的探活）。
#[allow(dead_code)]
pub async fn simple_get(port: u16, path: &str) -> io::Result<(Head, Vec<u8>)> {
    let mut c = Conn::connect(port).await?;
    c.send_request("GET", path, &[], b"", port).await?;
    let head = c.read_head().await?;
    let mode = BodyMode::from_head(&head);
    let body = c.read_all(mode).await?;
    Ok((head, body))
}
