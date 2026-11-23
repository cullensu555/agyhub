//! Antigravity Hub —— 把 Antigravity 的网页界面开放给手机和局域网。
//!
//! 架构：
//!
//!     浏览器 --HTTPS/HTTP2--> 本进程 --HTTP/1.1--> language_server
//!
//! 为什么浏览器侧必须 HTTP/2：
//!   前端会同时开 6 个永不结束的流式订阅。HTTP/1.1 下浏览器对同一域名
//!   只给 6 个并发连接，被订阅占满后，后续请求（发消息、取认证状态）
//!   全部排队超时 —— 表现为「页面能看，一操作就卡」。
//!
//! 为什么上游侧走 HTTP/1.1：
//!   每个请求由本进程自己建连、自己管，不受浏览器连接数限制。
//!
//! ## 进程管理由内核负责
//!
//! 菜单栏 App 不再自己 fork/kill 服务进程，而是调用：
//!   `start-web` / `stop-web` / `start-ssh-web` / `stop-ssh-web` / `stop`
//! 内核用 pid 文件（`~/.agyhub/run/*.json`）登记自己起的进程，
//! 所以启动是幂等的（已在跑就复用），停止只会停自己登记的进程，
//! 不会再按端口误杀用户的其他程序。

mod actions;
mod cdp;
mod cert;
mod config;
mod inject;
mod proxy;
mod server;
mod state;
mod sys;
mod trust;
mod tunnel;
mod upstream;

use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use state::Kind;

