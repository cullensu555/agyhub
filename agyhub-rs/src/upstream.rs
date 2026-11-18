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

/// 列出所有 language_server 在监听的端口。
///
/// 用 lsof 而不是遍历 /proc，因为 macOS 没有 /proc。
/// 只保留 127.0.0.1 上的 —— 上游只监听回环。
fn list_listen_ports() -> Vec<u16> {
    let out = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output();

    let text = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return vec![],
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
    // 同时写进缓存，让已经拿到旧值的调用方也能尽快纠正
    cache_put(port);
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

    for p in list_listen_ports() {
        if probe_http(p) {
            cache_put(p);
            return Some(p);
        }
    }

    cache_clear();
    None
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
