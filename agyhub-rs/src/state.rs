//! 运行时状态：服务进程登记（pid 文件）与注入状态。
//!
//! ## 为什么要 pid 文件
//!
//! 早期判断"服务在不在跑"靠三步推理：lsof 拿到监听端口的 pid →
//! `ps` 看命令行 → 命令行里有没有 agyhub。实机分发后这条链路三处都会断：
//!
//!   · `ps` 被企业管控/沙箱拦截（operation not permitted）
//!   · lsof 在某些机器上很慢或拿不到结果
//!   · 最后一次"修复"又加了 `openssl s_client`，它没有超时，遇到不回话
//!     的端口能把 status 挂死；兜底分支还把**任何** TLS 服务都当成自己的
//!
//! 现在由服务进程自己登记：启动时写 `~/.agyhub/run/<kind>.json`
//! （pid、端口、模式），退出时删掉。查询方只需要：
//!   1. 读文件
//!   2. `kill(pid, 0)` 看进程在不在（syscall，不调外部命令）
//!   3. `proc_pidpath(pid)` 确认是 agyhub（防 pid 复用）
//!   4. 端口能连上 = 已就绪
//!
//! 老版本起的进程没有 pid 文件，退回到 lsof + proc_pidpath 兜底识别，
//! 保证升级时能接管旧进程。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::sys;

/// 服务种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 本机网页版
    Web,
    /// SSH 网页版
    SshWeb,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Web => "web",
            Kind::SshWeb => "ssh_web",
        }
    }
}

/// 服务进程登记信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub pid: u32,
    pub port: u16,
    /// 端口是否已经开始监听（SSH 模式要先建隧道，可能要十几秒）
    #[serde(default)]
    pub ready: bool,
    /// SSH 模式下连的开发机
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub started: u64,
    /// 启动这个服务的内核文件标识（版本 + 修改时间 + 大小）。
    /// App 升级后内核文件换了，旧进程还在跑旧代码 —— 靠它识别出来并重启。
    #[serde(default)]
    pub exe_id: String,
    /// 进程启动时间（proc_pidinfo）。pid 会被系统复用：电脑重启、进程被强杀后
    /// 登记文件还在，同一个 pid 可能已经是别的程序了。对上启动时间才算同一个进程。
    #[serde(default)]
    pub start_time: u64,
}

/// 登记信息里的进程是否还是它本人。
fn same_process(info: &RunInfo) -> bool {
    if !sys::pid_alive(info.pid) || !sys::is_our_pid(info.pid) {
        return false;
    }
    if info.start_time != 0 {
        if let Some(t) = sys::pid_start_time(info.pid) {
            return t == info.start_time;
        }
    }
    true
}

/// 当前内核可执行文件的标识。
pub fn current_exe_id() -> String {
    let exe = std::env::current_exe().ok();
    let md = exe.as_ref().and_then(|p| std::fs::metadata(p).ok());
    let (mtime, size) = md
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (t, m.len())
        })
        .unwrap_or((0, 0));
    format!("{}-{}-{}", crate::VERSION, mtime, size)
}

/// 服务当前的实际情况。
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub pid: u32,
    pub port: u16,
    /// 已在监听，可以访问
    pub running: bool,
    /// 进程在，但还没开始监听（正在建隧道等）
    pub starting: bool,
    pub target: String,
    /// 是不是当前这份内核文件起的（false = 老版本留下的进程，需要重启）
    pub current: bool,
}

fn info_path(kind: Kind) -> PathBuf {
    sys::run_dir().join(format!("{}.json", kind.name()))
}

fn error_path(kind: Kind) -> PathBuf {
    sys::run_dir().join(format!("{}.error", kind.name()))
}

fn ensure_run_dir() {
    let d = sys::run_dir();
    let _ = std::fs::create_dir_all(&d);
    sys::chmod_700(&sys::data_dir());
}

