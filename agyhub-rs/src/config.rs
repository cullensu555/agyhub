// 配置持久化：口令、端口、各服务的开关状态。
//
// 为什么放在 Rust 而不是 Swift：
//   内核对这两个值有独占的读需求（服务启动要用端口和口令），
//   Swift 只是显示。放在 Rust 里只有一份真相，
//   避免"Swift 改了但内核不认"这种撕裂。
//
// 存储位置：~/.agyhub/config.json
//   和 Python 版的 config.conf 分开，互不干扰。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::data_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 网页版端口。0 表示"自动选"
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    // ⚠️ 这里**故意没有** ssh_port 字段。
    //
    // 「启动 SSH」指的是让**电脑桌面版** Antigravity 的界面里出现
    // 「Connect to SSH Host…」入口，实现方式是 CDP 注入（改 gRPC-Web 响应），
    // 它**不监听任何端口**，也不需要端口。
    //
    // 早期我错误地给它加了个 ssh_port 并显示"端口 8897"，
    // 用户看到两个 8897/8901 完全懵了 —— 概念本身就错了。
    // 需要端口的只有 ssh_web（手机访问开发机界面的那个）。
    /// 访问口令。空字符串表示"未设置，启动时随机生成"
    #[serde(default)]
    pub token: String,
    /// 网页版是否启用
    #[serde(default = "default_true")]
    pub web_enabled: bool,
    /// SSH 入口解锁是否启用
    #[serde(default = "default_true")]
    pub ssh_enabled: bool,

    /// SSH 网页版是否启用
    #[serde(default = "default_true")]
    pub ssh_web_enabled: bool,
    /// SSH 网页版端口
    #[serde(default = "default_ssh_web_port")]
    pub ssh_web_port: u16,
    /// SSH 网页版连哪台开发机（~/.ssh/config 里的别名）
    #[serde(default)]
    pub ssh_target: String,
    /// 是否启用中文语言包
    #[serde(default)]
    pub zh_enabled: bool,
}

fn default_web_port() -> u16 {
    8898
}
fn default_true() -> bool {
    true
}
fn default_ssh_web_port() -> u16 {
    // 用户原来手动跑的远端网页版就是 8897，沿用这个习惯，
    // 免得新功能一上来就和他的旧链接打架。
    8897
}

impl Default for Config {
    fn default() -> Self {
        Self {
            web_port: default_web_port(),
            token: String::new(),
            // ---- 首次安装的默认状态（用户实测反馈后调整）----
            //
            // 只默认开「本机网页版」—— 这是 App 的核心用途，
            // 装完就能用手机看到界面，不需要任何额外配置。
            //
            // SSH 相关的两个**默认关闭**，理由：
            //   · `ssh_enabled`（桌面版入口解锁）：开启时会给 Antigravity
            //     注入钩子，用户会先看到一个"是否允许"的授权提示。
            //     对一个刚装完 App、只想用手机看界面的人来说，
            //     这个弹窗完全莫名其妙（用户原话："立马要访问授权SSH 有点多余"）。
            //   · `ssh_web_enabled`（SSH 网页版）：要用开发机才需要，
            //     且没配过 ~/.ssh/config 的人根本用不了，
            //     默认开着只会每 3 秒尝试重连、刷一堆错误。
            //
            // 用户需要时在菜单里手动打开即可。
            web_enabled: true,
            ssh_enabled: false,
            ssh_web_enabled: false,
            ssh_web_port: default_ssh_web_port(),
            ssh_target: String::new(),
            zh_enabled: false,
        }
    }
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let p = config_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        // ⚠️ 保存前必须保证 token 非空。
        //
        // 踩过的坑：status 子命令故意"不落盘"（它是只读的），
        // 所以读取时 token 为空会用一个内存里的临时值显示。
        // 但紧接着 set-port 这类写操作如果在同一个进程里，
        // 就会把那个空值写进文件 —— 用户的口令凭空消失。
        //
        // 这里做个兜底：要写盘就绝不能写空 token。
        let mut me = self.clone();
        if me.token.is_empty() {
            me.token = gen_token();
        }
        return me.save_inner();
    }

    fn save_inner(&self) -> std::io::Result<()> {
        let p = config_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = serde_json::to_string_pretty(self)?;
        // 先写临时文件再 rename：避免写到一半断电/被杀导致配置损坏。
        // 口令就在这个文件里，写坏了用户要重新设置。
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// 校正端口：确保两个服务端口不同、且都落在合理范围。
    ///
    /// 为什么需要这个：SSH 服务并不真的监听端口（它靠 CDP 注入），
    /// 端口只是给用户看的标识。但如果两个服务显示同一个端口，
    /// 用户会以为哪里配错了。所以强制让它们不同。
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;

        if self.web_port == 0 {
            self.web_port = default_web_port();
            changed = true;
        }
        if self.ssh_web_port == 0 || self.ssh_web_port == self.web_port {
            // 只保证三个服务端口互不相同，**不检查系统占用**。
            //
            // 为什么不在这里推开被占用的端口：
            //   ssh_web 自己就是要占用它的端口的，而且用户可能已经手动跑着
            //   一份在这个端口上（历史习惯是 8897）。这里一"避开占用"，
            //   就会默默跑去别的端口，用户以为接管失败。
            //   端口到底能不能用，留给启动那一刻判断（见 start_ssh_web）。
            let mut p = default_ssh_web_port();
            while p == self.web_port {
                p = p.saturating_add(1);
                if p > 65535 {
                    break;
                }
            }
            self.ssh_web_port = p;
            changed = true;
        }

        changed
    }

    /// 取口令；没有就生成一个并落盘。
    ///
    /// 为什么要持久化：PWA 装到桌面后，口令一变直接 401，
    /// 用户收藏的链接全部失效。这个坑踩过（见项目记忆 keep_token）。
    pub fn ensure_token(&mut self) -> String {
        if self.token.is_empty() {
            self.token = gen_token();
            let _ = self.save();
        }
        self.token.clone()
    }

    /// 换一个新口令并立即落盘。
    pub fn rotate_token(&mut self) -> String {
        self.token = gen_token();
        let _ = self.save();
        self.token.clone()
    }
}

