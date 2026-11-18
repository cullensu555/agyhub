//! HTTP/2 服务器：浏览器侧的唯一入口。
//!
//! 为什么必须是 HTTP/2（不可妥协）：
//!   Antigravity 前端一打开就同时建 6 个**永不结束**的流式订阅。
//!   HTTP/1.1 下浏览器对同一域名只给 6 个并发连接，被订阅占满后，
//!   后续请求（发消息、取认证状态、加载 main.js）全部排队超时。
//!   现象：页面能看，一操作就卡死。
//!
//!   注意这不是「性能优化」，是「能不能用」的问题。
//!
//! 与 Python 版的关键差别：
//!   Python 的 hyper-h2 里 `send_data` 超出流控窗口的部分**静默丢弃**
//!   （不报错），所以必须手写分片 + 等窗口，那段代码是踩过两次坑的产物。
//!   Rust 的 h2 crate 不会丢数据，但 `send_data` 是同步函数，
//!   不做 reserve 就发会**无界缓冲**（文档原话：this buffering is unbounded），
//!   转发大文件时内存会爆。所以统一走下面的 `send_flow()`：
//!   reserve_capacity → poll_capacity 等窗口 → 只发可用量。
//!   这才是正确的背压，也顺便替代了 Python 版手写的分片逻辑。
//!
//! 上游侧走 HTTP/1.1：每个请求自己建连、自己管，不受浏览器连接数限制。

use crate::{inject, proxy, upstream};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// 全局配置。
pub struct Ctx {
    pub token: TokenSource,
    pub verbose: bool,
    /// 根证书 PEM，供 `/__agy/ca.crt` 下载安装。
    pub ca_pem: String,
    /// 当前监听端口，引导页上会显示。
    pub port: u16,
}

/// 访问口令的来源。
///
/// ## 为什么不在启动时读一次就完事
///
/// 原来口令在启动时读进内存，之后不再变。菜单里「更换访问口令」只改了
/// 配置文件，正在跑的网关还认旧口令：**新链接打不开（401），旧链接照样能用**，
/// 和弹窗里说的"旧链接会失效"正好相反，换口令等于没换。
///
/// 现在每次校验都取当前值：配置文件里的口令最多缓存 1 秒。
/// 换口令立即生效，不需要重启服务（SSH 网页版也不用重建隧道）。
pub struct TokenSource {
    /// 环境变量 AGEHUB_TOKEN 指定的固定口令（命令行用户用）
    fixed: Option<String>,
    cache: std::sync::Mutex<(String, std::time::Instant)>,
}

impl TokenSource {
    pub fn new(fixed: Option<String>, initial: String) -> Self {
        Self {
            fixed,
            cache: std::sync::Mutex::new((initial, std::time::Instant::now())),
        }
    }

    pub fn get(&self) -> String {
        if let Some(t) = &self.fixed {
            return t.clone();
        }
        let mut g = match self.cache.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if g.1.elapsed() >= Duration::from_secs(1) {
            let t = crate::config::Config::load().token;
            if !t.is_empty() {
                g.0 = t;
            }
            g.1 = std::time::Instant::now();
        }
        g.0.clone()
    }
}

/// 在阻塞线程池里找上游（lsof/探测都是同步 IO，不能卡住异步工作线程）。
async fn find_upstream_async(force: bool) -> Option<u16> {
    tokio::task::spawn_blocking(move || upstream::find_upstream(force))
        .await
        .ok()
        .flatten()
}

/// 连上游；失败时清掉缓存重新找一次再连。
///
/// Antigravity 重启后端口会变，而上游端口有 30 秒缓存 ——
/// 不重试的话这 30 秒内所有请求都直接断开，页面一片空白。
async fn connect_upstream() -> Option<(proxy::Conn, u16)> {
    let port = find_upstream_async(false).await?;
    if let Ok(c) = proxy::Conn::connect(port).await {
        return Some((c, port));
    }
    upstream::invalidate();
    let port2 = find_upstream_async(true).await?;
    proxy::Conn::connect(port2).await.ok().map(|c| (c, port2))
}

