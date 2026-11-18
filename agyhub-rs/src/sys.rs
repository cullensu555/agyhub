//! 系统层小工具：带超时的外部命令、进程身份、随机数、端口探测、目录。
//!
//! ## 为什么单独抽一个模块
//!
//! 早期版本到处直接 `Command::new("ps"/"lsof"/"openssl"/"pgrep").output()`，
//! 实机分发后暴露出两类问题：
//!
//!   1. **没有超时**。某些机器上 lsof 要跑好几秒，openssl s_client 遇到
//!      不回话的端口会一直挂着。status 每 3 秒跑一次，一挂就把菜单栏卡死。
//!   2. **外部命令可能被拦**。企业管控软件 / 沙箱环境下 `ps` 会报
//!      `operation not permitted`，于是"服务明明在跑却显示未开启"。
//!
//! 现在的原则：
//!   · 能用系统调用（libc）解决的不调外部命令：进程是否存活、可执行文件路径、
//!     随机数、端口监听都走 syscall。
//!   · 必须调外部命令的（lsof、security、ssh）一律走 [`output_timeout`]。

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// 跑一个外部命令，超时就杀掉并返回 None。
///
/// stdout/stderr 用独立线程读，避免管道写满导致子进程阻塞（经典死锁）。
pub fn output_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let mut so = child.stdout.take()?;
    let mut se = child.stderr.take()?;
    // 读管道的线程通过 channel 交回结果，主线程**有上限地**等。
    //
    // 不能直接 join：子进程退出后，它派生的孙进程（比如 ssh 的 ProxyJump、
    // ControlPersist 后台连接）可能还握着管道的写端，read_to_end 就永远读不到 EOF。
    let (tx_out, rx_out) = std::sync::mpsc::channel();
    let (tx_err, rx_err) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        let _ = tx_out.send(v);
    });
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        let _ = tx_err.send(v);
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break None,
        }
    };
    let stdout = rx_out.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let stderr = rx_err.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    Some(Output {
        status: status?,
        stdout,
        stderr,
    })
}

/// 进程是否存活（不需要 ps）。
///
/// `kill(pid, 0)` 不发信号，只做权限与存在性检查：
/// 返回 0 = 存在；EPERM = 存在但不是我们的用户；ESRCH = 不存在。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 进程的可执行文件路径（用 proc_pidpath，不需要 ps）。
pub fn pid_exe(pid: u32) -> Option<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// 进程名（内核里记录的 p_comm，最多 16 字节）。
///
/// 可执行文件被删掉之后 proc_pidpath 会失败，但进程名还在 ——
/// App 升级时旧的 .app 被整个删除，旧内核进程还在跑，就只能靠这个认出来。
pub fn pid_name(pid: u32) -> Option<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        libc::proc_name(
            pid as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// 进程启动时间（秒）。pid 会被系统复用，登记时记下启动时间，
/// 查询时对得上才认为是同一个进程。
pub fn pid_start_time(pid: u32) -> Option<u64> {
    bsdinfo(pid).map(|i| i.pbi_start_tvsec)
}

/// 进程属于哪个用户。
pub fn pid_uid(pid: u32) -> Option<u32> {
    bsdinfo(pid).map(|i| i.pbi_uid)
}

fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    Some(info)
}

/// 这个 pid 是不是 agyhub 内核（任何版本、任何安装位置）。
///
/// ⚠️ 大小写敏感：菜单栏 App 本身叫 `AgyHub`，内核叫 `agyhub`，
/// 不区分的话会把 App 自己当成内核去停。
pub fn is_our_pid(pid: u32) -> bool {
    // 只认当前用户的进程：同一台 Mac 上另一个账号也在用 AgyHub 时，
    // 它的网关不归我们管（也停不掉）。
    if pid_uid(pid) != Some(unsafe { libc::getuid() }) {
        return false;
    }
    let name = match pid_exe(pid) {
        Some(p) => p.rsplit('/').next().unwrap_or("").to_string(),
        None => match pid_name(pid) {
            Some(n) => n,
            None => return false,
        },
    };
    name == "agyhub" || name.starts_with("agyhub-")
}

/// 列出所有可执行文件路径满足条件的进程（不需要 pgrep）。
pub fn find_pids<F: Fn(&str) -> bool>(pred: F) -> Vec<u32> {
    let mut out = Vec::new();
    // 先问需要多大缓冲
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return out;
    }
    let cap = (n as usize) + 64;
    let mut pids = vec![0 as libc::pid_t; cap];
    let got = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if got <= 0 {
        return out;
    }
    for &pid in pids.iter().take(got as usize) {
        if pid <= 0 {
            continue;
        }
        if let Some(path) = pid_exe(pid as u32) {
            if pred(&path) {
                out.push(pid as u32);
            }
        }
    }
    out
}

/// 发信号并等进程退出。返回进程最终是否已退出。
pub fn terminate(pid: u32, grace: Duration) -> bool {
    if !pid_alive(pid) {
        return true;
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let start = Instant::now();
    while start.elapsed() < grace {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
    !pid_alive(pid)
}

/// 本机回环上这个端口有没有人在监听。
pub fn tcp_listening(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 用系统随机源生成 n 字节，返回十六进制串。
///
/// 访问口令是局域网里唯一的门槛（网关后面是能执行命令的 AI agent），
/// 不能用时间戳派生的伪随机 —— 那样同一秒内启动的口令可被推算。
pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // 极端兜底：/dev/urandom 都读不了。用 arc4random（macOS 自带 CSPRNG）。
        for b in buf.iter_mut() {
            *b = (unsafe { libc::arc4random() } & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 当前时间（秒）。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// stdout 是不是终端（用来决定要不要把带口令的链接打出来）。
pub fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// 数据目录：`~/.agyhub`
pub fn data_dir() -> PathBuf {
    home().join(".agyhub")
}

/// 运行时状态目录：`~/.agyhub/run`（pid 文件、注入状态等）
pub fn run_dir() -> PathBuf {
    data_dir().join("run")
}

/// 日志目录：`~/Library/Logs`
pub fn logs_dir() -> PathBuf {
    home().join("Library").join("Logs")
}

/// 日志超过上限就滚动一份（只留一份旧的），避免无限增长。
pub fn rotate_log(path: &std::path::Path, max_bytes: u64) {
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > max_bytes {
            let old = path.with_extension("log.1");
            let _ = std::fs::rename(path, old);
        }
    }
}

/// 收紧文件权限为仅本人可读写。
pub fn chmod_600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(path) {
        let mut p = md.permissions();
        p.set_mode(0o600);
        let _ = std::fs::set_permissions(path, p);
    }
}

/// 收紧目录权限为仅本人可访问。
pub fn chmod_700(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(path) {
        let mut p = md.permissions();
        p.set_mode(0o700);
        let _ = std::fs::set_permissions(path, p);
    }
}