/// 内核版本。发版时由 build.sh 传入 AGYHUB_VERSION，与 App 版本号保持一致
/// （原来内核固定报 0.1.0，App 是 1.1.0，用户发日志过来分不清是哪个版本）。
pub const VERSION: &str = match option_env!("AGYHUB_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // ---- 子命令解析 ----
    //
    // ⚠️ 这里出过一个严重 bug，务必理解这段为什么要这么写：
    //
    // 早期实现是 `cmd = args[1]`。当调用方传 `--port 8898 status` 时，
    // args[1] 是 "--port"，于是 cmd = "--port"，匹配不到任何已知子命令，
    // **落到默认分支直接启动服务**。
    //
    // 正确做法：**先剥掉所有以 -- 开头的开关（连同它的值），
    // 剩下的第一个位置参数才是子命令。**
    let known_cmds = [
        "run", "status", "doctor", "trust-cert", "rotate-token",
        "set-enabled", "set-port", "pick-port",
        "ssh-hosts", "start-ssh-web", "stop-ssh-web", "start-web", "stop-web", "stop",
        "zh", "ssh-unlock", "ensure-inject", "relaunch-antigravity", "usage", "help", "version",
    ];
    // 带值的开关：值要一起跳过，否则 `--target status` 里的 status 会被当成子命令
    let valued_flags = ["--port", "--target", "--ssh-target", "--wait"];
    let mut cmd = "run";
    {
        let mut i = 1; // 跳过 argv[0]
        while i < args.len() {
            let a = &args[i];
            // ⚠️ --version / --help 必须在下面 `starts_with("--")` 分支之前判断，
            // 否则 `agyhub --version` 会启动整个服务（踩过）。
            if a == "--version" || a == "-V" {
                cmd = "version";
                break;
            }
            if a == "--help" || a == "-h" {
                cmd = "help";
                break;
            }
            if a.starts_with("--") {
                if valued_flags.contains(&a.as_str()) && i + 1 < args.len() {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if known_cmds.contains(&a.as_str()) {
                cmd = a;
                // ⚠️ 找到第一个已知子命令就停（反例：`agyhub zh status`）。
                break;
            }
            i += 1;
        }
    }

    // 日志一律写 stderr：`status` 等命令要往 stdout 输出单行 JSON 给菜单栏 App 解析。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let port_arg = read_port(&args);

    match cmd {
        "doctor" => doctor(),
        // 供菜单栏 App 调用的状态查询。输出单行 JSON。
        "status" => status(),
        "rotate-token" => rotate_token(),
        "set-enabled" => set_enabled(&args),
        "pick-port" => pick_port_cmd(&args),
        "set-port" => set_port_cmd(&args),
        "ssh-hosts" => list_ssh_hosts(),
        "zh" => toggle_zh(&args),
        "usage" => toggle_usage(&args),
        "ssh-unlock" => cmd_ssh_unlock(&args),
        "ensure-inject" => ensure_inject_cmd(&args),
        // 只由用户在菜单里点击触发；守护逻辑永远不调它（会打断正在跑的 agent）
        "relaunch-antigravity" => match inject::relaunch_with_debug_port() {
            Ok(msg) => println!("{}", json!({ "ok": true, "message": msg })),
            Err(e) => println!("{}", json!({ "ok": false, "error": e })),
        },
        "start-web" => println!("{}", start_web()),
        "stop-web" => println!("{}", stop_cmd(&[Kind::Web])),
        "start-ssh-web" => println!("{}", start_ssh_web(&args)),
        "stop-ssh-web" => println!("{}", stop_cmd(&[Kind::SshWeb])),
        "stop" => println!("{}", stop_cmd(&[Kind::Web, Kind::SshWeb])),
        "trust-cert" => {
            let dir = data_dir().join("certs");
            // 先确保证书存在（有锁，不会和网关进程同时生成两张 CA）
            if let Err(e) = cert::ensure_certs(&dir) {
                println!("  ✗ 证书生成失败: {}", e);
                std::process::exit(1);
            }
            let force = args.iter().any(|a| a == "--force");
            println!();
            let ok = trust::install(&dir, force);
            println!();
            std::process::exit(if ok { 0 } else { 1 });
        }
        "version" => {
            println!("agyhub {}", VERSION);
        }
        "help" => usage(),
        _ => {
            let ssh_target = read_flag(&args, "--ssh-target");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("无法创建 tokio 运行时");
            rt.block_on(run(port_arg, ssh_target))
        }
    }
}

/// 读 `--name value` 或 `--name=value` 形式的开关值。
fn read_flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter().skip(1);
    let prefix = format!("{}=", name);
    while let Some(a) = it.next() {
        if a == name {
            if let Some(v) = it.next() {
                if !v.is_empty() && !v.starts_with("--") {
                    return Some(v.clone());
                }
            }
        } else if let Some(v) = a.strip_prefix(&prefix) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 从参数或环境变量取端口。只认 `--port N` / `--port=N` 与 AGEHUB_PORT。
///
/// 早期版本会把任意数字位置参数当端口（`agyhub set-port web 9000` 里的 9000
/// 也被认成端口），现在严格按开关匹配。
fn read_port(args: &[String]) -> Option<u16> {
    if let Some(v) = read_flag(args, "--port") {
        if let Ok(p) = v.parse() {
            return Some(p);
        }
    }
    std::env::var("AGEHUB_PORT").ok().and_then(|v| v.parse().ok())
}

fn usage() {
    println!(
        "agyhub {} — Antigravity 网页访问网关\n\n\
         用法:\n  \
           agyhub                         前台启动本机网页版（默认端口 8898）\n  \
           agyhub --port 9000             指定端口\n  \
           agyhub --ssh-target <主机>     前台启动 SSH 网页版\n  \
           agyhub status                  各服务状态（单行 JSON）\n  \
           agyhub start-web | stop-web    后台启动/停止本机网页版\n  \
           agyhub start-ssh-web --target <主机> | stop-ssh-web\n  \
           agyhub stop                    停止全部后台服务\n  \
           agyhub ssh-hosts               列出 ~/.ssh/config 里的主机\n  \
           agyhub zh on|off               桌面版中文界面\n  \
           agyhub usage on|off            网页版的上下文与用量指示器\n  \
           agyhub ssh-unlock [off]        桌面版 SSH 入口\n  \
           agyhub relaunch-antigravity    带调试端口重新打开 Antigravity（中文界面需要）\n  \
           agyhub trust-cert [--force]    把根证书装进钥匙串\n  \
           agyhub rotate-token            更换访问口令（立即生效）\n  \
           agyhub doctor                  环境自检\n\n\
         环境变量:\n  \
           AGEHUB_PORT                    端口（--port 优先）\n  \
           AGEHUB_TOKEN                   固定口令（不设则读 ~/.agyhub/config.json）\n  \
           RUST_LOG                       日志级别，如 debug\n",
        VERSION
    );
}

/// 数据目录：`~/.agyhub`
fn data_dir() -> PathBuf {
    sys::data_dir()
}

fn lan_ip() -> String {
    cert::local_ips()
        .iter()
        .find(|ip| !ip.is_loopback())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

fn service_url(lan_ip: &str, port: u16, token: &str) -> String {
    if token.is_empty() {
        format!("https://{}:{}/", lan_ip, port)
    } else {
        format!("https://{}:{}/?t={}", lan_ip, port, token)
    }
}

/// 供菜单栏 App 调用的状态查询，输出单行 JSON。
///
/// 这里每 3 秒被调用一次，所以：
///   · 不调 ps / openssl / pgrep（可能被拦截、没有超时）
///   · 服务状态读 pid 文件 + syscall 验证
///   · 上游先探上次的端口，不行再 lsof（带超时）
///   · Antigravity 没开时直接跳过上游探测
fn status() {
    let mut cfg = config::Config::load();
    cfg.normalize();
    let token = cfg.token.clone();
    let lan_ip = lan_ip();
    let cert_dir = data_dir().join("certs");

    let ag_installed = inject::antigravity_installed();
    let ag_running = inject::antigravity_running();
    let upstream_port = if ag_running {
        upstream::find_upstream(false)
    } else {
        None
    };

    let ca_trusted = trust::is_trusted(&cert_dir);
    let ca_fingerprint = cert::existing_ca_fingerprint(&cert_dir);
    let (fw_enabled, fw_blocked) = firewall_state();

    // ---- 网页版 ----
    let web = state::live(Kind::Web, cfg.web_port);
    let web_port = if web.pid > 0 { web.port } else { cfg.web_port };
    let web_url = service_url(&lan_ip, web_port, &token);
    let web_error = if web.running { String::new() } else { state::last_error(Kind::Web) };

    // ---- SSH 网页版 ----
    let sshw = state::live(Kind::SshWeb, cfg.ssh_web_port);
    let ssh_web_port = if sshw.pid > 0 { sshw.port } else { cfg.ssh_web_port };
    let ssh_web_url = if sshw.running {
        service_url(&lan_ip, ssh_web_port, &token)
    } else {
        String::new()
    };
    // 运行中也要带上错误：网关在监听，但到开发机的隧道可能断了正在重连
    // （隧道守护会写入/清除这个错误），菜单里要能看到。
    let ssh_web_error = if sshw.starting {
        String::new()
    } else {
        state::last_error(Kind::SshWeb)
    };

    // ---- 桌面版注入（SSH 入口 / 中文）----
    //
    // 没有常驻进程，"是否生效"看最近一次巡检结果：
    // 结果够新（60 秒内）且 Antigravity 没重启过（调试端口没变）才算数。
    // 原来这里 pgrep 一个早已废弃的 node 脚本，于是 SSH 永远显示"未运行"。
    let devtools = upstream::devtools_port();
    let devtools_ok = ag_running && upstream::devtools_ok();
    // 2.33.0 起官方开放了 SSH：服务端开关已打开时不需要注入，直接算生效
    let ssh_official = ag_running && upstream::ssh_official(upstream_port) == Some(true);
    let inj = state::inject_load();
    let fresh = ag_running && state::inject_fresh(&inj, devtools);
    let ssh_running = cfg.ssh_enabled && (ssh_official || (fresh && inj.ssh && inj.ssh_active));
    let zh_running = cfg.zh_enabled && fresh && inj.zh;
    // 桌面版用量指示器是否已生效（网页版那半边永远算在 running 里，见 status 组装）
    let usage_running = cfg.usage_enabled && fresh && inj.usage;
    let ssh_pending = cfg.ssh_enabled && !ssh_official && fresh && inj.ssh && !inj.ssh_active;
    // 需要注入、但 Antigravity 没开调试端口 → 只能带参数重开一次。
    // 用量指示器也算在内：桌面版同样靠 CDP 注入。
    let need_relaunch = ag_running
        && !devtools_ok
        && ((cfg.zh_enabled && !zh_running)
            || (cfg.ssh_enabled && !ssh_running)
            || (cfg.usage_enabled && !usage_running));
    let inject_detail = |enabled: bool, running: bool, ok_text: &str| -> String {
        if !enabled {
            String::new()
        } else if !ag_running {
            "等待 Antigravity 打开".into()
        } else if running {
            ok_text.into()
        } else if !devtools_ok {
            // 2.33.0 起不带参数启动就没有调试端口，注入做不了。
            // 原来这里会落到"正在生效…"，用户看不出卡在哪。
            "Antigravity 没开调试端口，需要用下面的按钮重新打开一次".into()
        } else if ok_text == "已解锁" && ssh_pending {
            "已就绪 · 在 Antigravity 里切换一次对话后出现".into()
        } else if fresh && !inj.error.is_empty() {
            inj.error.chars().take(120).collect()
        } else {
            "正在生效…".into()
        }
    };
    let ssh_ok_text = if ssh_official { "官方已开放" } else { "已解锁" };

    let obj = json!({
        "version": VERSION,
        // 兼容旧字段：顶层 running/port/url 指向网页版
        "running": web.running,
        "port": web_port,
        "url": web_url,
        "pid": web.pid,

        "upstream_ok": upstream_port.is_some(),
        "upstream_port": upstream_port.unwrap_or(0),
        "antigravity": {
            "installed": ag_installed,
            "running": ag_running,
            // 调试端口是否可用（桌面版注入的前提）
            "debug_port": devtools_ok,
            // 有开着的注入功能因为没调试端口而做不了 → 菜单显示「重新打开」按钮
            "need_relaunch": need_relaunch,
        },
        "lan_ip": lan_ip,
        "token": token,
        "ca_trusted": ca_trusted,
        "ca_fingerprint": ca_fingerprint,
        "firewall": { "enabled": fw_enabled, "blocked": fw_blocked },

        "web": {
            "enabled": cfg.web_enabled,
            "running": web.running,
            "starting": web.starting,
            "port": web_port,
            "preferred_port": config::DEFAULT_WEB_PORT,
            // 网关总是绑 0.0.0.0，在跑就意味着局域网可访问
            "lan_accessible": web.running,
            "url": web_url,
            "pid": web.pid,
            "last_error": web_error,
        },
        "ssh": {
            "enabled": cfg.ssh_enabled,
            "running": ssh_running,
            "port": 0,
            "preferred_port": 0,
            "pid": 0,
            "url": "",
            // 官方已开放 SSH：不再注入，菜单文案相应改变
            "official": ssh_official,
            "detail": inject_detail(cfg.ssh_enabled, ssh_running, ssh_ok_text),
        },
        "ssh_web": {
            "enabled": cfg.ssh_web_enabled,
            "running": sshw.running,
            "starting": sshw.starting,
            "port": ssh_web_port,
            "preferred_port": config::DEFAULT_SSH_WEB_PORT,
            "url": ssh_web_url,
            "pid": sshw.pid,
            "lan_accessible": sshw.running,
            "target": cfg.ssh_target,
            "last_error": ssh_web_error,
        },
        "zh": {
            "enabled": cfg.zh_enabled,
            "running": zh_running,
            "pid": 0,
            "detail": inject_detail(cfg.zh_enabled, zh_running, "已生效"),
        },
        // 网页版用量指示器：只是往首页塞一段脚本，在跑即生效，没有"待注入"状态。
        // detail 为空串，菜单只显示开关本身。
        "usage": {
            "enabled": cfg.usage_enabled,
            // 两端都算：网页版靠网关注入（web 在跑就有），桌面版靠 CDP 注入。
            // 只要有一端能看，就算在跑；两端都在才算完全生效（见 detail）。
            "running": cfg.usage_enabled && (web.running || usage_running),
            "pid": 0,
            "detail": if !cfg.usage_enabled {
                String::new()
            } else if !web.running && !usage_running {
                if ag_running && !devtools_ok {
                    "网页版没在运行；桌面版需要 Antigravity 开调试端口".to_string()
                } else {
                    "网页版没在运行，启动后生效".to_string()
                }
            } else {
                String::new()
            },
        },
        "both_on": cfg.web_enabled && cfg.ssh_enabled && cfg.ssh_web_enabled,
        "token_persisted": !cfg.token.is_empty(),
    });

    println!("{}", obj);
}

/// macOS 应用防火墙状态：(是否开启, 是否拦截了本内核的入站连接)。
///
/// 公司管控的 Mac 常常强制开防火墙。被拦截时网关在本机一切正常，
/// 只是手机连不上 —— 不单独提示的话用户根本查不出原因。
/// 另外内核是 ad-hoc 签名，每次升级签名都会变，之前的"允许"可能失效。
fn firewall_state() -> (bool, bool) {
    const FW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    let run = |args: &[&str]| -> String {
        let mut c = std::process::Command::new(FW);
        c.args(args);
        sys::output_timeout(c, Duration::from_secs(3))
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
            .unwrap_or_default()
    };
    let global = run(&["--getglobalstate"]);
    // "Firewall is enabled. (State = 1)" / "(State = 2)"（2 = 阻止所有传入连接）
    let enabled = global.contains("enabled") && !global.contains("disabled");
    if !enabled {
        return (false, false);
    }
    if run(&["--getblockall"]).contains("enabled") {
        return (true, true);
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let app = run(&["--getappblocked", &exe]);
    (true, app.contains("is blocked"))
}

// ============================================================
// 后台服务的启停
// ============================================================

/// 日志文件路径。
fn log_path(kind: Kind) -> PathBuf {
    match kind {
        Kind::Web => sys::logs_dir().join("AgyHub.log"),
        Kind::SshWeb => sys::logs_dir().join("AgyHub-sshweb.log"),
    }
}

/// 日志文件最后几行（启动失败时把原因带给菜单显示）。
fn log_tail(kind: Kind, lines: usize) -> String {
    let text = std::fs::read_to_string(log_path(kind)).unwrap_or_default();
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// 在后台拉起一个服务进程（同一个二进制的 `run` 模式）。
fn spawn_service(kind: Kind, extra: &[&str]) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().map_err(|e| format!("无法定位自身可执行文件: {}", e))?;
    let _ = std::fs::create_dir_all(sys::logs_dir());
    let lp = log_path(kind);
    sys::rotate_log(&lp, 5 * 1024 * 1024);

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run").args(extra);
    // 独立进程组：菜单栏 App 或终端收到的信号不会连带打到服务上
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::null());
    // 子进程不需要这个环境变量（口令从配置文件读，换口令才能即时生效）
    cmd.env_remove("AGEHUB_TOKEN");

    match std::fs::OpenOptions::new().create(true).append(true).open(&lp) {
        Ok(f) => {
            let f2 = f.try_clone().map_err(|e| format!("日志文件无法复制: {}", e))?;
            cmd.stdout(f).stderr(f2);
        }
        Err(_) => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }
    cmd.spawn().map_err(|e| format!("启动失败: {}", e))
}

/// 等服务就绪：返回 Ok(true)=已就绪，Ok(false)=还在启动，Err=进程已退出。
fn wait_ready(kind: Kind, child: &mut std::process::Child, timeout: Duration) -> Result<bool, String> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let mut msg = state::last_error(kind);
            if msg.is_empty() {
                msg = log_tail(kind, 4);
            }
            if msg.is_empty() {
                msg = format!("服务进程退出（{}）", status);
            }
            return Err(msg);
        }
        let l = state::live(kind, 0);
        if l.running {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 后台启动本机网页版。幂等：已经在跑就直接返回。
fn start_web() -> serde_json::Value {
    let mut cfg = config::Config::load();
    if cfg.normalize() {
        let _ = cfg.save();
    }
    let l = state::live(Kind::Web, cfg.web_port);
    let mut replaced = 0;
    if l.pid > 0 {
        if l.current && (l.running || l.starting) {
            return json!({ "ok": true, "reused": true, "pid": l.pid, "port": l.port, "running": l.running });
        }
        // 两种情况要换掉：
        //   · 老版本内核起的进程（App 升级 / Sparkle 更新后）：
        //     它跑的还是旧代码，不换掉的话用户升级了等于没升级
        //   · 进程还在但端口已经不通（卡死）：不换掉的话永远"复用"一个坏进程
        tracing::info!("替换网页版进程 pid={}（{}）", l.pid, if l.current { "无响应" } else { "旧版本" });
        state::stop(Kind::Web, cfg.web_port);
        replaced = l.pid;
    }
    cfg.ensure_token();
    state::clear_error(Kind::Web);

    let mut child = match spawn_service(Kind::Web, &[]) {
        Ok(c) => c,
        Err(e) => {
            state::set_error(Kind::Web, &e);
            return json!({ "ok": false, "error": e });
        }
    };
    match wait_ready(Kind::Web, &mut child, Duration::from_secs(8)) {
        Ok(ready) => {
            let l = state::live(Kind::Web, 0);
            json!({
                "ok": true, "pid": child.id(), "port": l.port,
                "running": ready, "starting": !ready, "replaced": replaced,
            })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// 后台启动 SSH 网页版。
///
/// 建隧道可能要十几秒（ssh 握手 + 扫远端端口），这里默认只等 2 秒：
/// 进程起来了就返回 `starting`，后续进度由 status 的 starting / last_error 反映，
/// 菜单不必一直卡着等。`--wait N` 可以让命令行用户等到结果。
fn start_ssh_web(args: &[String]) -> serde_json::Value {
    let mut cfg = config::Config::load();
    cfg.normalize();

    let target = read_flag(args, "--target")
        .unwrap_or_else(|| cfg.ssh_target.clone())
        .trim()
        .to_string();
    if target.is_empty() {
        return json!({
            "ok": false,
            "error": "没有指定开发机。请先在菜单里选一台（~/.ssh/config 里的别名）。"
        });
    }
    // 主机名不能以 - 开头：否则会被 ssh 当成命令行选项（参数注入）
    if target.starts_with('-') || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return json!({ "ok": false, "error": format!("主机名不合法: {}", target) });
    }

    let l = state::live(Kind::SshWeb, cfg.ssh_web_port);
    if l.pid > 0 {
        if l.current && l.target == target && (l.running || l.starting) {
            return json!({
                "ok": true, "reused": true, "pid": l.pid, "port": l.port,
                "target": target, "running": l.running, "starting": l.starting,
            });
        }
        // 换了开发机 / 老版本内核起的进程：先停掉旧的
        state::stop(Kind::SshWeb, cfg.ssh_web_port);
    }

    cfg.ensure_token();
    cfg.ssh_target = target.clone();
    let _ = cfg.save();
    state::clear_error(Kind::SshWeb);

    let wait = read_flag(args, "--wait")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2);

    let mut child = match spawn_service(Kind::SshWeb, &["--ssh-target", &target]) {
        Ok(c) => c,
        Err(e) => {
            state::set_error(Kind::SshWeb, &e);
            return json!({ "ok": false, "error": e });
        }
    };
    match wait_ready(Kind::SshWeb, &mut child, Duration::from_secs(wait)) {
        Ok(ready) => json!({
            "ok": true, "pid": child.id(), "port": cfg.ssh_web_port,
            "target": target, "running": ready, "starting": !ready,
        }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// 停止服务。只停 pid 文件里登记的（或确认是 agyhub 的）进程。
fn stop_cmd(kinds: &[Kind]) -> serde_json::Value {
    let cfg = config::Config::load();
    let mut stopped = Vec::new();
    for &k in kinds {
        if let Some(pid) = state::stop(k, cfg.port_of(k)) {
            stopped.push(json!({ "service": k.name(), "pid": pid }));
        }
    }
    json!({ "ok": true, "stopped": stopped })
}

/// 换口令：生成新的、落盘。运行中的网关 1 秒内自动改用新口令。
fn rotate_token() {
    let mut cfg = config::Config::load();
    let t = cfg.rotate_token();
    println!("{}", json!({ "ok": true, "token": t }));
}

/// 设置某个服务的开关。用法：agyhub set-enabled web|ssh|ssh_web 0|1
fn set_enabled(args: &[String]) {
    let mut cfg = config::Config::load();
    let mut target = String::new();
    let mut value: Option<bool> = None;
    for (i, a) in args.iter().enumerate() {
        if a == "web" || a == "ssh" || a == "ssh_web" {
            target = a.clone();
            if let Some(v) = args.get(i + 1) {
                value = Some(v == "1" || v == "true" || v == "on");
            }
        }
    }
    match (target.as_str(), value) {
        ("web", Some(v)) => cfg.web_enabled = v,
        ("ssh", Some(v)) => cfg.ssh_enabled = v,
        ("ssh_web", Some(v)) => cfg.ssh_web_enabled = v,
        _ => {
            println!("{}", json!({"ok": false, "error": "用法: set-enabled web|ssh|ssh_web 0|1"}));
            std::process::exit(2);
        }
    }
    let _ = cfg.save();
    println!(
        "{}",
        json!({
            "ok": true,
            "web_enabled": cfg.web_enabled,
            "ssh_enabled": cfg.ssh_enabled,
            "ssh_web_enabled": cfg.ssh_web_enabled,
        })
    );
}

/// 智能选端口并写回配置。输出 JSON。
///
/// 优先用**配置里记录的端口**（用户可能改过），而不是固定的 8898：
/// 原来这里每次都从 8898 开始找，用户设置的自定义端口只要服务没在跑，
/// 下次启动就被悄悄改回 8898。
fn pick_port_cmd(args: &[String]) {
    let mut cfg = config::Config::load();
    cfg.normalize();
    let which = args
        .iter()
        .skip(1)
        .find(|a| *a == "web" || *a == "ssh_web")
        .cloned()
        .unwrap_or_else(|| "web".to_string());
    let kind = if which == "ssh_web" { Kind::SshWeb } else { Kind::Web };
    let preferred = match kind {
        Kind::Web => config::DEFAULT_WEB_PORT,
        Kind::SshWeb => config::DEFAULT_SSH_WEB_PORT,
    };
    let current = cfg.port_of(kind);
    let other = match kind {
        Kind::Web => cfg.ssh_web_port,
        Kind::SshWeb => cfg.web_port,
    };

    let l = state::live(kind, current);
    let chosen = if l.pid > 0 {
        l.port
    } else if current != other && config::port_free(current) {
        current
    } else {
        config::pick_port(current, other)
    };

    match kind {
        Kind::Web => cfg.web_port = chosen,
        Kind::SshWeb => cfg.ssh_web_port = chosen,
    }
    let _ = cfg.save();

    println!(
        "{}",
        json!({
            "ok": true,
            "service": which,
            "port": chosen,
            "preferred": preferred,
            "changed": chosen != preferred,
        })
    );
}

/// 列出 ~/.ssh/config 里可用的主机别名（支持 Include）。
fn list_ssh_hosts() {
    let ssh_dir = sys::home().join(".ssh");
    let mut hosts: Vec<String> = Vec::new();
    collect_ssh_hosts(&ssh_dir.join("config"), &ssh_dir, &mut hosts, 0);
    println!("{}", json!({ "ok": true, "hosts": hosts }));
}

fn collect_ssh_hosts(path: &std::path::Path, ssh_dir: &std::path::Path, hosts: &mut Vec<String>, depth: u32) {
    if depth > 4 {
        return;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        // 关键字与值之间可以是空格、制表符或 =
        let (key, rest) = match t.find(|c: char| c.is_whitespace() || c == '=') {
            Some(i) => (&t[..i], t[i + 1..].trim_start_matches(|c: char| c.is_whitespace() || c == '=')),
            None => continue,
        };
        let key = key.to_ascii_lowercase();
        if key == "include" {
            // 很多公司的 ssh 配置是 `Include config.d/*` 拆开写的，
            // 不跟进去就会误报"没有可用的 SSH 主机"。
            for pat in rest.split_whitespace() {
                for f in expand_include(pat, ssh_dir) {
                    collect_ssh_hosts(&f, ssh_dir, hosts, depth + 1);
                }
            }
            continue;
        }
        if key != "host" {
            continue;
        }
        for h in rest.split_whitespace() {
            // 跳过通配符和取反
            if h.contains('*') || h.contains('?') || h.starts_with('!') {
                continue;
            }
            if !hosts.iter().any(|x| x == h) {
                hosts.push(h.to_string());
            }
        }
    }
}

/// 展开 Include 路径：支持 `~/`、相对 ~/.ssh 的路径、文件名里的 `*`。
fn expand_include(pat: &str, ssh_dir: &std::path::Path) -> Vec<PathBuf> {
    let p = if let Some(rest) = pat.strip_prefix("~/") {
        sys::home().join(rest)
    } else if pat.starts_with('/') {
        PathBuf::from(pat)
    } else {
        ssh_dir.join(pat)
    };
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if !name.contains('*') {
        return vec![p];
    }
    let dir = match p.parent() {
        Some(d) => d.to_path_buf(),
        None => return vec![],
    };
    let (pre, post) = name.split_once('*').unwrap_or((&name, ""));
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|f| {
                    let n = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    f.is_file() && n.starts_with(pre) && n.ends_with(post) && n.len() >= pre.len() + post.len()
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 注入操作的跨进程锁。
///
/// App 的定时巡检（ensure-inject）和用户点开关（zh / ssh-unlock）是两个独立进程，
/// 同时连 CDP 时会各自判断"这个窗口还没注入"，结果同一个窗口被刷新两次。
/// 持有返回的文件句柄期间加锁，句柄释放时自动解锁。
fn inject_lock() -> Option<std::fs::File> {
    let _ = std::fs::create_dir_all(sys::run_dir());
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(sys::run_dir().join("inject.lock"))
        .ok()?;
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(f.as_raw_fd(), libc::LOCK_EX);
    }
    Some(f)
}

fn current_thread_rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建运行时失败: {}", e))
}

/// 桌面版 SSH 入口是否还需要我们注入钩子。
///
/// 2.33.0 起官方正式开放了 SSH（服务端开关不再带 disabled），
/// 这时再注入只会白白刷新一次窗口。
///
/// ⚠️ 只有**明确查到开关还关着**才注入。查不到（None）时这一轮先不注入：
/// 实测 Antigravity 刚重开的几秒里 language_server 还没起来，查不到开关，
/// 原来按"查不到就注入"处理，结果在官方已开放的版本上也注入并多刷新了一次窗口。
/// 老版本（开关带 disabled）同一个接口能查到 Some(false)，照样会注入；
/// 接口要是真没了，钩子拦的就是这个接口，注入也没用。
fn ssh_hook_needed(cfg: &config::Config) -> bool {
    cfg.ssh_enabled && upstream::ssh_official(upstream::find_upstream(false)) == Some(false)
}

/// 巡检桌面版注入（App 每隔几秒调用一次）。
///
/// `--force-ssh-reload`：允许为装 SSH 钩子刷新页面（用户刚打开开关时）。
fn ensure_inject_cmd(args: &[String]) {
    // 先拿锁再读配置：等锁期间用户可能刚关掉了中文
    let _lock = inject_lock();
    let cfg = config::Config::load();
    let want_ssh = ssh_hook_needed(&cfg);
    // 桌面版用量指示器：和网页版共用同一个开关（usage_enabled）。
    // 用户脑子里的「我要看用量」是一个概念，不该因为端不同而分成两个开关。
    let want_usage = cfg.usage_enabled;
    if !cfg.zh_enabled && !want_ssh && !want_usage {
        println!("{}", json!({ "ok": true, "skipped": true }));
        return;
    }
    let force = args.iter().any(|a| a == "--force-ssh-reload");
    let rt = match current_thread_rt() {
        Ok(r) => r,
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e }));
            return;
        }
    };
    let res = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(40),
            actions::ensure_injected(cfg.zh_enabled, want_ssh, want_usage, force),
        )
        .await
        .unwrap_or_else(|_| Err("巡检超时".into()))
    });
    match res {
        Ok(r) => println!(
            "{}",
            json!({
                "ok": r.errors.is_empty(),
                "pages": r.pages,
                "zh_ok": r.zh_ok,
                "ssh_ok": r.ssh_ok,
                "usage_ok": r.usage_ok,
                "actions": r.actions,
                "error": r.errors.join("; "),
            })
        ),
        Err(e) => println!("{}", json!({ "ok": false, "error": e })),
    }
}

/// 桌面版 SSH 入口：`ssh-unlock` 打开，`ssh-unlock off` 关闭。
fn cmd_ssh_unlock(args: &[String]) {
    let off = args.iter().skip(1).any(|a| a == "off");
    let _lock = inject_lock();
    let mut cfg = config::Config::load();
    cfg.ssh_enabled = !off;
    let _ = cfg.save();

    let rt = match current_thread_rt() {
        Ok(r) => r,
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e }));
            return;
        }
    };

    if off {
        match rt.block_on(actions::ssh_unlock_off()) {
            Ok(msg) => println!("{}", json!({ "ok": true, "enabled": false, "message": msg })),
            // Antigravity 没开也算关闭成功：下次打开时不会再注入
            Err(e) => println!("{}", json!({ "ok": true, "enabled": false, "message": e })),
        }
        return;
    }

    // 没查到"开关还关着"：开关只记在配置里，不注入、不刷新窗口。
    // 官方已开放时是真生效；暂时查不到（Antigravity 刚启动）时巡检会在查到 Some(false) 后补上。
    if !ssh_hook_needed(&cfg) {
        let official = upstream::ssh_official(upstream::find_upstream(false)) == Some(true);
        println!(
            "{}",
            json!({
                "ok": true, "enabled": true, "running": official, "official": official,
                "message": if official { "官方已开放 SSH 入口，不需要注入" } else { "已记下，Antigravity 就绪后自动生效" },
            })
        );
        return;
    }

    let res = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(40),
            actions::ensure_injected(cfg.zh_enabled, true, cfg.usage_enabled, true),
        )
        .await
        .unwrap_or_else(|_| Err("注入超时".into()))
    });
    match res {
        Ok(r) if r.ssh_ok > 0 => println!(
            "{}",
            json!({
                "ok": true, "enabled": true, "running": true,
                "message": format!("已解锁 {} 个页面", r.ssh_ok),
            })
        ),
        Ok(r) => println!(
            "{}",
            json!({ "ok": false, "enabled": true, "error": r.errors.join("; ") })
        ),
        Err(e) => println!(
            "{}",
            json!({
                "ok": false, "enabled": true,
                "error": format!("{}（Antigravity 打开后会自动生效）", e),
            })
        ),
    }
}

