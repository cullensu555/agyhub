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
        let candidates = self.discover_remote_ports()?;
        self.say(&format!(
            "远端 {} 候选端口: {:?}",
            self.target, candidates
        ));

        let local = if self.local_port == 0 {
            pick_free_port()?
        } else {
            self.local_port
        };

        let mut tried = Vec::new();
        for rp in &candidates {
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
                        continue;
                    }

                    // probe 必须打在**已映射到本机**的端口上。
                    // 直接探远端端口探到的是本机服务，没有任何意义。
                    if probe_http(local) {
                        self.proc = Some(proc);
                        self.local_port = local;
                        self.remote_port = *rp;
                        self.say(&format!(
                            "隧道就绪 127.0.0.1:{} → {}:{}（明文 HTTP）",
                            local, self.target, rp
                        ));
                        return Ok(());
                    }
                    tried.push(format!("{}(非 HTTP 上游)", rp));
                    let mut p = proc;
                    let _ = p.kill();
                    let _ = p.wait();
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
    fn discover_remote_ports(&self) -> io::Result<Vec<u16>> {
        let out = self.ssh_run("ss -lntp 2>/dev/null || netstat -lntp 2>/dev/null")?;
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
        Ok(ports)
    }

    /// 在远端执行命令，返回 stdout。
    fn ssh_run(&self, command: &str) -> io::Result<String> {
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                &self.target,
                command,
            ])
            .output()?;

        if !out.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "ssh {} 执行失败: {}",
                    self.target,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
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

        let control_path = format!("{}/%r@%h-%p", control_dir);

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
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("拉起 ssh 失败: {}", e)))?;

        Ok(child)
    }

    /// 隧道是否还活着（进程在 + 端口可连）。
    pub fn alive(&self) -> bool {
        match &self.proc {
            None => false,
            Some(_) => {
                // 端口能连上就认为活着。
                // 不用 try_wait 是因为 ssh -N 会一直挂着，
                // 真正的死法是端口不通。
                TcpStream::connect_timeout(
                    &format!("127.0.0.1:{}", self.local_port).parse().unwrap(),
                    Duration::from_millis(500),
                )
                .is_ok()
            }
        }
    }

    /// 停止隧道。
    pub fn stop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

impl Drop for RemoteTunnel {
    fn drop(&mut self) {
        self.stop();
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
    use std::io::{Read, Write};

    let addr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));

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
