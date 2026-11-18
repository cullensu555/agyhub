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

mod actions;
mod cdp;
mod cert;
mod config;
mod inject;
mod proxy;
mod server;
mod trust;
mod tunnel;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;

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
    // 后果：Swift 每次轮询状态（每 3 秒一次）都会拉起一个服务进程，
    // 而且这些进程的输出混进 stdout，把 JSON 污染成
    // "Antigravity Hub 已启动…"，Swift 解码必然失败 →
    // 菜单永远显示 `v-`、点启动"没反应"。
    //
    // 正确做法：**先剥掉所有以 -- 开头的开关（连同它的值），
    // 剩下的第一个位置参数才是子命令。**
    let known_cmds = [
        "run", "status", "doctor", "trust-cert", "rotate-token",
        "set-enabled", "set-port", "pick-port",
        "ssh-hosts", "start-ssh-web", "zh", "ssh-unlock", "help",
        "version",
    ];
    let mut cmd = "run";
    {
        let mut i = 1; // 跳过 argv[0]
        while i < args.len() {
            let a = &args[i];
            // ⚠️ 顺序很重要：--version / --help 必须在下面那个
            // `starts_with("--")` 分支**之前**判断。
            // 否则它们会被当成普通开关直接跳过，
            // 结果是 `agyhub --version` 会启动整个服务（踩过）。
            if a == "--version" || a == "-V" {
                cmd = "version";
                break;
            }
            if a == "--help" || a == "-h" {
                cmd = "help";
                break;
            }
            if a.starts_with("--") {
                // --port 8898 这种带值的开关，要把值也跳过去
                if a == "--port" && i + 1 < args.len() {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if known_cmds.contains(&a.as_str()) {
                cmd = a;
                // ⚠️ 找到第一个已知子命令就停，不能继续往后匹配。
                //
                // 反例：`agyhub zh status`。
                // 如果不 break，后面的 `status` 也是 known_cmds，
                // 会把 cmd 从 "zh" 覆盖成 "status"，
                // 于是 `zh status` 变成了"查整体状态"。
                // 更糟的是 `zh on` 之类会走到默认分支直接起服务。
                break;
            }
            // 不是已知子命令：可能是子命令的参数（如 `set-port web 9000` 里的 web），
            // 继续往后找
            i += 1;
        }
    }

    // 日志一律写 stderr。
    //
    // 这不是洁癖：`status` 子命令要往 stdout 输出单行 JSON 给菜单栏 App 解析，
    // 日志混进去就解析失败。写 stderr 后两者互不干扰，
    // 也不会影响用户在终端里正常看日志。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // 取端口：--port 8898 或 AGEHUB_PORT
    let port = read_port(&args).unwrap_or(8898);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("无法创建 tokio 运行时");

    match cmd {
        "doctor" => doctor(port),
        // 供菜单栏 App 调用的状态查询。输出单行 JSON，不做美化 ——
        // Swift 那边直接 JSONDecoder，多余空白只是浪费解析。
        // 约定：只输出到 stdout，成功返回 0，失败返回非 0。
        "status" => status(port),
        "rotate-token" => rotate_token(),
        "set-enabled" => set_enabled(&args),
        "pick-port" => pick_port_cmd(&args),
        "set-port" => set_port_cmd(&args),
        "ssh-hosts" => list_ssh_hosts(),
        "zh" => toggle_zh(&args),
        "ssh-unlock" => cmd_ssh_unlock(),
        "start-ssh-web" => println!("{}", start_ssh_web(&args)),
        "trust-cert" => {
            let dir = data_dir().join("certs");
            // 先确保证书存在
            let _ = cert::ensure_certs(&dir);
            let force = args.iter().any(|a| a == "--force");
            println!();
            let ok = trust::install(&dir.join(cert::CA_CERT), force);
            println!();
            if ok {
                println!("  验证：security find-certificate -c \"Antigravity Local CA\" ~/Library/Keychains/login.keychain-db");
            }
            std::process::exit(if ok { 0 } else { 1 });
        }
        "--version" | "-V" | "version" => {
            println!("agyhub {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        "--help" | "-h" | "help" => usage(),
        _ => {
            // --ssh-target 把网关变成远端开发机的前端
            let ssh_target = read_ssh_target(&args);
            rt.block_on(run(port, ssh_target))
        }
    }
}

/// 从参数或环境变量取端口。
///
/// 注意只认 `--port N` / `--port=N` 两种写法。
/// 早期版本这里用「遍历所有参数找数字」的实现，会把 `agyhub status 8898`
/// 里的位置参数误当成端口 —— 所以现在严格按开关匹配。
/// 从参数里读 --ssh-target。
fn read_ssh_target(args: &[String]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--ssh-target" {
            if let Some(v) = it.next() {
                if !v.is_empty() && !v.starts_with("--") {
                    return Some(v.clone());
                }
            }
        } else if let Some(v) = a.strip_prefix("--ssh-target=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn read_port(args: &[String]) -> Option<u16> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            if let Some(v) = it.next() {
                if let Ok(p) = v.parse() {
                    return Some(p);
                }
            }
        } else if let Some(v) = a.strip_prefix("--port=") {
            if let Ok(p) = v.parse() {
                return Some(p);
            }
        }
    }
    // 子命令后面的第一个纯数字位置参数也认（`agyhub status 8898` 这种写法）
    let known = [
        "run", "status", "doctor", "trust-cert", "help", "--help", "-h",
    ];
    let mut rest = args.iter().skip(1).filter(|a| !known.contains(&a.as_str()));
    while let Some(a) = rest.next() {
        if a.starts_with("--") {
            continue;
        }
        if let Ok(p) = a.parse() {
            return Some(p);
        }
    }
    std::env::var("AGEHUB_PORT").ok().and_then(|v| v.parse().ok())
}

