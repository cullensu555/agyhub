//! SSH 隧道：把远端开发机的 language_server 端口映射到本机。
//!
//! # 为什么需要这个模块
//!
//! 手机浏览器没法自己连远端开发机。但 Mac 可以。
//! 所以本机起一条 `ssh -L` 隧道，把远端的 language_server 端口
//! 映射到 127.0.0.1，之后所有请求原样转发过去。
//!
//! 手机打开网关端口，看到的就是**远端开发机**的完整界面：
//! 文件树、终端、AI 全在开发机上跑。
//!
//! # 从 Python 迁移过来时的注意点
//!
//! 原实现是 `agygate.py` 里的 `RemoteTunnel` 类。迁移时保留了三处
//! 用真金白银换来的经验，都在对应代码旁写了注释：
//!
//! 1. **必须扫远端端口**——远端启动时用 `--https_server_port 0`，
//!    端口是随机的，写死没用。
//! 2. **必须用 SSH 连接复用**——否则每个候选端口都要一次完整握手，
//!    单次 0.3~1 秒，探测几个端口就要好几秒。
//! 3. **存活探测要放宽**——AI 推理时远端可能长时间不说话，
//!    探测太激进会被误判成断线，界面反复闪 `Connection lost`。
//!
//! # 远端要求
//!
//! - 该机器已装好 language_server（先在 Mac 桌面版连过一次即可自动部署）
//! - 密钥免密登录
//! - 该机器自身能出网（AI 推理由远端进程发起，不绕回本机）

use std::io;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 一条到远端开发机的隧道。
pub struct RemoteTunnel {
    /// ssh 目标（`~/.ssh/config` 里的别名，如 `dev`）
    pub target: String,
    /// 本机监听端口（映射出来的那个）
    pub local_port: u16,
    /// 远端 language_server 的实际端口
    pub remote_port: u16,
    /// 远端 language_server 的主 CSRF 口令（取不到时为空）。
    ///
    /// 2.33.0 起桌面版经官方 SSH 部署的远端服务带
    /// `AGY_REQUIRE_AUTHENTICATED_UI_BOOTSTRAP=1` 启动：首页里的
    /// `__APP_CONFIG__.csrfToken` 被置空，页面要先拿一次性令牌（`#t=`）
    /// 去 `/auth` 换会话口令。那个令牌只有桌面版主进程能签发，
    /// 浏览器经网关打开时什么口令都没有 → 前端报 "Missing LS CSRF token"，白屏。
    /// 所以由网关把主口令填回首页（等同旧版服务把口令直接写在页面里）。
    pub csrf_token: String,
    /// `ssh -N` 子进程
    proc: Option<Child>,
    /// 日志回调
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl RemoteTunnel {
    pub fn new(target: &str, local_port: u16) -> Self {
        RemoteTunnel {
            target: target.to_string(),
            local_port,
            remote_port: 0,
            csrf_token: String::new(),
            proc: None,
            log: Box::new(|_| {}),
        }
    }

    pub fn with_logger<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.log = Box::new(f);
        self
    }

    fn say(&self, msg: &str) {
        (self.log)(msg);
    }

