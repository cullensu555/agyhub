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
            web_enabled: true,
            ssh_enabled: true,
            ssh_web_enabled: true,
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
    let out = match std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{}", port), "-sTCP:LISTEN"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let pid: u32 = match String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|s| s.trim().parse().ok())
    {
        Some(p) => p,
        None => return false,
    };
    // 看该进程的可执行文件路径里是否含 agyhub
    let cmd = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    match cmd {
        Ok(o) => {
            let line = String::from_utf8_lossy(&o.stdout).to_lowercase();
            line.contains("agyhub")
        }
        Err(_) => false,
    }
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