/// 取一个局域网 IP 用于展示（引导页底部显示"本机地址"）。
///
/// 只用于显示，不参与任何逻辑判断 —— 所以取不到就返回占位符。
fn local_ip() -> String {
    crate::cert::local_ips()
        .into_iter()
        .find(|ip| !ip.is_loopback())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "本机".into())
}

// ============================================================
// 流控发送
// ============================================================

/// 按流控窗口发一段数据，带**背压**。
///
/// 为什么必须这么做（这是本项目最容易出错的点）：
///
///   h2 crate 的 `send_data` 是**同步**函数，不返回 future。文档明确写着：
///   「不 reserve 直接 send_data 时，数据会被缓冲，容量被隐式申请……
///     但这种缓冲是无界的（unbounded）」。
///
///   也就是说：直接 `send_data` 不会丢数据（这点比 Python 的 hyper-h2 好），
///   但会**把整个响应堆进内存**。转发一个几百 MB 的文件时内存直接爆掉。
///
///   正确姿势是三步：
///     1. reserve_capacity(n)      —— 告诉连接「我打算发 n 字节」
///     2. poll_capacity() 等到位   —— 等对端回 WINDOW_UPDATE
///     3. capacity() 取实际可用量，只发这么多
///
///   这样窗口不足时会挂起（await），产生真正的背压：上游读得慢一点，
///   就不会把浏览器跟不上的数据全堆在内存里。
async fn send_flow(
    send: &mut h2::SendStream<Bytes>,
    mut data: Bytes,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while !data.is_empty() {
        // 请求容量。每次只请求「还需要发多少」，连接会按窗口逐步满足。
        send.reserve_capacity(data.len());

        // 等到真有容量可用。
        // 用 poll_fn 把 poll_capacity 包装成 future —— h2 没提供 async 版本。
        let cap = match std::future::poll_fn(|cx| send.poll_capacity(cx)).await {
            Some(Ok(n)) => n,
            Some(Err(e)) => return Err(e.into()),
            // None 表示流已关闭，不再继续
            None => return Ok(()),
        };

        let take = cap.min(data.len());
        // split_to 是零拷贝的：只移动 Bytes 的起始指针，不复制底层缓冲区。
        let head = data.split_to(take);
        send.send_data(head, false)?;
    }
    Ok(())
}

/// 发完并结束流。
async fn end_stream(
    send: &mut h2::SendStream<Bytes>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 空的数据帧不占流控窗口，可以直接发。
    send.send_data(Bytes::new(), true)?;
    Ok(())
}

/// 绑定监听端口（与 serve 分开：调用方要在绑定成功后才登记"已就绪"）。
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port)).await
}

/// 启动 HTTPS/HTTP2 服务。
pub async fn serve(
    listener: TcpListener,
    tls: TlsAcceptor,
    ctx: Arc<Ctx>,
) -> std::io::Result<()> {
    tracing::info!("监听 0.0.0.0:{} (HTTPS/HTTP2)", ctx.port);

    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // 文件句柄耗尽（EMFILE）时 accept 会立刻反复失败，
                // 不歇一下就是一个打满 CPU 的死循环。
                tracing::warn!("accept 失败: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        sock.set_nodelay(true).ok();
        let tls = tls.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), tls.accept(sock)).await {
                Ok(Ok(stream)) => {
                    if let Err(e) = serve_conn(stream, ctx).await {
                        tracing::debug!("连接 {} 结束: {}", peer, e);
                    }
                }
                Ok(Err(e)) => tracing::debug!("TLS 握手失败 {}: {}", peer, e),
                Err(_) => tracing::debug!("TLS 握手超时 {}", peer),
            }
        });
    }
}

type TlsStream = tokio_rustls::server::TlsStream<TcpStream>;

