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
