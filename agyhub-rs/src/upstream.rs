//! 配置与上游（language_server）发现。
//!
//! 上游是 Antigravity 自己的 language_server，它同时开两个端口：
//!   - https_server_port：HTTPS，桌面版窗口连这个
//!   - http_server_port：明文 HTTP，**网关要走这个**
//!
//! 两个都是启动时随机分配的（启动参数 `--https_server_port 0`），
//! 所以只能运行时扫描 + 探测。

use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::Command;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

/// 上游永远在本机回环。
pub const UPSTREAM_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// 探测超时。太短会在机器繁忙时误判，太长会拖慢启动。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// 缓存的上游端口，避免每个请求都去 lsof 一遍。
static CACHED_PORT: AtomicU16 = AtomicU16::new(0);
static CACHED_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn cache_get() -> Option<u16> {
    let p = CACHED_PORT.load(Ordering::Relaxed);
    if p == 0 {
        return None;
    }
    let at = CACHED_AT.lock().ok()?.as_ref().copied()?;
    if at.elapsed() < Duration::from_secs(30) {
        Some(p)
    } else {
        None
    }
}

fn cache_put(port: u16) {
    CACHED_PORT.store(port, Ordering::Relaxed);
    if let Ok(mut g) = CACHED_AT.lock() {
        *g = Some(Instant::now());
    }
}

fn cache_clear() {
    CACHED_PORT.store(0, Ordering::Relaxed);
    if let Ok(mut g) = CACHED_AT.lock() {
        *g = None;
    }
}

/// 让缓存立即失效（连上游失败时调用：Antigravity 可能重启换了端口）。
pub fn invalidate() {
    cache_clear();
}

/// 上次找到的上游端口，跨进程保存。
///
/// `status` 每 3 秒跑一次、每次都是新进程，没有内存缓存可用。
/// 先探一下上次的端口（一次本地 HTTP 请求，约 1ms），
/// 还能用就不必跑一遍 lsof。
fn last_known_path() -> std::path::PathBuf {
    crate::sys::run_dir().join("upstream")
}