    /// 建立隧道：发现远端端口 → 逐个试映射 → 找到 HTTP 上游后固定下来。
    pub fn start(&mut self) -> io::Result<()> {
        let (candidates, tokens) = self.discover_remote_ports()?;
        self.say(&format!(
            "远端 {} 候选端口: {:?}",
            self.target, candidates
        ));

        // 先把旧的 ssh 子进程和后台复用连接都收掉（重连时）。
        // 网络断过之后旧的复用连接可能已经半死不活，复用它只会让新转发也挂住。
        self.stop();
        set_active_target(&self.target);
        shutdown_master();

        let mut tried = Vec::new();
        for rp in &candidates {
            // ⚠️ 每个候选端口用**新的**本地端口。
            //
            // 开了 ControlMaster 复用后，-L 转发登记在后台的 master 连接上，
            // 杀掉我们这个 ssh 子进程并不一定会撤掉转发。
            // 如果所有候选都复用同一个本地端口，第一个候选失败后它的转发
            // 还占着这个端口，后面的候选全都 "Address already in use" ——
            // 远端 HTTPS 端口恰好排在 HTTP 端口前面时，SSH 网页版就永远起不来。
            let local = pick_free_port()?;
            match self.open_forward(*rp, local) {
                Ok(proc) => {
                    // ⚠️ 必须等端口真的可连再 probe。
                    //
                    // ssh 进程 fork 出来只是"开始了"，隧道真正打通要等到
                    // 远端握手完成 + -L 监听建立，通常 0.5~2 秒
                    // （跨洋链路更久）。不等就探，一定探空，
                    // 于是所有候选端口都被判成"非 HTTP 上游"。
                    // 实测：不加等待时两个候选端口全部误判失败。
                    if !wait_port(local, Duration::from_secs(8)) {
                        tried.push(format!("{}(隧道未就绪)", rp));
                        let mut p = proc;
                        let _ = p.kill();
                        let _ = p.wait();
                        self.cancel_forward(local, *rp);
                        continue;
                    }

                    // probe 必须打在**已映射到本机**的端口上。
                    // 直接探远端端口探到的是本机服务，没有任何意义。
                    if probe_http(local) {
                        SSH_CHILD_PID.store(proc.id(), std::sync::atomic::Ordering::SeqCst);
                        self.proc = Some(proc);
                        self.local_port = local;
                        self.remote_port = *rp;
                        self.csrf_token = tokens
                            .iter()
                            .find(|(p, _)| p == rp)
                            .map(|(_, t)| t.clone())
                            .unwrap_or_default();
                        self.say(&format!(
                            "隧道就绪 127.0.0.1:{} → {}:{}（明文 HTTP，{}）",
                            local,
                            self.target,
                            rp,
                            if self.csrf_token.is_empty() { "未取到远端口令" } else { "已取到远端口令" }
                        ));
                        return Ok(());
                    }
                    tried.push(format!("{}(非 HTTP 上游)", rp));
                    let mut p = proc;
                    let _ = p.kill();
                    let _ = p.wait();
                    self.cancel_forward(local, *rp);
                }
                Err(e) => tried.push(format!("{}({})", rp, e)),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "无法建立到 {} 的隧道。已尝试: {}",
                self.target,
                tried.join(", ")
            ),
        ))
    }

    /// 枚举远端 language_server 监听的端口。
    ///
    /// 远端启动参数是 `--https_server_port 0 / --http_server_port 0`，
    /// 即**随机端口**，必须扫。这里只返回候选，实际判定由 `open_forward`
    /// 之后的 `probe_http` 完成（要区分 HTTP 和 HTTPS）。
    fn discover_remote_ports(&self) -> io::Result<(Vec<u16>, Vec<(u16, String)>)> {
        // 同一条 ssh 顺带读出 discovery 文件（端口 → 主口令），省一次完整握手。
        // 文件由远端 language_server 自己写，权限 600，只有本账号能读。
        let out = self.ssh_run(&format!(
            "ss -lntp 2>/dev/null || netstat -lntp 2>/dev/null; echo '{}'; \
             for f in ~/.gemini/antigravity/daemon/ls_*.json; do [ -f \"$f\" ] && cat \"$f\"; echo; done",
            DISCOVERY_MARK
        ))?;
        let (out, tokens) = match out.split_once(DISCOVERY_MARK) {
            Some((ss, disc)) => (ss.to_string(), parse_discovery(disc)),
            None => (out, Vec::new()),
        };
        if out.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "无法在 {} 上枚举监听端口（缺少 ss/netstat？）",
                    self.target
                ),
            ));
        }

        let mut ports = Vec::new();
        for line in out.lines() {
            if !line.contains("language_server") {
                continue;
            }
            // 匹配 127.0.0.1:端口
            if let Some(idx) = line.find("127.0.0.1:") {
                let rest = &line[idx + "127.0.0.1:".len()..];
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse::<u16>() {
                    if !ports.contains(&p) {
                        ports.push(p);
                    }
                }
            }
        }

        if ports.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} 上没有 language_server 在跑。\n\
                     请先在 Mac 桌面版连一次这台机器，让它自动部署服务器。",
                    self.target
                ),
            ));
        }

        ports.sort_unstable();
        Ok((ports, tokens))
    }

    /// 在远端执行命令，返回 stdout。
    fn ssh_run(&self, command: &str) -> io::Result<String> {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            // ⚠️ 不复用、也不创建复用连接。
            // 用户自己的 ~/.ssh/config 里常有 ControlMaster auto + ControlPersist：
            // 那样这条命令会在后台留下一个复用连接，它一直握着我们读取输出的管道，
            // 读取就永远等不到结束 —— SSH 网页版卡在"正在连接开发机…"。
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            &self.target,
            command,
        ]);
        // ConnectTimeout 只管建连；连上之后远端卡住也不能无限等
        let out = crate::sys::output_timeout(cmd, Duration::from_secs(40)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ssh {} 执行超时（40 秒）", self.target),
            )
        })?;

        if !out.status.success() {
            let raw = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("ssh {} 失败：{}{}", self.target, raw, ssh_hint(&raw)),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 建立一条 `-L` 转发。
    ///
    /// ## 参数为什么这么设
    ///
    /// **复用连接**（`ControlMaster=auto` + `ControlPersist`）：
    /// 这是性能关键。不开复用时，每个候选端口都要一次完整 SSH 握手
    /// （密钥交换 + 认证 + 加密协商），单次 0.3~1 秒。
    /// 实测优化前 SSH 网页版首字节 5 秒，优化后 0.6 秒。
    ///
    /// **存活探测放宽**（`ServerAliveInterval=30` × `CountMax=6`）：
    /// 原来 15×3 = 45 秒没响应就判死。但 AI 推理、大文件索引时
    /// 远端完全可能长时间静默，被误判成断线 → 界面反复
    /// `Connection lost. Reconnecting...`。改成 3 分钟容忍。
    ///
    /// **关闭压缩**（`Compression=no`）：局域网/近距链路压缩
    /// 只增加 CPU 和延迟，不减少传输时间。
    fn open_forward(&self, remote_port: u16, local_port: u16) -> io::Result<Child> {
        let control_dir = control_dir();
        let _ = std::fs::create_dir_all(&control_dir);
        crate::sys::chmod_700(std::path::Path::new(&control_dir));

        let control_path = control_path();

        let child = Command::new("ssh")
            .args([
                "-N",
                "-o", "BatchMode=yes",
                "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=30",
                "-o", "ServerAliveCountMax=6",
                "-o", "TCPKeepAlive=yes",
                "-o", "ConnectTimeout=10",
                // SSH 连接复用（性能关键）
                "-o", "ControlMaster=auto",
                "-o", &format!("ControlPath={}", control_path),
                "-o", "ControlPersist=600",
                // 本地链路不需要压缩
                "-o", "Compression=no",
                "-L",
                &format!("127.0.0.1:{}:127.0.0.1:{}", local_port, remote_port),
                &self.target,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // ⚠️ 不能用 piped()：ssh -N 会常驻，没人读的管道写满 64KB 后
            // ssh 就阻塞在 write 上，隧道随之卡死。
            // 继承父进程的 stderr = 直接写进 AgyHub-sshweb.log，出错时也看得到原因。
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("拉起 ssh 失败: {}", e)))?;

        Ok(child)
    }

    /// 隧道是否还活着（端到端：通过隧道能拿到 Antigravity 首页）。
    ///
    /// 原来只看本地端口能不能连上。但本地端口由 ssh（或后台的 ControlMaster）
    /// 监听，远端 language_server 重启换了端口后，本地端口照样能连上，
    /// 只是每个请求都失败 —— 于是永远不触发重连，SSH 网页版一直打不开。
    pub fn alive(&self) -> bool {
        match &self.proc {
            None => false,
            // 守护探测用长超时：探测和页面请求走同一条 ssh 连接，
            // 页面下载大文件（2.33.0 的 main.js 12MB，跨境链路要几十秒）时
            // 探测请求会排在后面。2 秒超时会连续判失败 → 重连 → 杀掉复用连接，
            // 正在下载的 main.js 被截断，页面白屏（实测断在 6.8/7.8/9.9MB）。
            Some(_) => probe_http_timeout(self.local_port, ALIVE_PROBE_TIMEOUT),
        }
    }

    /// 停止隧道（只停前台 ssh 子进程；后台复用连接由 [`shutdown_master`] 处理）。
    pub fn stop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        SSH_CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// 撤掉复用连接上某条转发（候选端口试失败时用）。
    ///
    /// 转发登记在后台的复用连接上，杀掉前台 ssh 不会撤掉它，
    /// 不撤的话失败的候选会一直占着本地端口。
    fn cancel_forward(&self, local: u16, remote: u16) {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o",
            &format!("ControlPath={}", control_path()),
            "-O",
            "cancel",
            "-L",
            &format!("127.0.0.1:{}:127.0.0.1:{}", local, remote),
            &self.target,
        ]);
        let _ = crate::sys::output_timeout(cmd, Duration::from_secs(5));
    }
}