/// 启停桌面版中文界面。
///
/// 持久化靠 App 的巡检（ensure-inject）：Antigravity 重启 / 刷新后
/// 几秒内自动重新注入。
fn toggle_zh(args: &[String]) {
    let _lock = inject_lock();
    let mut cfg = config::Config::load();
    let action = args
        .iter()
        .skip(1)
        .find(|a| *a == "on" || *a == "off" || *a == "status")
        .cloned()
        .unwrap_or_else(|| "status".to_string());

    match action.as_str() {
        "status" => {
            let inj = state::inject_load();
            let fresh = state::inject_fresh(&inj, upstream::devtools_port());
            println!(
                "{}",
                json!({ "ok": true, "enabled": cfg.zh_enabled, "running": cfg.zh_enabled && fresh && inj.zh, "pid": 0 })
            );
        }
        "off" => {
            // ⚠️ 先落盘关闭，再还原界面。
            // 顺序反了的话，App 的巡检可能恰好在还原之后、落盘之前跑一次，
            // 看到"中文开着但页面没中文"，又给注入回去。
            cfg.zh_enabled = false;
            let _ = cfg.save();

            let mut restored = false;
            if let Ok(rt) = current_thread_rt() {
                if let Ok(msg) = rt.block_on(actions::restore_zh_ui(ssh_hook_needed(&cfg))) {
                    tracing::info!("还原界面: {}", msg);
                    restored = true;
                }
            }
            println!(
                "{}",
                json!({
                    "ok": true,
                    "enabled": false,
                    "running": false,
                    // 界面是否已还原成英文。false 时可以提示用户手动刷新。
                    "ui_restored": restored,
                })
            );
        }
        _ => {
            cfg.zh_enabled = true;
            let _ = cfg.save();
            let rt = match current_thread_rt() {
                Ok(r) => r,
                Err(e) => {
                    println!("{}", json!({"ok": false, "error": e}));
                    return;
                }
            };
            let res = rt.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(40),
                    actions::ensure_injected(true, ssh_hook_needed(&cfg), cfg.usage_enabled, false),
                )
                .await
                .unwrap_or_else(|_| Err("注入超时".into()))
            });
            match res {
                Ok(r) if r.zh_ok > 0 => println!(
                    "{}",
                    json!({
                        "ok": true, "enabled": true, "running": true,
                        "message": format!("已对 {} 个页面注入中文", r.zh_ok),
                    })
                ),
                // 注入失败但配置已记下：Antigravity 打开后巡检会自动补上。
                // 所以对用户来说这不是"失败"，ok 仍为 true，只是还没生效。
                Ok(r) => println!(
                    "{}",
                    json!({ "ok": true, "enabled": true, "running": false, "message": r.errors.join("; ") })
                ),
                Err(e) => println!(
                    "{}",
                    json!({
                        "ok": true, "enabled": true, "running": false,
                        "message": format!("{}（Antigravity 打开后会自动生效）", e),
                    })
                ),
            }
        }
    }
}