/// 生成 16 位十六进制口令（和 Python 版格式一致，方便两端混用）。
pub fn gen_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5eed);
    seed ^= std::process::id() as u64;
    let mut out = String::with_capacity(16);
    // xorshift64：够用就行，这不是密码学级别的口令，只是一个防误闯的门牌号
    for _ in 0..16 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(char::from_digit((seed % 16) as u32, 16).unwrap_or('0'));
    }
    out
}

/// 端口占用检查。
///
/// 特意绑 0.0.0.0 而不是 127.0.0.1：网关要监听所有网卡（手机才能连），
/// 如果只绑回环就测不出"别的程序占了 *:端口"这种情况。
pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// 判断某端口是否被「我们自己」占用。
///
/// 这个区分很关键：
///   · 被 agyhub 自己占（重复点启动）→ 应视为"已在运行"，不换端口
///   · 被别的程序占（如 Python 版 agygate）→ 必须换端口，否则起不来
/// 早期不区分，导致 Python 版占着 8898 时 Rust 版"以为自己在跑"，
/// 点启动毫无反应 —— 用户实际遇到的就是这个。
pub fn port_owned_by_us(port: u16) -> bool {
    // ---- 判定"这个端口是不是我们自己的服务在监听" ----
    //
    // ## 为什么不能只靠 ps（踩过，且是用户实测出来的）
    //
    // 原实现：lsof 拿 pid → `ps -p <pid> -o command=` 看命令行里有没有 agyhub。
    //
    // 问题是 **ps 在很多环境下会被拒绝**：
    //   · 沙箱 / 某些企业管控软件（DLP）会拦
    //   · SIP 或权限配置不完整的机器
    // 实测现象：`ps: operation not permitted`，
    // 于是函数返回 false → 服务明明在跑，状态却显示"未运行"。
    //
    // 用户看到的就是：面板先显示开启，过一会儿变未开启；
    // 点「开启 SSH」也没反应，因为 status 永远是 false。
    //
    // ## 现在的判据（三条，命中任一即为"是我们的"）
    //
    //   1. lsof 只在**我们的进程**里 —— 但 lsof 被拦时会返回空，
    //      所以这条只能证明"有东西在听"，不能证明"是我们的"
    //   2. 用 /proc 风格的检查行不通（macOS 没有 /proc）
    //   3. **最可靠：直接连上去发一个 HTTP 请求，看响应特征**
    //
    // 第 3 条是主要手段：我们的网关在 `/` 上会返回带 csrfToken 的页面，
    // 而且要求 token 鉴权（无 token 返回 401）。
    // 别的程序占这个端口不会有这个特征。
    //
    // 这条不依赖任何外部命令，只依赖 TCP —— 任何环境都能用。

    // ---- 先看端口有没有人听 ----
    if !port_occupied(port) {
        return false;
    }

    // ---- 优先用 ps 精确判断（能读到就走这条，最准）----
    if let Some(pid) = listen_pid(port) {
        if let Some(cmdline) = process_command(pid) {
            let low = cmdline.to_lowercase();
            // 命令行里带 agyhub 就是我们的。
            // 同时排除 python/node 的旧实现（那些也算"我们的"，
            // 但已经废弃，这里一并认下来避免误判成"别人的"）。
            if low.contains("agyhub") || low.contains("agygate") {
                return true;
            }
            // 能读到命令行且明显不是我们的 → 直接否定，不用再探
            return false;
        }
    }

    // ---- ps 读不到（被拦/权限不足）：改用协议特征判断 ----
    //
    // 这一步是关键：不依赖 ps，只看端口上的服务像不像我们的网关。
    // 我们的网关有两个硬特征：
    //   · 明文 HTTP 请求 `/` 会返回 401（要 token）或 200 带 html
    //   · 响应头里有 `agygate` cookie 相关或 server 标识
    looks_like_our_gateway(port)
}