fn last_known() -> Option<u16> {
    std::fs::read_to_string(last_known_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn save_last_known(p: u16) {
    let _ = std::fs::create_dir_all(crate::sys::run_dir());
    let _ = std::fs::write(last_known_path(), p.to_string());
}

/// 列出所有 language_server 在监听的端口。
///
/// 用 lsof 而不是遍历 /proc，因为 macOS 没有 /proc。
/// 只保留 127.0.0.1 上的 —— 上游只监听回环。
fn list_listen_ports() -> Vec<u16> {
    let mut cmd = Command::new("/usr/sbin/lsof");
    cmd.args(["-nP", "-iTCP", "-sTCP:LISTEN"]);
    // lsof 在文件句柄很多的机器上可能要跑好几秒，必须有上限
    let text = match crate::sys::output_timeout(cmd, Duration::from_secs(5)) {
        Some(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        None => return vec![],
    };

    let mut ports = Vec::new();
    for line in text.lines() {
        if !line.contains("language_") {
            continue;
        }
        // 形如 `language_ 1234 user 3u IPv4 0x.. 0t0 TCP 127.0.0.1:62134 (LISTEN)`
        if let Some(idx) = line.find("127.0.0.1:") {
            let rest = &line[idx + "127.0.0.1:".len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = num.parse::<u16>() {
                if !ports.contains(&p) {
                    ports.push(p);
                }
            }
        }
    }
    ports.sort_unstable();
    ports
}

/// 探测某端口是不是 Antigravity 的明文 HTTP 上游。
///
/// 判据是响应里含 `csrfToken`。这个字符串只出现在 language_server
/// 返回的 index.html 里，误判概率极低。
///
/// HTTPS 端口探不到（我们发明文请求过去会被拒），正好用来区分两个端口。
pub fn probe_http(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = SocketAddrV4::new(UPSTREAM_HOST, port);
    let mut sock = match TcpStream::connect_timeout(&addr.into(), PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = sock.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = sock.set_write_timeout(Some(PROBE_TIMEOUT));

    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        UPSTREAM_HOST, port
    );
    if sock.write_all(req.as_bytes()).is_err() {
        return false;
    }

    let mut buf = [0u8; 4096];
    let mut total = Vec::new();
    // 读够 4KB 或者连接关闭就停，不用等完整响应
    for _ in 0..4 {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total.extend_from_slice(&buf[..n]);
                if total.len() >= 4096 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let body = String::from_utf8_lossy(&total);
    body.contains("csrfToken")
}

/// 找到可用的明文 HTTP 上游端口。
///
/// 必须走明文 HTTP：网关到上游用 HTTP/1.1，逐请求建连接，
/// 自己管理连接池，不受浏览器 6 连接限制。走 HTTPS 还要再套一层 TLS，
/// 纯属多余且握手开销大。
/// 远端模式下的固定上游端口。
///
/// ## 为什么要单独记这个
///
/// 本机模式的上游是"扫出来的"——遍历所有监听端口，看谁响应里有
/// csrfToken。但 SSH 模式的上游是**隧道映射出来的固定端口**，
/// 不能也不该去扫：
///
///   · 扫会扫到本机 Antigravity，把手机指到错误的机器上去
///   · 隧道端口由 RemoteTunnel 决定，扫也扫不出正确值
///
/// 所以 SSH 模式启动时调用 `set_remote()` 钉死端口，
/// `find_upstream()` 见到它就直连返回，跳过所有探测。
static REMOTE_UPSTREAM: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// 设定远端模式的上游端口（SSH 隧道映射出来的那个）。
pub fn set_remote(port: u16) {
    REMOTE_UPSTREAM.store(port, std::sync::atomic::Ordering::SeqCst);
}

/// 当前是否是远端模式，返回其上游端口。
pub fn remote_port() -> Option<u16> {
    match REMOTE_UPSTREAM.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        p => Some(p),
    }
}

/// 远端 language_server 的主 CSRF 口令（见 `tunnel::RemoteTunnel::csrf_token`）。
/// 隧道每次（重）连时更新：远端服务重启后口令会变。
static REMOTE_CSRF: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_remote_csrf(token: &str) {
    *REMOTE_CSRF.lock().unwrap() = token.to_string();
}

pub fn remote_csrf() -> String {
    REMOTE_CSRF.lock().unwrap().clone()
}

/// 本机 language_server 的主口令（从 discovery 文件里读）。
///
/// ## 为什么需要
///
/// Antigravity 2.34.0 起，**本机模式**的首页 `__APP_CONFIG__.csrfToken` 也被置空了
/// （2.33.0 只在 SSH 部署的远端服务上这么做）。首页拿不到口令时，前端 `Q9()`
/// 三处回退全部落空 → 抛 `Missing LS CSRF token cookie` → 建不出 gRPC 客户端 → 白屏。
///
/// 口令本身还在 discovery 文件里：
/// `~/.gemini/antigravity/daemon/hub_<httpPort>.json` → `csrfToken`
/// （文件名就是明文 HTTP 端口，正好等于我们连的上游端口）。
///
/// ## 为什么不走 /auth/mint
///
/// 桌面版主进程那套一次性令牌要多两步 HTTP（mint → 换 sessionToken），
/// 而换出来的 sessionToken 实测**就等于这里的 csrfToken**。直接读文件，
/// 少两次往返、少一个失败点。
///
/// 解析不了就返回 None（退回到不改首页，即 2.34.0 之前的行为）。
pub fn local_csrf(port: u16) -> Option<String> {
    let path = crate::sys::home()
        .join(".gemini")
        .join("antigravity")
        .join("daemon")
        .join(format!("hub_{}.json", port));
    // 这份文件由 Antigravity 写成 0600，读不到就当作没有。
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // 端口要对得上：目录里堆着历次启动的文件，别拿错某一代的口令。
    if v.get("httpPort").and_then(|p| p.as_u64()) != Some(port as u64) {
        return None;
    }
    let token = v.get("csrfToken").and_then(|t| t.as_str())?;
    // 口令要原样拼进页面 JS 里，只接受 UUID 那类字符（和 tunnel 里的校验一致）。
    let safe = !token.is_empty()
        && token.len() <= 128
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe {
        Some(token.to_string())
    } else {
        None
    }
}

pub fn find_upstream(force: bool) -> Option<u16> {
    // 远端模式：上游固定，直接返回。
    //
    // 注意这里**不看 force** —— 远端端口钉死之后不会变，
    // 隧道重连也还是同一个 local_port（RemoteTunnel 内部保证）。
    // 强扫反而会误判成本机的 Antigravity。
    if let Some(p) = remote_port() {
        return Some(p);
    }

    if !force {
        if let Some(p) = cache_get() {
            return Some(p);
        }
    }

    // 同一进程里同一时刻只做一次完整探测。
    // 缓存过期那一瞬间，浏览器可能有十几个请求同时进来，
    // 不加锁就是十几个 lsof 同时跑。
    static DISCOVER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = DISCOVER.lock().unwrap_or_else(|e| e.into_inner());
    if !force {
        if let Some(p) = cache_get() {
            return Some(p);
        }
    }

    // 1. 上次的端口还能用就直接用
    let mut tried: Vec<u16> = Vec::new();
    if let Some(p) = last_known() {
        tried.push(p);
        if probe_http(p) {
            cache_put(p);
            return Some(p);
        }
    }

    // 2. lsof 列出 language_server 的监听端口逐个探
    for p in list_listen_ports() {
        if tried.contains(&p) {
            continue;
        }
        tried.push(p);
        if probe_http(p) {
            cache_put(p);
            save_last_known(p);
            return Some(p);
        }
    }

    // 3. lsof 不可用（被管控软件拦截 / 超时）时的兜底：
    //    从 Antigravity 调试端口的页面列表里拿到 HTTPS 端口，
    //    明文 HTTP 端口通常就在它旁边（实测为 HTTPS 端口 +1）。
    for p in candidates_from_devtools() {
        if tried.contains(&p) {
            continue;
        }
        tried.push(p);
        if probe_http(p) {
            cache_put(p);
            save_last_known(p);
            return Some(p);
        }
    }

    cache_clear();
    None
}

/// 调试端口是否真的可用：文件在，且端口上确实有人在监听。
///
/// ## 为什么要单独判断
///
/// Antigravity 2.33.0 起，只有带 `--remote-debugging-port` 启动时才开调试端口，
/// 不带的话启动时还会主动删掉 DevToolsActivePort。从程序坞打开、自动更新后重启
/// 都不带这个参数 → 桌面版注入全部做不了。菜单要能说清楚是这个原因，
/// 而不是笼统地显示"正在生效…"。
///
/// 只看文件不够：Antigravity 崩溃时文件可能残留，端口却早没了。
pub fn devtools_ok() -> bool {
    devtools_port().map(crate::sys::tcp_listening).unwrap_or(false)
}

/// 官方是否已经放开桌面版 SSH 入口。
///
/// - `Some(true)`：服务端 `GetMendelFlags` 里 `enable-ssh-remote` 没带 `disabled`
///   （2.33.0 起官方正式开放 SSH），不需要再注入钩子
/// - `Some(false)`：仍带 `disabled: true`，需要注入
/// - `None`：查不到（Antigravity 没开、没登录、接口变了），按老逻辑走
///
/// 直接查开关而不是比版本号：官方哪天收回、或者灰度到一部分账号，都能如实反映。
///
/// 结果按上游端口缓存 10 分钟（`run/ssh_flag`）：响应约 80KB，
/// `status` 每 3 秒跑一次，不能每次都拉。端口变了 = Antigravity 重启过，重新查。
pub fn ssh_official(upstream_port: Option<u16>) -> Option<bool> {
    let port = upstream_port?;
    let cache = crate::sys::run_dir().join("ssh_flag");
    let now = crate::sys::now_secs();
    if let Ok(s) = std::fs::read_to_string(&cache) {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() == 3 {
            let (p, ts, v) = (f[0].parse::<u16>().ok(), f[1].parse::<u64>().ok(), f[2]);
            if p == Some(port) && ts.map(|t| now.saturating_sub(t) < 600).unwrap_or(false) {
                return Some(v == "1");
            }
        }
    }
    let v = query_ssh_flag(port)?;
    let _ = std::fs::create_dir_all(crate::sys::run_dir());
    let _ = std::fs::write(&cache, format!("{} {} {}", port, now, if v { 1 } else { 0 }));
    Some(v)
}

/// 真正去问 language_server。走明文 HTTP 端口，和网关用的是同一个。
fn query_ssh_flag(port: u16) -> Option<bool> {
    // 1. 首页里拿 csrfToken（接口要求带 x-codeium-csrf-token 头）
    let index = http_simple(port, "GET", "/", &[], b"", 64 * 1024)?;
    let key = "\"csrfToken\":\"";
    let i = index.find(key)? + key.len();
    let token: String = index[i..].chars().take_while(|c| *c != '"').collect();
    if token.is_empty() {
        return None;
    }

    // 2. 用 Connect 协议的普通 JSON 调用（不是 grpc-web 帧），响应就是一个 JSON 对象
    let body = http_simple(
        port,
        "POST",
        "/exa.language_server_pb.LanguageServerService/GetMendelFlags",
        &[
            ("Content-Type", "application/json"),
            ("x-codeium-csrf-token", token.as_str()),
        ],
        b"{}",
        2 * 1024 * 1024,
    )?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let exps = v.pointer("/experimentConfig/experiments")?.as_array()?;
    for e in exps {
        if e.get("keyString").and_then(|k| k.as_str()) == Some("enable-ssh-remote") {
            let disabled = e.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false);
            return Some(!disabled);
        }
    }
    // 列表里根本没有这一项：前端拿不到 flag，入口不会显示，等同于没放开
    Some(false)
}

/// 极简阻塞 HTTP/1.0 请求，返回响应体。
///
/// 用 HTTP/1.0：服务器发完就关连接，读到 EOF 即结束，不用解析 chunked / keep-alive。
/// 不发 Accept-Encoding，响应不会被 gzip。
fn http_simple(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    max_bytes: usize,
) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = SocketAddrV4::new(UPSTREAM_HOST, port);
    let mut s = TcpStream::connect_timeout(&addr.into(), PROBE_TIMEOUT).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(3)));

    let mut req = format!(
        "{} {} HTTP/1.0\r\nHost: {}:{}\r\nContent-Length: {}\r\n",
        method, path, UPSTREAM_HOST, port, body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).ok()?;
    s.write_all(body).ok()?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline || buf.len() > max_bytes {
            return None;
        }
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.") || !head.split_whitespace().nth(1).map(|c| c == "200").unwrap_or(false) {
        return None;
    }
    Some(rest.to_string())
}