fn usage() {
    println!(
        "agyhub — Antigravity 网页访问网关\n\n\
         用法:\n  \
           agyhub                    启动网关（默认端口 8898）\n  \
           agyhub --port 9000        指定端口\n  \
           agyhub doctor             检查环境（上游、证书、端口）\n  \
           agyhub trust-cert         把根证书装进钥匙串（消除不安全警告）\n  \
           agyhub trust-cert --force 已装过也要重写信任设置\n  \
           agyhub help               显示本帮助\n\n\
         环境变量:\n  \
           AGEHUB_PORT               端口（--port 优先）\n  \
           AGEHUB_TOKEN              口令（不设则读 ~/.agyhub/token）\n  \
           RUST_LOG                  日志级别，如 debug\n"
    );
}

/// 供菜单栏 App 调用的状态查询，输出单行 JSON。
///
/// 为什么不把这些逻辑写进 Swift：
///   网卡枚举、上游探测（要扫 lsof + 探针）、证书指纹，这些都在 Rust 里
///   已经写好了。让 Swift 再做一遍就是两份实现，改一处漏一处的经典开局。
///   所以 Swift 只负责「显示」，事实由这里给。
fn status(preferred_port: u16) {
    let mut cfg = config::Config::load();
    // 校正端口一致性（两个服务不能同端口）。只改内存不落盘 ——
    // status 是只读操作，落盘留给真正需要修改的时机。
    cfg.normalize();
    // 状态查询不该修改磁盘，但口令为空时必须有值可显示。
    // 用内存里的临时口令，不落盘 —— status 必须是幂等的只读操作。
    let token = if cfg.token.is_empty() {
        config::gen_token()
    } else {
        cfg.token.clone()
    };

    let ips = cert::local_ips();
    let upstream_port = upstream::find_upstream(true);
    let cert_dir = data_dir().join("certs");

    let ca_path = cert_dir.join(cert::CA_CERT);
    let ca_fingerprint = std::process::Command::new("openssl")
        .args([
            "x509",
            "-in",
            ca_path.to_str().unwrap_or(""),
            "-noout",
            "-fingerprint",
            "-sha256",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_start_matches("sha256 Fingerprint=")
                .trim_start_matches("SHA256 Fingerprint=")
                .to_string()
        })
        .unwrap_or_default();

    let ca_trusted = trust::installed().is_some();

    let lan_ip = ips
        .iter()
        .find(|ip| !ip.is_loopback())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // ---- 网页版 ----
    //
    // 端口取值顺序：显式传入的 --port > 配置文件 > 默认 8898
    // 实际在用哪个端口，看配置里记录的（服务启动时写回的就是真实端口）。
    let web_port = if preferred_port != 8898 {
        preferred_port
    } else if cfg.web_port != 0 {
        cfg.web_port
    } else {
        8898
    };

    // ⚠️ 必须区分「端口被占」和「我们的服务在跑」。
    //
    // 早期用 `!port_free(port)` 判断，结果 Python 版 agygate 占着 8898 时，
    // 菜单显示"正在运行"，但其实我们一个进程都没起。
    // 用户点"启动服务"时因为 already running 的判断直接跳过，
    // 表现就是"点了没反应" —— 这是最初那个 bug 的另一半。
    let web_running = config::port_owned_by_us(web_port);
    // 8099 是 Antigravity 可能占用的端口；避免把它当成自己的服务
    let web_url = if token.is_empty() {
        format!("https://{}:{}/", lan_ip, web_port)
    } else {
        format!("https://{}:{}/?t={}", lan_ip, web_port, token)
    };

    // ---- SSH ----
    //
    // SSH 入口解锁是独立进程（node agy-ssh-unlock.cjs），
    // 它不监听端口，只通过 CDP 注入。所以"是否运行"要查进程而不是查端口。
    let ssh_pid = find_ssh_unlock_pid();
    let ssh_running = ssh_pid > 0;

    // ---- SSH 网页版 ----
    //
    // 它不是 Rust 内核起的，而是由 agygate.py --ssh-target 起的
    // （Python 版里 ssh 隧道 + 远端端口发现那套逻辑已经验证过，
    // 在 Rust 里重写一遍风险高、收益低）。
    // 所以这里只做两件事：查进程有没有在跑、拼出访问 URL。
    let ssh_web_port = cfg.ssh_web_port;
    let ssh_web_pid = find_ssh_web_pid();
    let ssh_web_running = ssh_web_pid > 0;
    let ssh_web_url = if token.is_empty() {
        format!("https://{}:{}/", lan_ip, ssh_web_port)
    } else {
        format!("https://{}:{}/?t={}", lan_ip, ssh_web_port, token)
    };

    let obj = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        // 兼容旧字段：顶层 running/port/url 指向网页版
        "running": web_running,
        "port": web_port,
        "url": web_url,
        "pid": running_pid(web_port).unwrap_or(0),

        "upstream_ok": upstream_port.is_some(),
        "upstream_port": upstream_port.unwrap_or(0),
        "lan_ip": lan_ip,
        "token": token,
        "ca_trusted": ca_trusted,
        "ca_fingerprint": ca_fingerprint,

        // ---- 双服务结构 ----
        "web": {
            "enabled": cfg.web_enabled,
            "running": web_running,
            "port": web_port,
            "preferred_port": 8898,
            "lan_accessible": web_running && listening_on_all_interfaces(web_port),
            "url": web_url,
            // pid 只在"确实是我们自己的进程"时才有意义。
            // 端口被别的程序占用时给 0 —— 否则 Swift 侧会拿到 Python 的 pid，
            // 点"停止服务"就把别人的进程杀了。
            "pid": if web_running { running_pid(web_port).unwrap_or(0) } else { 0 },
        },
        "ssh": {
            "enabled": cfg.ssh_enabled,
            "running": ssh_running,
            // SSH 解锁不监听端口，port 固定 0 —— 菜单据此不显示端口行
            "port": 0,
            "preferred_port": 0,
            "pid": ssh_pid,
            // SSH 入口没有独立 URL —— 它在 Antigravity 界面里的命令面板中
            "url": "",
        },
        "ssh_web": {
            "enabled": cfg.ssh_web_enabled,
            "running": ssh_web_running,
            "port": ssh_web_port,
            "preferred_port": 8897,
            "url": if ssh_web_running { ssh_web_url } else { String::new() },
            "pid": ssh_web_pid,
            // 是否绑在所有网卡上 —— 手机能不能连全看这个
            "lan_accessible": ssh_web_running && listening_on_all_interfaces(ssh_web_port),
            // 连的是哪台开发机，菜单上显示出来才知道转发到哪
            "target": cfg.ssh_target,
        },
        "zh": {
            "enabled": cfg.zh_enabled,
            "running": find_zh_pid() > 0,
            "pid": find_zh_pid(),
        },
        "both_on": cfg.web_enabled && cfg.ssh_enabled && cfg.ssh_web_enabled,
        "token_persisted": !cfg.token.is_empty(),
    });

    println!("{}", obj);
}