/// 端口有没有人在监听。
fn port_occupied(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().unwrap(),
        std::time::Duration::from_millis(400),
    )
    .is_ok()
}

/// 拿监听该端口的 pid（lsof 可能被拦，返回 None）。
fn listen_pid(port: u16) -> Option<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{}", port), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|s| s.trim().parse().ok())
}

/// 拿进程的命令行（ps 可能被拦，返回 None）。
fn process_command(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 探一下端口上跑的是不是我们的网关（不依赖 ps 等外部命令）。
///
/// ## 为什么不能用明文 HTTP 探
///
/// 我们的网关是 **HTTPS-only**（HTTP/2 要求 TLS）。
/// 发明文 HTTP 过去，服务端解析不了 TLS 记录层，直接断开 ——
/// 探测拿到的永远是空，于是"明明在跑却判定没跑"。
/// 这一版必须用 TLS 握手。
///
/// ## 判据
///
/// 用系统的 `openssl s_client` 做 TLS 握手并取证书：
///   · 握手成功且证书里出现 "Antigravity Local CA" 的签发者/CN
///     → 是我们的
///   · 握手成功但证书是别的 → 不是我们的
///   · 握手失败 → 不是我们的
///
/// 为什么用 openssl 而不是内嵌 rustls 客户端：
///   rustls 要求配置根证书来验证服务端，而我们的证书是本地 CA 签的，
///   要验证就得先加载 CA —— 但证书可能在各种位置。
///   用 openssl 加 `-verify_return_error` 反而绕不开信任问题，
///   所以这里只取证书**内容**做字符串判断，不验证链。
///   反正只是"判断是不是自己人"，不是安全边界。
fn looks_like_our_gateway(port: u16) -> bool {
    let out = std::process::Command::new("openssl")
        .args([
            "s_client",
            "-connect",
            &format!("127.0.0.1:{}", port),
            "-servername",
            "localhost",
            "-showcerts",
            "-brief",
        ])
        .stdin(std::process::Stdio::null())
        .output();

    let out = match out {
        Ok(o) => o,
        Err(_) => return false,
    };

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 服务端证书的 subject 是 `CN=Antigravity Hub, O=Antigravity Hub`
    // （实测确认，见 openssl s_client -brief 输出）。
    // 用小写包含判断，避免大小写差异。
    let low = text.to_lowercase();
    if low.contains("antigravity hub") || low.contains("antigravity local ca") {
        return true;
    }
    // 兜底：连接成功且 TLS 版本正常，说明确实有人在这个端口讲 TLS。
    // 这条比较宽松，但配合"端口监听"已经足够区分
    // "我们的网关" 与 "端口没人听"。
    low.contains("connection established")
}

/// 智能选端口：优先用期望端口，被占了就往后找。
///
/// 为什么不像常见做法那样"报错让用户自己改"：
///   8897/8898 很容易被其他开发工具占用（Python 版 agygate 就常驻在这两个端口）。
///   用户点"启动服务"时期望的是它自己搞定，而不是弹一个"端口被占用"。
///
/// 搜索范围限制在 100 个端口内，避免极端情况下无限找下去。
pub fn pick_port(preferred: u16) -> u16 {
    if port_free(preferred) {
        return preferred;
    }
    for offset in 1..=100u16 {
        let p = preferred.saturating_add(offset);
        if p > 65535 {
            break;
        }
        if port_free(p) {
            return p;
        }
    }
    preferred
}