/// 当前 ssh 子进程的 pid。
///
/// 网关被 SIGTERM 停掉时 Rust 不会跑析构函数，`Drop` 里的清理不会执行，
/// ssh 子进程就成了孤儿（每重启一次 SSH 网页版就多留一个）。
/// 信号处理里读这个值把它杀掉。
pub static SSH_CHILD_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 当前连的开发机（关闭复用连接时要用）。
static ACTIVE_TARGET: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn set_active_target(t: &str) {
    if let Ok(mut g) = ACTIVE_TARGET.lock() {
        *g = t.to_string();
    }
}

/// 关掉 AgyHub 专用的 SSH 复用连接（连同上面的所有转发）。
///
/// ## 为什么必须做
///
/// `ControlPersist=600` 会让第一条 ssh 在后台留一个复用连接（master），
/// 端口转发实际挂在它上面。只杀前台的 ssh 子进程，master 和转发还会
/// 再活 10 分钟：停了 SSH 网页版，本地端口和到开发机的连接却都还在。
///
/// 复用连接用的是 AgyHub 专用的 socket（`agyhub-%C`），不会碰到用户自己的
/// ssh 复用连接，所以这里可以放心 `-O exit`。
pub fn shutdown_master() {
    let target = ACTIVE_TARGET.lock().map(|g| g.clone()).unwrap_or_default();
    if target.is_empty() {
        return;
    }
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        &format!("ControlPath={}", control_path()),
        "-O",
        "exit",
        &target,
    ]);
    let _ = crate::sys::output_timeout(cmd, Duration::from_secs(5));
}