/// 找 SSH 解锁守护进程的 pid。没在跑返回 0。
fn find_ssh_unlock_pid() -> u32 {
    let out = match std::process::Command::new("pgrep")
        .args(["-f", "agy-ssh-unlock"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return 0,
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// 检查某端口是否在**所有网卡**上监听（而不只是回环）。
///
/// 为什么需要这个检查：
///   服务起在 127.0.0.1 和 0.0.0.0 看起来都"端口在监听"，
///   但前者手机根本连不上（ERR_CONNECTION_REFUSED）。
///   早期只用 `curl localhost` 验证，localhost 走回环当然通，
///   于是"我以为好了"，用户拿手机一试就废。
///
/// 判据：lsof 输出的地址列如果是 `127.0.0.1:port` → 仅本机；
/// 如果是 `*:port` 或 `0.0.0.0:port` → 全网卡，手机可访问。
fn listening_on_all_interfaces(port: u16) -> bool {
    let out = match std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{}", port), "-sTCP:LISTEN"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().skip(1) {
        // NAME 列是最后一列，形如 `*:8897` / `127.0.0.1:8897`
        if let Some(name) = line.split_whitespace().last() {
            if name.starts_with("*:") || name.starts_with("0.0.0.0:") || name.starts_with("[::]:") {
                return true;
            }
        }
    }
    false
}

/// 找 SSH 网页版（agygate.py --ssh-target）的进程。
///
/// 判断依据：命令行里同时有 agygate.py 和 --ssh-target。
/// 只匹配 agygate.py 会把用户其他用途的网关也当成我们的。
fn find_ssh_web_pid() -> u32 {
    // ---- 判定"SSH 网页版是否在跑"，以**端口监听**为准 ----
    //
    // ⚠️ 这里换过两次实现，前两次都错了，记下来免得再踩：
    //
    // 第一次：`pgrep -f "agygate.py.*--ssh-target"`
    //   pgrep 的正则跨整条命令行匹配，会把普通的本机网关（不带 --ssh-target）
    //   也当成 SSH 网页版 → status 显示 running=true，但 8897 根本没起。
    //
    // 第二次：pgrep 拿到 pid，再用 `ps -p <pid> -o command=` 核对命令行里
    //   有没有 --ssh-target。
    //   问题：在本机这个环境里 `ps` 对某些进程返回**空字符串**（沙箱限制），
    //   于是 contains 判断永远 false；更糟的是循环里 pid 取到的是已消失的
    //   幽灵进程，导致状态乱跳。
    //
    // 第三次（现在）：不去猜进程身份，直接看**配置里的端口有没有在监听**。
    //   理由很朴素：SSH 网页版的唯一目的是提供网络服务，
    //   端口在监听 = 服务在跑；端口不监听 = 服务没跑。
    //   这比推断进程命令行可靠得多，也不会受 ps 限制影响。
    let cfg = config::Config::load();
    let port = if cfg.ssh_web_port == 0 { 8897 } else { cfg.ssh_web_port };
    running_pid(port).unwrap_or(0)
}

/// 换口令：生成新的、落盘、并告诉调用方。
///
/// 为什么要换完立刻落盘（而不是等下次启动）：
///   用户换口令的意图通常是"刚才那个泄露了，立刻失效"。
///   如果只是改了内存、重启才生效，那泄露的口令还能用一个会话，等于没换。
fn rotate_token() {
    let mut cfg = config::Config::load();
    let t = cfg.rotate_token();
    let obj = serde_json::json!({ "ok": true, "token": t });
    println!("{}", obj);
}

/// 设置某个服务的开关。
/// 用法：agyhub set-enabled web 0|1
fn set_enabled(args: &[String]) {
    let mut cfg = config::Config::load();
    // 找 "web"/"ssh" 后面的 0/1
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
            println!("{}", serde_json::json!({"ok": false, "error": "用法: set-enabled web|ssh 0|1"}));
            std::process::exit(2);
        }
    }
    let _ = cfg.save();
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "web_enabled": cfg.web_enabled,
            "ssh_enabled": cfg.ssh_enabled,
        })
    );
}