/// 服务进程登记自己。
pub fn register(kind: Kind, info: &RunInfo) {
    ensure_run_dir();
    let p = info_path(kind);
    let tmp = p.with_extension("json.tmp");
    if let Ok(s) = serde_json::to_string(info) {
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// 服务进程退出时注销（只删属于自己 pid 的那份，避免误删新进程的登记）。
pub fn unregister(kind: Kind, pid: u32) {
    if let Some(info) = read_info(kind) {
        if info.pid == pid {
            let _ = std::fs::remove_file(info_path(kind));
        }
    }
}

fn read_info(kind: Kind) -> Option<RunInfo> {
    let s = std::fs::read_to_string(info_path(kind)).ok()?;
    serde_json::from_str(&s).ok()
}

/// 记录最近一次启动失败的原因（菜单里显示给用户看）。
pub fn set_error(kind: Kind, msg: &str) {
    ensure_run_dir();
    let _ = std::fs::write(error_path(kind), msg);
}

pub fn clear_error(kind: Kind) {
    let _ = std::fs::remove_file(error_path(kind));
}

pub fn last_error(kind: Kind) -> String {
    std::fs::read_to_string(error_path(kind))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 查服务实际状态。
///
/// `fallback_port`：没有登记文件时（老版本起的进程），去这个端口上兜底识别。
pub fn live(kind: Kind, fallback_port: u16) -> Live {
    if let Some(info) = read_info(kind) {
        if same_process(&info) {
            let listening = info.ready && sys::tcp_listening(info.port);
            return Live {
                pid: info.pid,
                port: info.port,
                running: listening,
                starting: !info.ready,
                target: info.target,
                current: !info.exe_id.is_empty() && info.exe_id == current_exe_id(),
            };
        }
        // 进程已经不在了（崩溃 / 被强杀）：登记文件过期，清掉
        let _ = std::fs::remove_file(info_path(kind));
    }

    // ---- 兜底：老版本起的进程（没有登记文件）----
    //
    // ⚠️ 要排除另一个服务登记在案的进程：端口配置被改得互相重叠时，
    // 这个端口上可能是 SSH 网页版，不能把它当成"旧版本的网页版"停掉。
    if fallback_port != 0 && sys::tcp_listening(fallback_port) {
        if let Some(pid) = listen_pid(fallback_port) {
            let other_registered = [Kind::Web, Kind::SshWeb]
                .iter()
                .filter(|k| **k != kind)
                .filter_map(|k| read_info(*k))
                .any(|i| i.pid == pid);
            if !other_registered && sys::is_our_pid(pid) {
                return Live {
                    pid,
                    port: fallback_port,
                    running: true,
                    starting: false,
                    target: String::new(),
                    current: false,
                };
            }
        }
    }
    Live::default()
}

/// 拿监听该端口的 pid（lsof，带超时；被拦或超时返回 None）。
pub fn listen_pid(port: u16) -> Option<u32> {
    let mut cmd = std::process::Command::new("/usr/sbin/lsof");
    cmd.args(["-nP", "-t", &format!("-iTCP:{}", port), "-sTCP:LISTEN"]);
    let out = sys::output_timeout(cmd, Duration::from_secs(3))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|s| s.trim().parse().ok())
}

/// 查监听该端口的进程名（只用于给用户看"被谁占了"）。
pub fn port_holder_name(port: u16) -> String {
    match listen_pid(port) {
        Some(pid) => sys::pid_exe(pid)
            .map(|p| p.rsplit('/').next().unwrap_or("").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// 停掉某个服务。只停**登记在案或确认是 agyhub** 的进程，绝不按端口乱杀。
///
/// 返回被停掉的 pid（没在跑返回 None）。
pub fn stop(kind: Kind, fallback_port: u16) -> Option<u32> {
    let l = live(kind, fallback_port);
    if l.pid == 0 {
        return None;
    }
    sys::terminate(l.pid, Duration::from_secs(3));
    unregister(kind, l.pid);
    Some(l.pid)
}

// ============================================================
// 注入状态（桌面版 SSH 入口 / 中文）
// ============================================================

/// 最近一次注入巡检的结果。
///
/// 注入本身没有常驻进程，所以"是否生效"只能靠巡检结果判断：
/// 巡检时间够新、且 Antigravity 没换过调试端口（没重启过），才算生效。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InjectState {
    #[serde(default)]
    pub ts: u64,
    /// 巡检时 Antigravity 的调试端口。端口变了 = Antigravity 重启过，结果作废。
    #[serde(default)]
    pub devtools_port: u16,
    #[serde(default)]
    pub pages: usize,
    #[serde(default)]
    pub zh: bool,
    #[serde(default)]
    pub ssh: bool,
    /// SSH 钩子至少在一个页面上真正生效过（入口已经出现）
    #[serde(default)]
    pub ssh_active: bool,
    /// 用量指示器是否已经装到所有页面
    #[serde(default)]
    pub usage: bool,
    #[serde(default)]
    pub error: String,
    /// 每个页面最近一次因为 SSH 解锁而刷新的时间（防止反复刷新）
    #[serde(default)]
    pub reloaded: std::collections::HashMap<String, u64>,
}

fn inject_path() -> PathBuf {
    sys::run_dir().join("inject.json")
}

pub fn inject_load() -> InjectState {
    std::fs::read_to_string(inject_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn inject_save(st: &InjectState) {
    ensure_run_dir();
    let p = inject_path();
    let tmp = p.with_extension("json.tmp");
    if let Ok(s) = serde_json::to_string(st) {
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// 巡检结果是否还有效。
pub fn inject_fresh(st: &InjectState, current_devtools_port: Option<u16>) -> bool {
    let age_ok = sys::now_secs().saturating_sub(st.ts) <= 60;
    let port_ok = current_devtools_port.map(|p| p == st.devtools_port).unwrap_or(false);
    age_ok && port_ok
}