/// 网页版「上下文与用量」指示器开关。用法：agyhub usage on|off|status
///
/// 和中文不同，这个开关**没有巡检、也不需要连 CDP**：
/// 它只是在网关转发的首页里塞/不塞一段脚本，所以开关本身就是"立即生效" ——
/// 用户刷新页面就能看到变化。因此这里不需要等 Antigravity、不需要超时重试。
fn toggle_usage(args: &[String]) {
    let mut cfg = config::Config::load();
    let action = args
        .iter()
        .skip(1)
        .find(|a| *a == "on" || *a == "off" || *a == "status")
        .cloned()
        .unwrap_or_else(|| "status".to_string());

    match action.as_str() {
        "status" => {
            println!(
                "{}",
                json!({ "ok": true, "enabled": cfg.usage_enabled })
            );
        }
        "off" => {
            cfg.usage_enabled = false;
            let _ = cfg.save();
            println!(
                "{}",
                json!({
                    "ok": true, "enabled": false,
                    "message": "已关闭，刷新网页后消失",
                })
            );
        }
        _ => {
            cfg.usage_enabled = true;
            let _ = cfg.save();
            println!(
                "{}",
                json!({
                    "ok": true, "enabled": true,
                    "message": "已开启，刷新网页后出现在发送按钮旁",
                })
            );
        }
    }
}