/// 智能选端口并写回配置。输出 JSON 供 Swift 读取。
fn pick_port_cmd(args: &[String]) {
    let mut cfg = config::Config::load();
    let which = args
        .iter()
        .skip(1)
        .find(|a| *a == "web" || *a == "ssh_web")
        .cloned()
        .unwrap_or_else(|| "web".to_string());

    let preferred = if which == "ssh_web" { 8897 } else { 8898 };
    // 已经在跑的端口要认，否则每次查都"换一个新端口"
    let current = if which == "ssh_web" { cfg.ssh_web_port } else { cfg.web_port };
    // 只有"被我们自己占着"才保持，被别的程序占着必须换。
    let mut chosen = if current != 0 && config::port_owned_by_us(current) {
        current
    } else {
        config::pick_port(preferred)
    };

    // 两个服务不能撞同一个端口。
    // pick_port 只看"系统里是否有人监听"，而此刻另一个服务可能还没启动，
    // 于是两边各自算出来同一个端口。这里显式错开。
    let other = if which == "ssh_web" { cfg.web_port } else { cfg.ssh_web_port };
    if chosen == other && other != 0 {
        // 从当前值往后找，跳过被占的和另一个服务占的
        let mut p = chosen.saturating_add(1);
        while p <= 65535 {
            if config::port_free(p) && p != other {
                chosen = p;
                break;
            }
            p += 1;
        }
    }

    if which == "ssh_web" {
        cfg.ssh_web_port = chosen;
    } else {
        cfg.web_port = chosen;
    }
    let _ = cfg.save();

    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "service": which,
            "port": chosen,
            "preferred": preferred,
            "changed": chosen != preferred,
        })
    );
}

/// 端口是否有人在监听。
fn port_in_use(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_err()
}

/// 找出监听该端口的进程 pid。查不到返回 None。
fn running_pid(port: u16) -> Option<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{}", port), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next()?.trim().parse().ok()
}

/// 列出 ~/.ssh/config 里可用的主机别名。
///
/// 为什么要这个：SSH 网页版必须选一台开发机，用户不该去手敲主机名。
/// 菜单里做成可选项更省事。
fn list_ssh_hosts() {
    let cfg_path = match std::env::var("HOME") {
        Ok(h) => format!("{}/.ssh/config", h),
        Err(_) => {
            println!("{}", serde_json::json!({"ok": false, "error": "无 HOME"}));
            return;
        }
    };
    let text = std::fs::read_to_string(&cfg_path).unwrap_or_default();

    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // 匹配 "Host xxx" / "host xxx"（ssh config 大小写不敏感）
        let lower = t.to_lowercase();
        if !lower.starts_with("host ") && !lower.starts_with("host\t") {
            continue;
        }
        let rest = t[4..].trim();
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

    println!(
        "{}",
        serde_json::json!({ "ok": true, "hosts": hosts })
    );
}