async fn serve_conn(stream: TlsStream, ctx: Arc<Ctx>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // enable_connect_protocol：声明支持 RFC 8441 extended CONNECT。
    // 浏览器在 HTTP/2 下就用这个（`:protocol: websocket`）发起 WebSocket。
    // 不声明的话浏览器会退回 HTTP/1.1 另开一条连接，白白占掉一个槽位 ——
    // 而连接槽位正是本项目最稀缺的资源。
    let mut conn = h2::server::Builder::new()
        .enable_connect_protocol()
        .initial_window_size(1024 * 1024)
        .initial_connection_window_size(8 * 1024 * 1024)
        .handshake(stream)
        .await?;

    while let Some(result) = conn.accept().await {
        match result {
            Ok((req, respond)) => {
                let ctx = ctx.clone();
                // 每个 stream 单独一个任务：请求之间互不阻塞，
                // 长轮询/流式订阅不会卡住后面的静态资源请求。
                tokio::spawn(async move {
                    if let Err(e) = handle(req, respond, ctx).await {
                        tracing::debug!("请求处理失败: {}", e);
                    }
                });
            }
            Err(e) => {
                tracing::debug!("accept stream 失败: {}", e);
                break;
            }
        }
    }
    Ok(())
}

async fn handle(
    req: Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    ctx: Arc<Ctx>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ---- RFC 8441：HTTP/2 上的 WebSocket ----
    let is_ws = req.method() == Method::CONNECT
        && req
            .extensions()
            .get::<h2::ext::Protocol>()
            .map(|p| p.as_str().eq_ignore_ascii_case("websocket"))
            .unwrap_or(false);

    if is_ws {
        return ws_tunnel(req, respond, ctx).await;
    }

    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let clean_path = path.split('?').next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();

    if ctx.verbose {
        tracing::info!("{} {}", method, path);
    }

    // ---- 内置路由：免口令 ----
    //
    // 必须在鉴权之前拦。favicon 与 manifest 是浏览器自动发起的，
    // 不会带 ?t= 口令，走鉴权会被 401 挡掉 —— 标签页就永远显示默认图标、
    // PWA 也装不上。
    if let Some(asset) = inject::builtin_asset(&clean_path, &ctx.ca_pem) {
        let mut r = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", asset.content_type)
            .header("content-length", asset.body.len().to_string());
        if let Some(cc) = asset.cache {
            r = r.header("cache-control", cc);
        }
        let resp = r.body(()).unwrap();
        let mut send = respond.send_response(resp, asset.body.is_empty())?;
        if !asset.body.is_empty() {
            send_flow(&mut send, Bytes::from(asset.body)).await?;
            end_stream(&mut send).await?;
        }
        return Ok(());
    }

    // ---- 口令 ----
    let token = ctx.token.get();
    if !token_ok(&headers, &path, &token) {
        let body = AUTH_PAGE.as_bytes().to_vec();
        let resp = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "text/html; charset=utf-8")
            .header("content-length", body.len().to_string())
            .body(())?;
        let mut send = respond.send_response(resp, false)?;
        send_flow(&mut send, Bytes::from(body)).await?;
        end_stream(&mut send).await?;
        return Ok(());
    }

    // ---- 收完请求体 ----
    //
    // 必须边读边 release_capacity：h2 的接收窗口不会自动恢复，
    // 不释放的话对端发到 64KB 就停住，请求永远收不完
    // （现象：上传附件后发送按钮一直转圈）。
    let req_body = read_body(body, &ctx).await?;

    // ---- 找上游并连上 ----
    let (mut up, port) = match connect_upstream().await {
        Some(v) => v,
        None => {
            // 上游不可用 → 给引导页，而不是一句"not running"。
            //
            // 区分几种情况（没装 / 没开 / 没登录 / 开发机断线），
            // 因为用户该做的事完全不同。详见 inject::guide_html。
            let status = inject::detect_status();
            let html = inject::guide_html(status, &local_ip(), ctx.port);
            let body = html.into_bytes();

            let resp = Response::builder()
                // 用 200 而不是 5xx：浏览器对 5xx 有时会显示自己的
                // 错误页，盖掉我们精心写的引导内容。
                .status(StatusCode::OK)
                .header("content-type", "text/html; charset=utf-8")
                .header("content-length", body.len().to_string())
                .header("cache-control", "no-store")
                .body(())?;
            let mut send = respond.send_response(resp, false)?;
            send_flow(&mut send, Bytes::from(body)).await?;
            end_stream(&mut send).await?;
            return Ok(());
        }
    };

    up.send_request(method.as_str(), &path, &headers, &req_body, port)
        .await?;

    let head = up.read_head().await?;
    let mode = proxy::BodyMode::from_head(&head);

    // ---- HTML 需要整段读完才能注入 ----
    //
    // 不能流式：注入点必须在 </head> 之前，而 </head> 在文档末尾附近，
    // 流式转发时那段字节早就发出去了，没法再改。
    if head.is_html() {
        let raw = up.read_all(mode).await?;
        let text = String::from_utf8_lossy(&raw);
        let patched = inject::patch_html(&text);
        let out = patched.into_bytes();

        let mut r = Response::builder().status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::OK));
        for (k, v) in forward_headers(&head, true) {
            r = r.header(k, v);
        }
        // ---- 种 cookie ----
        //
        // 这一步**绝对不能省**。首次通过 ?t=口令 进来时，必须把口令写进
        // cookie；否则后续 main.js / css / api 请求（都不带 ?t=）全部 401，
        // 页面就是一个白屏。
        //
        // 用 Long Max-Age（400 天）：装成 PWA 后要长期免输口令。
        // HttpOnly：JS 读不到，减少 XSS 泄口令面。
        // SameSite=Lax：同站正常，跨站不带，避免被别站引用。
        if !token.is_empty() && path.contains(&token) {
            r = r.header(
                "set-cookie",
                format!(
                    "agygate={}; Path=/; Max-Age=34560000; HttpOnly; SameSite=Lax",
                    token
                ),
            );
        }
        r = r.header("content-length", out.len().to_string());
        let resp = r.body(())?;
        let mut send = respond.send_response(resp, out.is_empty())?;
        if !out.is_empty() {
            send_flow(&mut send, Bytes::from(out)).await?;
            end_stream(&mut send).await?;
        }
        return Ok(());
    }

    // ---- 其余：流式转发 ----
    let mut r = Response::builder().status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::OK));
    for (k, v) in forward_headers(&head, false) {
        r = r.header(k, v);
    }
    let resp = r.body(())?;
    let mut send = respond.send_response(resp, false)?;

    let mut m = mode;
    loop {
        match up.next_chunk(m).await? {
            Some(chunk) => {
                if let proxy::BodyMode::Length(rem) = m {
                    m = proxy::BodyMode::Length(rem.saturating_sub(chunk.len()));
                }
                // 带背压转发：窗口不够就挂起等 WINDOW_UPDATE，
                // 而不是把整段响应急着塞进内存缓冲。
                send_flow(&mut send, chunk).await?;
            }
            None => break,
        }
    }
    end_stream(&mut send).await?;
    Ok(())
}