/// 设置指定端口（用户自定义）。用法：agyhub set-port web|ssh_web 9000
///
/// 与 pick-port 的区别：用户指定的端口必须原样生效，被占用要**明确报错**
/// 并给出建议，而不是偷偷换一个。
fn set_port_cmd(args: &[String]) {
    let mut cfg = config::Config::load();
    cfg.normalize();

    let mut which = String::new();
    let mut num: Option<u16> = None;
    for (i, a) in args.iter().enumerate() {
        if a == "web" || a == "ssh_web" {
            which = a.clone();
            if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u16>().ok()) {
                num = Some(v);
            }
        }
    }

    let (kind, port) = match (which.as_str(), num) {
        ("web", Some(p)) => (Kind::Web, p),
        ("ssh_web", Some(p)) => (Kind::SshWeb, p),
        _ => {
            println!("{}", json!({ "ok": false, "error": "用法: set-port web|ssh_web <端口号>" }));
            std::process::exit(2);
        }
    };

    if port < 1024 {
        println!("{}", json!({
            "ok": false,
            "error": format!("端口 {} 小于 1024，需要管理员权限才能监听，请改用 1024 以上", port),
            "port": port,
        }));
        std::process::exit(1);
    }

    let (other_kind, other) = match kind {
        Kind::Web => (Kind::SshWeb, cfg.ssh_web_port),
        Kind::SshWeb => (Kind::Web, cfg.web_port),
    };
    // 另一个服务配置里的端口，或者它此刻实际在用的端口（临时换过端口时两者不同）
    let other_live = state::live(other_kind, 0);
    if port == other || (other_live.pid > 0 && other_live.port == port) {
        println!("{}", json!({
            "ok": false,
            "error": format!("端口 {} 已被另一个服务使用，请换一个", port),
            "port": port,
            "conflict_with": other,
        }));
        std::process::exit(1);
    }

    // 被系统里其他程序占用：说清楚"谁占了"，并给一个建议端口。
    // 只有"本服务自己正占着这个端口"才算可用（重复设置同一个端口）。
    let own = state::live(kind, cfg.port_of(kind));
    let held_by_self = own.pid > 0 && own.port == port;
    if !held_by_self && !config::port_free(port) {
        let holder = state::port_holder_name(port);
        let suggestion = config::pick_port(port, other);
        println!("{}", json!({
            "ok": false,
            "error": format!("端口 {} 已被占用{}，建议改用 {}", port,
                if holder.is_empty() { String::new() } else { format!("（{}）", holder) },
                suggestion),
            "port": port,
            "held_by": holder,
            "suggestion": suggestion,
        }));
        std::process::exit(1);
    }

    match kind {
        Kind::Web => cfg.web_port = port,
        Kind::SshWeb => cfg.ssh_web_port = port,
    }
    let _ = cfg.save();

    println!("{}", json!({
        "ok": true,
        "service": kind.name(),
        "port": port,
        // 服务在跑的话需要重启才会换到新端口
        "restart_needed": own.pid > 0 && own.port != port,
    }));
}