/// 读 Antigravity 的调试端口（DevToolsActivePort 第一行）。
pub fn devtools_port() -> Option<u16> {
    let path = crate::sys::home()
        .join("Library/Application Support/Antigravity/DevToolsActivePort");
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// 从调试端口的 /json/list 推出上游候选端口（不依赖 lsof）。
fn candidates_from_devtools() -> Vec<u16> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let port = match devtools_port() {
        Some(p) => p,
        None => return vec![],
    };
    let addr = SocketAddrV4::new(UPSTREAM_HOST, port);
    let mut s = match TcpStream::connect_timeout(&addr.into(), PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(PROBE_TIMEOUT));
    let req = format!(
        "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    );
    if s.write_all(req.as_bytes()).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    let _ = s.take(1 << 20).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);

    let mut bases: Vec<u16> = Vec::new();
    for marker in ["https://127.0.0.1:", "http://127.0.0.1:"] {
        let mut rest: &str = &text;
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u16>() {
                if p != port && !bases.contains(&p) {
                    bases.push(p);
                }
            }
        }
    }

    let mut out = Vec::new();
    for b in bases {
        for delta in [1i32, -1, 2, -2, 3, 0] {
            let p = b as i32 + delta;
            if (1..=65535).contains(&p) {
                let p = p as u16;
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// 上游进程是否还活着（用来判断 Antigravity 有没有退出）。
///
/// 目前没有调用方：网关的设计是「每个请求自己探，探不到就 502」，
/// 这样 Antigravity 重启换了端口也能自动跟上，比缓存一个布尔值可靠。
/// 保留它是为了将来做菜单栏状态灯。
#[allow(dead_code)]
pub fn upstream_alive() -> bool {
    find_upstream(true).is_some()
}