/// 读完整请求体，边读边释放接收窗口。
async fn read_body(
    mut body: h2::RecvStream,
    _ctx: &Ctx,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        // 关键：不释放窗口，对端发到 64KB 就永久停住。
        body.flow_control().release_capacity(chunk.len())?;
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 挑选要转发给浏览器的响应头。
fn forward_headers(head: &proxy::Head, for_html: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in &head.headers {
        let lk = k.to_ascii_lowercase();
        match lk.as_str() {
            // 逐跳头：h2 里没有意义，转发会造成协议错误
            "connection" | "keep-alive" | "transfer-encoding" | "upgrade"
            | "proxy-connection" => continue,
            // content-length 我们自己算：注入后长度变了。
            // 即便不注入，上游给的长度也可能与实际不符（chunked 场景）。
            "content-length" => continue,
            // 我们强制上游 identity 编码，这个头不该出现；
            // 万一出现，转发出去会让浏览器尝试解压未压缩的字节。
            "content-encoding" => continue,
            _ => {}
        }
        if for_html && lk == "cache-control" {
            // 注入后的 HTML 不要缓存：改了注入内容后用户刷新就该看到新版，
            // 否则会出现「改了没生效」的错觉。
            out.push((k.clone(), "no-store".to_string()));
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    out
}

/// 口令校验：cookie 或 query 里带 ?t=。
fn token_ok(headers: &[(String, String)], path: &str, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("cookie") {
            for part in v.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("agygate=") {
                    if rest == token {
                        return true;
                    }
                }
            }
        }
    }
    path.contains(token)
}

// ============================================================
// WebSocket 隧道
// ============================================================

/// 把 h2 的 extended CONNECT 转成普通 WebSocket 握手发给上游，
/// 之后在两条字节流之间做透明搬运。
///
/// 为什么不做 WebSocket 帧解析：
///   握手完成后两端都是**裸字节流**（帧已经含在数据里），
///   直接互转即可。不解析就不需要处理压缩扩展、ping/pong、
///   分片帧这些细节，天然透明。
async fn ws_tunnel(
    req: Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    ctx: Arc<Ctx>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = req.into_body();

    // WebSocket 同样要鉴权：原来这里没查口令，
    // 知道端口的人不带口令也能直接连上实时通道。
    if !token_ok(&headers, &path, &ctx.token.get()) {
        let resp = Response::builder().status(StatusCode::UNAUTHORIZED).body(())?;
        let mut s = respond.send_response(resp, true)?;
        let _ = s.send_data(Bytes::new(), true);
        return Ok(());
    }

    let bad_gateway = |mut respond: h2::server::SendResponse<Bytes>| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resp = Response::builder().status(StatusCode::BAD_GATEWAY).body(())?;
        let mut s = respond.send_response(resp, true)?;
        let _ = s.send_data(Bytes::new(), true);
        Ok(())
    };

    let mut port = match find_upstream_async(false).await {
        Some(p) => p,
        None => return bad_gateway(respond),
    };

    if ctx.verbose {
        tracing::info!("[ws] {}", path);
    }

    let mut up = match TcpStream::connect((proxy::UPSTREAM_HOST, port)).await {
        Ok(s) => s,
        Err(_) => {
            upstream::invalidate();
            port = match find_upstream_async(true).await {
                Some(p) => p,
                None => return bad_gateway(respond),
            };
            match TcpStream::connect((proxy::UPSTREAM_HOST, port)).await {
                Ok(s) => s,
                Err(_) => return bad_gateway(respond),
            }
        }
    };
    up.set_nodelay(true).ok();

    // 组装上游 Upgrade 握手
    let key = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            // 浏览器在 h2 下通常不带 key（RFC 8441 不需要），自己造一个
            use std::time::{SystemTime, UNIX_EPOCH};
            let n = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            b64_16(n)
        });

    let mut hs = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {}\r\n",
        path,
        proxy::UPSTREAM_HOST,
        port,
        key
    );
    for (k, v) in &headers {
        let lk = k.to_ascii_lowercase();
        if lk.starts_with(':')
            || matches!(
                lk.as_str(),
                "host"
                    | "connection"
                    | "upgrade"
                    | "sec-websocket-version"
                    | "sec-websocket-key"
                    | "content-length"
                    | "accept-encoding"
            )
        {
            continue;
        }
        hs.push_str(&format!("{}: {}\r\n", k, v));
    }
    hs.push_str("\r\n");
    up.write_all(hs.as_bytes()).await?;

    // 读上游握手响应
    let mut raw: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    let hdr_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(15), up.read(&mut tmp)).await??;
        if n == 0 {
            let resp = Response::builder().status(StatusCode::BAD_GATEWAY).body(())?;
            let mut s = respond.send_response(resp, true)?;
            let _ = s.send_data(Bytes::new(), true);
            return Ok(());
        }
        raw.extend_from_slice(&tmp[..n]);
        if let Some(p) = find(&raw, b"\r\n\r\n") {
            break p;
        }
        if raw.len() > 64 * 1024 {
            break 0;
        }
    };

    let head_txt = String::from_utf8_lossy(&raw[..hdr_end]).to_string();
    let ok = head_txt
        .lines()
        .next()
        .map(|l| l.contains(" 101 "))
        .unwrap_or(false);

    if !ok {
        tracing::warn!("[ws] 上游拒绝升级: {}", head_txt.lines().next().unwrap_or(""));
        let resp = Response::builder().status(StatusCode::BAD_GATEWAY).body(())?;
        let mut s = respond.send_response(resp, true)?;
        let _ = s.send_data(Bytes::new(), true);
        return Ok(());
    }

    // ⚠️ 握手响应读完，缓冲区里 \r\n\r\n 之后可能已经带着上游立刻推送的
    // 首批数据（Antigravity 的 /connect-websocket 就是回 101 的同一次
    // flush 里把初始状态一起推过来）。
    // 这段必须原样转发 —— 早期 Python 版直接丢弃，导致建连后第一批实时
    // 事件丢失：界面停在旧状态，必须手动刷新才看到最新。
    // 特征：丢失字节数随 TCP 分包时机变化，而「读到的字节数」与
    // 「发出去的字节数」完全相等（丢在读完之后）。
    let tail = raw[(hdr_end + 4).min(raw.len())..].to_vec();

    // RFC 8441：成功用 200 表示
    let resp = Response::builder().status(StatusCode::OK).body(())?;
    let mut send = respond.send_response(resp, false)?;

    // 拆成读写两半：两个方向要并发跑，不能互相阻塞。
    // （WebSocket 是双向持续流动的，串行处理会导致一边推数据时另一边读不到。）
    let (mut up_r, mut up_w) = up.into_split();

    // ---- 上游 → h2 ----
    let to_browser = tokio::spawn(async move {
        // 先把跟随握手一起读到的首批数据送出去。
        // 顺序不能颠倒：这批字节产生时间早于后续 read 的内容。
        if !tail.is_empty() && send_flow(&mut send, Bytes::from(tail)).await.is_err() {
            return;
        }
        let mut buf = vec![0u8; 16384];
        loop {
            match up_r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    // 带背压转发：浏览器读得慢时这里的 await 会挂起，
                    // 上游数据就积压在内核缓冲区，不会堆进本进程内存。
                    if send_flow(&mut send, Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = send.send_data(Bytes::new(), true);
    });

    // ---- h2 → 上游 ----
    loop {
        match body.data().await {
            Some(Ok(chunk)) => {
                if body.flow_control().release_capacity(chunk.len()).is_err() {
                    break;
                }
                if up_w.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            Some(Err(_)) => break,
            None => break,
        }
    }
    // 浏览器侧关闭 → 通知上游。shutdown 写方向即可，
    // 上游收到 EOF 后会自己收尾再关读方向。
    let _ = up_w.shutdown().await;

    // 等上游读方向结束，保证 tail 与后续数据都发完再返回。
    // 不 await 的话函数返回会 drop respond，那条 stream 被 RST，
    // 浏览器侧看到的是「连接突然断开」。
    let _ = to_browser.await;
    Ok(())
}

/// 造一个 base64 编码的 16 字节伪随机串（WebSocket-Key 用）。
///
/// RFC 6455 要求 16 字节随机数 base64 后作为 key，上游只做
/// 「回显 + 固定 GUID 做 SHA1」，不校验随机性，所以时间戳派生够用。
fn b64_16(seed: u64) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = [0u8; 16];
    let mut x = seed | 1;
    for b in bytes.iter_mut() {
        // xorshift，够用且不需依赖 rand
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    let mut s = String::with_capacity(24);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            s.push(T[(n >> 6) as usize & 63] as char);
        } else {
            s.push('=');
        }
        if chunk.len() > 2 {
            s.push(T[n as usize & 63] as char);
        } else {
            s.push('=');
        }
    }
    s
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 未授权页面。
const AUTH_PAGE: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>需要口令 · Antigravity Hub</title>
<style>
  html,body{height:100%;margin:0}
  body{display:flex;align-items:center;justify-content:center;
       font:16px/1.6 -apple-system,BlinkMacSystemFont,"PingFang SC",sans-serif;
       background:#f5f5f7;color:#1d1d1f}
  .box{max-width:420px;padding:32px 28px;background:#fff;border-radius:16px;
       box-shadow:0 4px 24px rgba(0,0,0,.08);text-align:center}
  h1{font-size:19px;margin:0 0 12px}
  p{font-size:14px;color:#6e6e73;margin:0 0 8px}
  code{background:#f5f5f7;padding:2px 6px;border-radius:5px;font-size:13px}
  .hint{margin-top:16px;font-size:13px;color:#86868b}
</style></head><body>
<div class="box">
  <h1>需要口令才能进入</h1>
  <p>请使用电脑上 AgyHub 给出的链接打开，链接里带有 <code>?t=</code> 口令。</p>
  <p>以前能打开、现在打不开？口令可能刚被更换过。</p>
  <p class="hint">在电脑屏幕右上角菜单栏点 AgyHub 图标 →「复制链接」或「显示二维码」，重新打开即可。</p>
</div></body></html>
"#;