/// 环境自检：把常见的启动失败原因提前暴露出来。
fn doctor() {
    println!("agyhub {} 环境检查\n", VERSION);
    let mut cfg = config::Config::load();
    cfg.normalize();

    // 1. Antigravity
    let installed = inject::antigravity_installed();
    let running = inject::antigravity_running();
    println!(
        "  {} Antigravity 已安装",
        if installed { "✓" } else { "✗" }
    );
    println!(
        "  {} Antigravity 正在运行",
        if running { "✓" } else { "✗" }
    );
    match upstream::find_upstream(true) {
        Some(p) => println!("  ✓ 上游 language_server: 127.0.0.1:{}", p),
        None => println!("  ✗ 找不到 Antigravity 上游（没打开 / 没登录 / 还在启动）"),
    }
    match upstream::devtools_port() {
        Some(p) => println!("  ✓ 调试端口: {}", p),
        None => println!("  - 读不到调试端口（桌面版注入功能需要 Antigravity 在运行）"),
    }

    // 2. 外部命令可用性
    let mut lsof = std::process::Command::new("/usr/sbin/lsof");
    lsof.args(["-nP", "-iTCP", "-sTCP:LISTEN"]);
    let t0 = std::time::Instant::now();
    match sys::output_timeout(lsof, Duration::from_secs(5)) {
        Some(o) if o.status.success() || !o.stdout.is_empty() => {
            println!("  ✓ lsof 可用（{} ms）", t0.elapsed().as_millis())
        }
        Some(_) => println!("  ! lsof 运行失败（会改用备用方式找上游）"),
        None => println!("  ! lsof 超时（会改用备用方式找上游）"),
    }

    // 3. 本机地址
    println!("  本机地址:");
    for ip in cert::local_ips() {
        println!("      {}", ip);
    }

    // 4. 证书
    let dir = data_dir().join("certs");
    match cert::ensure_certs(&dir) {
        Ok(b) => {
            println!("  ✓ 证书就绪（{}）", dir.display());
            println!("      根证书指纹 {}", cert::ca_fingerprint(&b.ca_pem));
            match cert::tls_acceptor(&b) {
                Ok(_) => println!("  ✓ TLS 配置可用"),
                Err(e) => println!("  ✗ TLS 配置失败: {}", e),
            }
        }
        Err(e) => println!("  ✗ 证书生成失败: {}", e),
    }
    if trust::is_trusted(&dir) {
        println!("  ✓ 本机浏览器信任该证书");
    } else {
        println!("  ✗ 本机浏览器不信任该证书 → 运行 agyhub trust-cert");
    }

    // 5. 服务
    for k in [Kind::Web, Kind::SshWeb] {
        let l = state::live(k, cfg.port_of(k));
        let name = match k {
            Kind::Web => "本机网页版",
            Kind::SshWeb => "SSH 网页版",
        };
        if l.running {
            println!("  ✓ {} 运行中（pid {}，端口 {}）", name, l.pid, l.port);
        } else if l.starting {
            println!("  … {} 正在启动（pid {}）", name, l.pid);
        } else {
            let port = cfg.port_of(k);
            let free = config::port_free(port);
            println!(
                "  - {} 未运行；端口 {} {}",
                name,
                port,
                if free { "可用".to_string() } else { format!("被占用（{}）", state::port_holder_name(port)) }
            );
            let err = state::last_error(k);
            if !err.is_empty() {
                println!("      上次失败：{}", err.lines().next().unwrap_or(""));
            }
        }
    }
    println!("\n  配置文件：{}", data_dir().join("config.json").display());
    println!("  日志：~/Library/Logs/AgyHub*.log");
}