/// 开始 SSH 网页版：拉起 agygate.py --ssh-target。
///
/// 为什么用 Python 版而不是在 Rust 里重写：
///   远端模式要做「ssh 隧道 + 扫远端 language_server 端口 + 判断 HTTP/HTTPS
///   + 把远端页面转出来」这一整套，Python 版 agygate.py 里已经实现并
///   在真机上验证过。在 Rust 里重写一遍风险高、收益低，
///   而且一旦出问题就是两套代码各修一遍。
fn start_ssh_web(args: &[String]) -> String {
    let mut cfg = config::Config::load();
    cfg.normalize();

    // ---- 取目标主机 ----
    let target = args
        .iter()
        .skip(1)
        .enumerate()
        .find(|(_, a)| *a == "--target")
        .and_then(|(i, _)| args.get(i + 2))
        .cloned()
        .or_else(|| {
            args.iter()
                .skip(1)
                .find_map(|a| a.strip_prefix("--target=").map(|v| v.to_string()))
        })
        .unwrap_or_else(|| cfg.ssh_target.clone());

    let target = if target.trim().is_empty() {
        return serde_json::json!({
            "ok": false,
            "error": "没有指定开发机。请先在菜单里选一台（~/.ssh/config 里的别名）。"
        })
        .to_string();
    } else {
        target.trim().to_string()
    };

    let port = if cfg.ssh_web_port == 0 { 8897 } else { cfg.ssh_web_port };

    // ---- 端口已被占用时，复用而不是杀掉 ----
    //
    // ⚠️ 这条纪律是被事故换来的：
    //   早期实现写的是「只要 pgrep 到 agygate --ssh-target 就 kill」，
    //   结果把用户从终端自己跑的那份远端网页版杀了 ——
    //   表现就是"昨天还能打开的 8897 打不开了"。
    //   现在只判断端口上有没有在监听，有就复用（口令共用，协议一样）。
    if let Some(holder) = running_pid(port) {
        return serde_json::json!({
            "ok": true,
            "pid": holder,
            "port": port,
            "target": target,
            "reused": true,
        })
        .to_string();
    }

    // ---- 启动自己（同一个二进制，带 --ssh-target）----
    //
    // 关键变化：**不再启动 Python 的 agygate.py**。
    //
    // 原来这里 fork 一个 python agygate.py，意味着 App 分发出去之后
    // 目标机器上必须装 Python + h2 库 + 整个工作区目录，
    // 否则 SSH 网页版直接不可用。
    // 现在 SSH 隧道与转发都由本二进制自己完成，真正单文件。
    let self_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({
                "ok": false,
                "error": format!("无法定位自身可执行文件: {}", e)
            })
            .to_string();
        }
    };

    let token = cfg.ensure_token();
    cfg.ssh_target = target.clone();
    let _ = cfg.save();

    let home = std::env::var("HOME").unwrap_or_default();
    let log_path = format!("{}/Library/Logs/AgyHub-sshweb.log", home);
    let _ = std::fs::create_dir_all(format!("{}/Library/Logs", home));

    let mut cmd = std::process::Command::new(&self_path);
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--ssh-target")
        .arg(&target);

    // 口令通过环境变量传，避免出现在 ps 里
    cmd.env("AGEHUB_TOKEN", &token);

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => {
            let f2 = match f.try_clone() {
                Ok(c) => c,
                Err(_) => {
                    return serde_json::json!({"ok": false, "error": "日志文件无法复制"})
                        .to_string()
                }
            };
            cmd.stdout(std::process::Stdio::from(f));
            cmd.stderr(std::process::Stdio::from(f2));
        }
        Err(_) => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }

    match cmd.spawn() {
        Ok(c) => serde_json::json!({
            "ok": true,
            "pid": c.id(),
            "port": port,
            "target": target,
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "ok": false,
            "error": format!("启动失败: {}", e)
        })
        .to_string(),
    }
}

/// 给 Antigravity 装 SSH 入口解锁钩子。
///
/// 单次执行、命令即返回：装的是 addScriptToEvaluateOnNewDocument，
/// 对当前文档和之后的新导航都生效，不需要常驻进程。
fn cmd_ssh_unlock() {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": format!("创建运行时失败: {}", e)})
            );
            return;
        }
    };

    let res = rt.block_on(actions::unlock_ssh_entry());
    match res {
        Ok(msg) => println!(
            "{}",
            serde_json::json!({"ok": true, "message": msg, "running": true, "pid": std::process::id()})
        ),
        Err(e) => println!("{}", serde_json::json!({"ok": false, "error": e})),
    }
}