/// 退出时的清理：前台 ssh 子进程 + 后台复用连接。
pub fn cleanup() {
    let pid = SSH_CHILD_PID.swap(0, std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    shutdown_master();
}

impl Drop for RemoteTunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 把 ssh 的英文报错翻成用户该做的事。
fn ssh_hint(stderr: &str) -> &'static str {
    let s = stderr.to_ascii_lowercase();
    if s.contains("permission denied") {
        "\n→ 需要密钥免密登录：在终端执行 ssh-copy-id <主机名>，确认 ssh <主机名> 不再要密码"
    } else if s.contains("host key verification failed") {
        "\n→ 先在终端手动 ssh 一次这台机器，输入 yes 接受主机指纹"
    } else if s.contains("could not resolve hostname") {
        "\n→ 主机名解析失败：检查 ~/.ssh/config 里的 HostName"
    } else if s.contains("no route to host") {
        "\n→ 连不上这台机器：检查网络 / VPN；开发机在局域网内的话，\
         还要在「系统设置 → 隐私与安全性 → 本地网络」里允许 AgyHub"
    } else if s.contains("timed out") {
        "\n→ 连不上这台机器：检查网络 / VPN"
    } else if s.contains("connection refused") {
        "\n→ 对方没开 SSH 服务或端口不对"
    } else {
        ""
    }
}

/// SSH ControlMaster socket 放哪。
///
/// 放 `~/.ssh/cm` 而不是 `/tmp`：`/tmp` 会被系统清理，
/// socket 一失效复用就断了，表现为"过一会儿又变慢"。
fn control_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{}/.ssh/cm", home)
}

/// AgyHub 专用的复用 socket 路径。
///
/// `%C` 是连接参数的哈希（定长 40 字符）：
///   · 和用户自己的 `%r@%h-%p` 复用连接分开，关闭时不会误伤
///   · 定长，不会因为主机名太长超过 unix socket 104 字节的路径上限
fn control_path() -> String {
    format!("{}/agyhub-%C", control_dir())
}

/// 远端命令输出里分隔「端口列表」和「discovery 文件」的标记。
const DISCOVERY_MARK: &str = "---agyhub-discovery---";

/// 从远端 discovery 文件（多个拼在一起的 JSON 对象，带缩进换行：
/// `{"httpPort":…,"csrfToken":"…"}`）里取出「HTTP 端口 → 主口令」。
/// 解析不了就到此为止：旧版远端没有这个目录，或字段改名，
/// 都退回到不填口令（即旧行为）。
fn parse_discovery(text: &str) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    let stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    for v in stream {
        let v = match v {
            Ok(v) => v,
            Err(_) => break,
        };
        let port = v.get("httpPort").and_then(|p| p.as_u64()).unwrap_or(0);
        let token = v.get("csrfToken").and_then(|t| t.as_str()).unwrap_or("");
        // 口令只允许 UUID 那类字符：它要原样拼进页面的 JS 里，不能带引号等。
        let safe = !token.is_empty()
            && token.len() <= 128
            && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if port > 0 && port <= u16::MAX as u64 && safe {
            out.push((port as u16, token.to_string()));
        }
    }
    out
}

/// 挑一个空闲的本地端口。
fn pick_free_port() -> io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let p = l.local_addr()?.port();
    drop(l);
    Ok(p)
}

/// 探一下某端口上跑的是不是我们要的那个 HTTP 上游。
///
/// 判据是响应里有 `csrfToken`：Antigravity 首页的
/// `__APP_CONFIG__` 里带这个字段，别的东西碰巧占这个端口不会有。
fn probe_http(port: u16) -> bool {
    probe_http_timeout(port, Duration::from_secs(2))
}

/// 隧道守护的存活探测超时（见 [`RemoteTunnel::alive`]）。
const ALIVE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn probe_http_timeout(port: u16, timeout: Duration) -> bool {
    use std::io::{Read, Write};

    let addr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let mut s = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));

    let req = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }

    let mut buf = Vec::new();
    let _ = s.take(4096).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.contains("csrfToken")
}

/// 等一下直到端口真的可连，或超时。
///
/// ssh 进程 fork 出来后端口不是立刻就能连的，需要短暂轮询。
pub fn wait_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(300),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}