// ============================================================
// 服务主循环
// ============================================================

async fn run(port_arg: Option<u16>, ssh_target: Option<String>) {
    let kind = if ssh_target.is_some() { Kind::SshWeb } else { Kind::Web };
    let me = std::process::id();
    let dir = data_dir();

    // ---- 同类服务已在运行：不重复启动 ----
    //
    // 原来没有这一层，菜单栏 App 只要误判一次"没在跑"，就会再拉一个进程；
    // 端口被占时还会自动换到 8899、8900……越堆越多。
    let existing = state::live(kind, 0);
    if existing.pid > 0 && existing.pid != me {
        eprintln!(
            "{} 已在运行（pid {}，端口 {}），不重复启动",
            kind.name(),
            existing.pid,
            existing.port
        );
        std::process::exit(0);
    }

    // ---- 端口 ----
    //
    // ⚠️ 必须按服务种类读写各自的端口字段。
    // 原来不管什么模式都把实际端口写回 `web_port`：一开 SSH 网页版，
    // 配置里的网页版端口就被改成 8897，菜单里两个服务的链接互相串了。
    let mut cfg = config::Config::load();
    cfg.normalize();
    let preferred = port_arg.unwrap_or_else(|| cfg.port_of(kind));
    let other = match kind {
        Kind::Web => cfg.ssh_web_port,
        Kind::SshWeb => cfg.web_port,
    };
    let port = if preferred != other && config::port_free(preferred) {
        preferred
    } else {
        let picked = config::pick_port(preferred, other);
        if picked != preferred {
            tracing::warn!("端口 {} 已被占用，本次改用 {}", preferred, picked);
        }
        picked
    };
    // ⚠️ 临时换的端口**不写回配置**，只记在 pid 文件里（status 从那里读实际端口）。
    //
    // 写回的话，一次短暂的端口冲突就会让端口永久从 8898 变成 8899，
    // 手机上收藏的链接、装好的 PWA 从此都打不开。
    // 不写回：冲突消失后下次启动自动回到用户的端口。
    // 只有 --port 显式指定时才当作用户意图保存。
    if port_arg.is_some() && port == preferred && cfg.port_of(kind) != port {
        match kind {
            Kind::Web => cfg.web_port = port,
            Kind::SshWeb => cfg.ssh_web_port = port,
        }
        let _ = cfg.save();
    }

    let token = read_token();

    // ---- 登记 + 信号处理 ----
    state::register(
        kind,
        &state::RunInfo {
            pid: me,
            port,
            ready: false,
            target: ssh_target.clone().unwrap_or_default(),
            started: sys::now_secs(),
            exe_id: state::current_exe_id(),
            start_time: sys::pid_start_time(me).unwrap_or(0),
        },
    );
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let (mut term, mut int, mut hup) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return,
        };
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
            _ = hup.recv() => {},
        }
        // SIGTERM 下 Rust 不跑析构函数：ssh 子进程和 pid 文件都要手动收拾
        tunnel::cleanup();
        state::unregister(kind, me);
        tracing::info!("收到退出信号，已停止");
        std::process::exit(0);
    });

    tracing::info!("数据目录: {}", dir.display());

    let cert_dir = dir.join("certs");
    let bundle = match cert::ensure_certs(&cert_dir) {
        Ok(b) => b,
        Err(e) => run_fail(kind, me, format!("证书生成失败: {}", e)),
    };
    let (tls, dyn_cert) = match cert::tls_acceptor_dynamic(&bundle) {
        Ok(t) => t,
        Err(e) => run_fail(kind, me, format!("TLS 配置失败: {}", e)),
    };

    // ---- SSH 模式：先建隧道 ----
    if let Some(target) = ssh_target.clone() {
        tracing::info!("远端模式：{}", target);
        let started = tokio::task::spawn_blocking(move || {
            let mut tun = tunnel::RemoteTunnel::new(&target, 0)
                .with_logger(|m| tracing::info!("{}", m));
            tun.start().map(|_| tun)
        })
        .await;
        let mut t = match started {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => run_fail(kind, me, format!("隧道建立失败: {}", e)),
            Err(e) => run_fail(kind, me, format!("隧道线程异常: {}", e)),
        };
        tracing::info!("上游（远端）: 127.0.0.1:{}", t.local_port);
        upstream::set_remote_csrf(&t.csrf_token);
        upstream::set_remote(t.local_port);

        // 隧道守护：端到端探测连续失败 3 次（约 30 秒）才重连。
        // 放宽是因为 AI 推理时远端可能很忙，单次探测超时不代表断线。
        // 重连失败后退避（30s → 60s → … → 5min），避免开发机长时间不可达时
        // 每 10 秒发起一次完整的 ssh 连接。
        std::thread::spawn(move || {
            let mut fails = 0u32;
            let mut reconnect_fails = 0u32;
            let mut next_try = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(10));
                if t.alive() {
                    if fails >= 3 || reconnect_fails > 0 {
                        state::clear_error(Kind::SshWeb);
                    }
                    fails = 0;
                    reconnect_fails = 0;
                    continue;
                }
                fails += 1;
                if fails < 3 || std::time::Instant::now() < next_try {
                    continue;
                }
                tracing::warn!("隧道不可用（连续 {} 次探测失败），尝试重连", fails);
                match t.start() {
                    Ok(()) => {
                        upstream::set_remote_csrf(&t.csrf_token);
                        upstream::set_remote(t.local_port);
                        state::clear_error(Kind::SshWeb);
                        tracing::info!("隧道已重连 → 127.0.0.1:{}", t.local_port);
                        fails = 0;
                        reconnect_fails = 0;
                    }
                    Err(e) => {
                        reconnect_fails += 1;
                        let wait = (30u64 << (reconnect_fails - 1).min(4)).min(300);
                        next_try = std::time::Instant::now() + Duration::from_secs(wait);
                        tracing::warn!("重连失败（{} 秒后再试）: {}", wait, e);
                        state::set_error(
                            Kind::SshWeb,
                            &format!("开发机连接中断，{} 秒后自动重试：{}", wait, e),
                        );
                    }
                }
            }
        });
    } else {
        // 不做硬失败：用户可能先开网关再开 Antigravity，运行时会自动重试。
        match tokio::task::spawn_blocking(|| upstream::find_upstream(true)).await.ok().flatten() {
            Some(p) => tracing::info!("上游 Antigravity: 127.0.0.1:{}", p),
            None => tracing::warn!("暂时找不到 Antigravity，请确认应用已打开（运行中会持续重试）"),
        }
    }

    let listener = match server::bind(port).await {
        Ok(l) => l,
        Err(e) => run_fail(kind, me, format!("端口 {} 无法监听: {}", port, e)),
    };
    state::register(
        kind,
        &state::RunInfo {
            pid: me,
            port,
            ready: true,
            target: ssh_target.clone().unwrap_or_default(),
            started: sys::now_secs(),
            exe_id: state::current_exe_id(),
            start_time: sys::pid_start_time(me).unwrap_or(0),
        },
    );
    state::clear_error(kind);

    // ---- 换网络后自动重签证书 ----
    {
        let dyn_cert = dyn_cert.clone();
        let cert_dir = cert_dir.clone();
        tokio::spawn(async move {
            let mut last = sorted_ips();
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let now = sorted_ips();
                if now == last {
                    continue;
                }
                last = now;
                let d = cert_dir.clone();
                match tokio::task::spawn_blocking(move || cert::ensure_certs(&d)).await {
                    Ok(Ok(b)) => match dyn_cert.replace(&b) {
                        Ok(()) => tracing::info!("本机 IP 变化，已更新服务器证书"),
                        Err(e) => tracing::warn!("更新证书失败: {}", e),
                    },
                    Ok(Err(e)) => tracing::warn!("重签证书失败: {}", e),
                    Err(_) => {}
                }
            }
        });
    }

    let ctx = Arc::new(server::Ctx {
        token: server::TokenSource::new(std::env::var("AGEHUB_TOKEN").ok().filter(|t| !t.is_empty()), token.clone()),
        verbose: std::env::var("RUST_LOG")
            .map(|v| v.contains("debug"))
            .unwrap_or(false),
        ca_pem: bundle.ca_pem.clone(),
        port,
    });

    // 打印可访问地址。口令只在终端里显示 —— 后台运行时 stdout 是日志文件，
    // 用户把日志发给别人排查问题时不该顺带泄露口令。
    let show_token = sys::stdout_is_tty();
    println!();
    println!("  Antigravity Hub 已启动（{}）", kind.name());
    println!();
    for ip in cert::local_ips() {
        let host = if ip.is_loopback() { "localhost".to_string() } else { ip.to_string() };
        if show_token {
            println!("      {}", service_url(&host, port, &token));
        } else {
            println!("      https://{}:{}/", host, port);
        }
    }
    println!();

    if let Err(e) = server::serve(listener, tls, ctx).await {
        run_fail(kind, me, format!("服务异常退出: {}", e));
    }
}

/// 服务启动失败：记下原因（菜单里显示）、清理登记后退出。
fn run_fail(kind: Kind, me: u32, msg: String) -> ! {
    tracing::error!("{}", msg);
    eprintln!("✗ {}", msg);
    state::set_error(kind, &msg);
    tunnel::cleanup();
    state::unregister(kind, me);
    std::process::exit(1);
}

fn sorted_ips() -> Vec<std::net::IpAddr> {
    let mut v = cert::local_ips();
    v.sort();
    v
}

/// 口令。
///
/// 优先级：环境变量 AGEHUB_TOKEN > 配置文件 > 旧 token 文件 > 新生成并落盘。
fn read_token() -> String {
    if let Ok(t) = std::env::var("AGEHUB_TOKEN") {
        if !t.is_empty() {
            return t;
        }
    }
    let mut cfg = config::Config::load();
    if !cfg.token.is_empty() {
        return cfg.token.clone();
    }
    // 从旧文件迁移：读到就写入 config.json，之后只用一处真相
    let legacy = data_dir().join("token");
    if let Ok(t) = std::fs::read_to_string(&legacy) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            cfg.token = t.clone();
            let _ = cfg.save();
            return t;
        }
    }
    cfg.ensure_token()
}