/// 检查中文是否已注入（用于 status 显示）。
///
/// 原来这里 pgrep 查 `daemon.py` —— 那套常驻模式已经废弃了：
/// 现在中文由 App 巡检重注，没有常驻进程。所以改成读配置判断。
fn find_zh_pid() -> u32 {
    // 没有常驻进程了，固定返回 0。
    // status 里的 running 字段由调用方根据 zh_enabled + 页面状态判断。
    0
}

/// 启停中文语言包。
///
/// 为什么这件事要由本 App 来做（用户的原话）：
///   「现在 antigravity 只要重新启动，中文包会失效，所以这个可以用我们的 app
///     来做这个持久化」
///
/// 实现方式：中文包的 daemon.py 本身是**常驻**的 —— 它每秒 tick 一次，
/// 每 2 秒探一次 window.__AGY_ZH__ 是否还在，丢了就重新注入；
/// 而且它会读 DevToolsActivePort，Antigravity 重启后端口变了也能跟上。
///
/// 所以"持久化"不需要我们另写逻辑，只要**保证 daemon 一直在跑**即可：
///   · App 启动时如果配置里启用了中文，就把 daemon 拉起来
///   · daemon 自己会处理 Antigravity 的重启
/// 这也解释了为什么以前"双击一次只管一次"：那个脚本是打完就退的。
fn toggle_zh(args: &[String]) {
    let mut cfg = config::Config::load();
    let action = args
        .iter()
        .skip(1)
        .find(|a| *a == "on" || *a == "off" || *a == "status")
        .cloned()
        .unwrap_or_else(|| "status".to_string());

    let pid = find_zh_pid();

    match action.as_str() {
        "status" => {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "enabled": cfg.zh_enabled,
                    "running": pid > 0,
                    "pid": pid,
                })
            );
        }
        "off" => {
            // ---- 关中文：光杀进程不够 ----
            //
            // 中文包的官方停止脚本自己也写了：
            //   「Antigravity 界面会保持当前中文，刷新后恢复英文。」
            //
            // 因为翻译是注入进 DOM 的：文字已经被改成中文了，
            // 注入脚本的 destroy() 只断开 MutationObserver，不会把字改回去。
            // 用户取消勾选后看到界面仍是中文，会认为功能坏了 —— 确实踩过。
            //
            // 所以要额外调 stop-zh.cjs：连 CDP 清掉注入并刷新页面，
            // 让界面回到原始英文。
            if pid > 0 {
                let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
                // 等它真的退出，否则它可能在我们刷新后又注入一次
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            // 用内置 CDP 客户端还原界面，不再依赖外部 node 脚本。
            //
            // 原来是 fork node 跑 stop-zh.cjs，那样分发出去要求目标机器
            // 装了 node 且工作区在固定路径。现在这套逻辑编译进了二进制。
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let mut restored = false;
            if let Ok(rt) = rt {
                if let Ok(msg) = rt.block_on(actions::restore_zh_ui()) {
                    tracing::info!("还原界面: {}", msg);
                    restored = true;
                }
            }

            cfg.zh_enabled = false;
            let _ = cfg.save();
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "enabled": false,
                    "running": false,
                    // 界面是否已还原成英文。false 时前端可以提示用户手动刷新。
                    "ui_restored": restored,
                })
            );
        }
        _ => {
            // ---- on：直接用内置 CDP 注入，不启动任何外部进程 ----
            //
            // 原来这里 fork python daemon.py 常驻，靠它每 2 秒探活重新注入。
            // 现在改成"一次注入 + App 巡检重注"：
            //   · 注入用的是 addScriptToEvaluateOnNewDocument，
            //     对当前文档和之后的新导航都生效
            //   · 页面刷新/新开窗口后钩子会丢，由 App 的 guardServices
            //     每 3 秒检查一次、发现丢了就重新调本命令
            //
            // 好处：不再依赖 python、不再依赖中文包目录在固定路径，
            // 中文词库已经编译进二进制。
            let pack = match actions::load_zh_pack() {
                Some(p) => p,
                None => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": "内置中文词库解析失败"
                        })
                    );
                    return;
                }
            };

            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    println!(
                        "{}",
                        serde_json::json!({"ok": false, "error": format!("创建运行时失败: {}", e)})
                    );
                    return;
                }
            };

            let result = rt.block_on(actions::apply_zh(&pack.0, &pack.1, &pack.2));

            match result {
                Ok(msg) => {
                    cfg.zh_enabled = true;
                    let _ = cfg.save();
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": true,
                            "enabled": true,
                            "running": true,
                            "message": msg,
                        })
                    );
                }
                Err(e) => {
                    // 注入失败但配置仍然记下来：用户下次打开
                    // Antigravity 时 App 会重试。
                    cfg.zh_enabled = true;
                    let _ = cfg.save();
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "enabled": true,
                            "error": format!("注入失败（Antigravity 可能没开）: {}", e)
                        })
                    );
                }
            }
        }
    }
}

/// 设置指定端口（用户自定义）。
///
/// 用法：agyhub set-port web 9000
///
/// ⚠️ 与 pick-port 的区别：
///   pick-port 是"自动找一个空闲的"，会静默换端口。
///   set-port 是"用户指定，必须原样生效"，如果被占用要**明确报错**，
///   而不是偷偷换一个 —— 用户填了 9000 却跑了 9001，会让人困惑。
fn set_port_cmd(args: &[String]) {
    let mut cfg = config::Config::load();
    // 先校正，保证下面比较用的是"有效的另一个端口"。
    // 不做的话，配置里残留的脏值（比如两个服务同端口）会让冲突检查失效 ——
    // 实测出现过"设成与 SSH 相同的端口却报成功"。
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

    let (which, port) = match (which.as_str(), num) {
        ("web", Some(p)) => ("web", p),
        ("ssh_web", Some(p)) => ("ssh_web", p),
        _ => {
            println!("{}", serde_json::json!({
                "ok": false,
                "error": "用法: set-port web|ssh_web <端口号>"
            }));
            std::process::exit(2);
        }
    };

    // 端口范围检查。1-1023 需要 root，普通人跑不了，直接说明。
    if port < 1024 {
        println!("{}", serde_json::json!({
            "ok": false,
            "error": format!("端口 {} 小于 1024，需要管理员权限才能监听，请改用 1024 以上", port),
            "port": port,
        }));
        std::process::exit(1);
    }

    // 与另一个服务冲突
    let other = if which == "web" { cfg.ssh_web_port } else { cfg.web_port };
    if port == other {
        println!("{}", serde_json::json!({
            "ok": false,
            "error": format!("端口 {} 已被另一个服务使用，请换一个", port),
            "port": port,
            "conflict_with": other,
        }));
        std::process::exit(1);
    }

    // 被系统里其他程序占用 —— 这是用户最可能遇到的情况，
    // 要说清楚"谁占了"，并给一个建议端口。
    if !config::port_free(port) && !config::port_owned_by_us(port) {
        let holder = port_holder(port);
        let suggestion = config::pick_port(port);
        println!("{}", serde_json::json!({
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

    if which == "web" {
        cfg.web_port = port;
    } else {
        cfg.ssh_web_port = port;
    }
    let _ = cfg.save();

    println!("{}", serde_json::json!({
        "ok": true,
        "service": which,
        "port": port,
    }));
}

/// 查端口被谁占用，返回可读描述（进程名）。
fn port_holder(port: u16) -> String {
    let out = match std::process::Command::new("lsof")
        .args(["-nP", "-iTCP:{}", "-sTCP:LISTEN"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    let _ = out; // 占位，下面用带端口的版本
    let out = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{}", port), "-sTCP:LISTEN"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            // 跳过表头行，取第一行的命令行名
            s.lines()
                .skip(1)
                .next()
                .and_then(|l| l.split_whitespace().next())
                .unwrap_or("")
                .to_string()
        }
        Err(_) => String::new(),
    }
}

/// 环境自检：把常见的启动失败原因提前暴露出来。
fn doctor(port: u16) {
    println!("agyhub 环境检查\n");

    // 1. 上游
    match upstream::find_upstream(true) {
        Some(p) => println!("  ✓ Antigravity 上游: 127.0.0.1:{}", p),
        None => println!("  ✗ 找不到 Antigravity 上游\n    请先打开 Antigravity 应用"),
    }

    // 2. 本机 IP
    let ips = cert::local_ips();
    println!("  本机地址:");
    for ip in &ips {
        println!("      {}", ip);
        let _ = ip;
    }

    // 3. 证书
    let dir = data_dir().join("certs");
    match cert::ensure_certs(&dir) {
        Ok(b) => {
            println!("  ✓ 证书就绪（{}）", dir.display());
            println!("      SAN 覆盖上面所有地址");
            // 顺手验证能不能装成 TLS —— 证书生成成功但加载失败是常见问题
            match cert::tls_acceptor(&b) {
                Ok(_) => println!("  ✓ TLS 配置可用"),
                Err(e) => println!("  ✗ TLS 配置失败: {}", e),
            }
        }
        Err(e) => println!("  ✗ 证书生成失败: {}", e),
    }

    // 4. 端口占用
    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(_) => println!("  ✓ 端口 {} 可用", port),
        Err(e) => println!("  ✗ 端口 {} 被占用: {}", port, e),
    }
}

/// 数据目录：`~/.agyhub`
fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agyhub")
}

async fn run(mut port: u16, ssh_target: Option<String>) {
    let dir = data_dir();

    // ---- 端口智能选择 ----
    //
    // 用户点「启动服务」时，8898 很可能已被占用（Python 版 agygate 常驻在那）。
    // 直接 bind 失败会让用户看到"点了没反应"，体验很差。
    // 这里自动找下一个空闲端口，并把实际用的端口写回配置 ——
    // 菜单栏下次 status 就能显示真实端口。
    let mut cfg = config::Config::load();
    if config::port_free(port) {
        // 期望端口可用，直接用
    } else if cfg.web_port != 0 && config::port_owned_by_us(cfg.web_port) {
        // 配置里记录的那个正被我们自己占用（重复启动），保持不变
        port = cfg.web_port;
    } else {
        let picked = config::pick_port(port);
        tracing::warn!("端口 {} 已被占用，改用 {}", port, picked);
        port = picked;
    }
    if cfg.web_port != port {
        cfg.web_port = port;
        let _ = cfg.save();
    }
    tracing::info!("数据目录: {}", dir.display());

    let bundle = match cert::ensure_certs(&dir.join("certs")) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("证书生成失败: {}", e);
            std::process::exit(1);
        }
    };
    tracing::info!("证书就绪");

    let tls = match cert::tls_acceptor(&bundle) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("TLS 配置失败: {}", e);
            std::process::exit(1);
        }
    };

    // ---- SSH 模式：先建隧道 ----
    //
    // 有 --ssh-target 时，上游不是本机的 Antigravity，而是
    // 远端开发机的 language_server（通过 ssh -L 映射到本地）。
    // 所以必须在这里先把隧道建好，并把 upstream 指过去。
    if let Some(target) = ssh_target.clone() {
        tracing::info!("远端模式：{}", target);
        let mut tun = tunnel::RemoteTunnel::new(&target, 0)
            .with_logger(|m| tracing::info!("{}", m));
        match tun.start() {
            Ok(()) => {
                // 让 upstream 模块知道上游在哪
                let local = tun.local_port;
                let mut t = tun;
                tracing::info!("上游（远端）: 127.0.0.1:{}", local);
                upstream::set_remote(local);
                // 隧道进程要活到程序结束：放到后台线程持有
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                        if !t.alive() {
                            tracing::warn!("隧道断开，尝试重连");
                            let _ = t.start();
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!("隧道建立失败: {}", e);
                eprintln!("✗ 隧道建立失败: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // 启动时探一次上游，尽早把「Antigravity 没开」这个情况说清楚。
        // 不做硬失败：用户可能先开网关再开 Antigravity，运行时会自动重试。
        match upstream::find_upstream(true) {
            Some(p) => tracing::info!("上游 Antigravity: 127.0.0.1:{}", p),
            None => tracing::warn!("暂时找不到 Antigravity，请确认应用已打开（运行中会持续重试）"),
        }
    }

    let ctx = Arc::new(server::Ctx {
        token: read_token(),
        verbose: std::env::var("RUST_LOG")
            .map(|v| v.contains("debug"))
            .unwrap_or(false),
        ca_pem: bundle.ca_pem.clone(),
        port,
    });

    // 打印可访问地址，方便用户直接复制到手机
    let ips = cert::local_ips();
    println!();
    println!("  Antigravity Hub 已启动");
    println!();
    for ip in &ips {
        let host = if ip.is_loopback() {
            "localhost".to_string()
        } else {
            ip.to_string()
        };
        let t = if ctx.token.is_empty() {
            String::new()
        } else {
            format!("?t={}", ctx.token)
        };
        println!("      https://{}:{}/{}{}", host, port, if t.is_empty() { "" } else { "" }, t);
    }
    println!();
    println!("  手机与电脑需在同一局域网。首次访问会提示证书，");
    println!("  在手机上装一次 https://<上面的地址>/__agy/ca.crt 即可长期免提示。");
    println!();

    if let Err(e) = server::serve(port, tls, ctx).await {
        tracing::error!("服务启动失败: {}", e);
        std::process::exit(1);
    }
}

/// 口令。
///
/// 优先级：环境变量 AGEHUB_TOKEN > 配置文件 > 空（不校验）。
/// 空口令在局域网里等于没有保护，所以默认要生成一个持久化的。
fn read_token() -> String {
    if let Ok(t) = std::env::var("AGEHUB_TOKEN") {
        return t;
    }
    // 优先用 config.json 里的口令（菜单栏 App 换口令时写这里）。
    // 旧的 ~/.agyhub/token 文件作为兜底读取，保证升级后不影响现有部署。
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
    let t = cfg.ensure_token();
    return t;
}

#[allow(dead_code)]
fn read_token_legacy() -> String {
    let p = data_dir().join("token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    // 生成一个 16 位十六进制口令并持久化。
    // 持久化很重要：手机收藏的链接带着口令，重启后换口令就直接 401。
    let t = random_token();
    let _ = std::fs::create_dir_all(data_dir());
    if std::fs::write(&p, &t).is_ok() {
        // 口令等于「访问密码」，不能让同机其他用户读到。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(&p) {
                let mut perm = md.permissions();
                perm.set_mode(0o600);
                let _ = std::fs::set_permissions(&p, perm);
            }
        }
    }
    t
}

fn random_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32;
    let mut x = seed | 1;
    let mut s = String::with_capacity(16);
    for _ in 0..16 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push(char::from_digit((x % 16) as u32, 16).unwrap());
    }
    s
}
