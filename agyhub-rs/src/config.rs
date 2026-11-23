// 配置持久化：口令、端口、各服务的开关状态。
//
// 为什么放在 Rust 而不是 Swift：
//   内核对这两个值有独占的读需求（服务启动要用端口和口令），
//   Swift 只是显示。放在 Rust 里只有一份真相，
//   避免"Swift 改了但内核不认"这种撕裂。
//
// 存储位置：~/.agyhub/config.json（权限 0600：里面有访问口令）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::sys;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 网页版端口
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    // ⚠️ 这里**故意没有** ssh_port 字段。
    //
    // 「启动 SSH」指的是让**电脑桌面版** Antigravity 的界面里出现
    // 「Connect to SSH Host…」入口，实现方式是 CDP 注入，
    // 它**不监听任何端口**。需要端口的只有 ssh_web。
    /// 访问口令。空字符串表示"未设置"，首次写盘时生成
    #[serde(default)]
    pub token: String,
    /// 网页版是否启用
    #[serde(default = "default_true")]
    pub web_enabled: bool,
    /// SSH 入口解锁是否启用。
    ///
    /// ⚠️ serde 缺省值必须与 `Default` 一致（都是 false）。
    /// 上一版只改了 `Default`，这里还是 `default_true`：
    /// 老配置文件里缺这个字段时，反序列化出来仍是"默认开启"。
    #[serde(default)]
    pub ssh_enabled: bool,
    /// SSH 网页版是否启用（同上，缺省 false）
    #[serde(default)]
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
    /// 网页版是否显示「上下文与用量」指示器。
    ///
    /// 官方有对应的功能开关（`enable-context-usage-indicator`），但**网页版根本
    /// 没打包那个组件** —— 宿主 `getAppVariant` 里写死 `contextUsageIndicator: void 0`，
    /// 写了 flag 也不会渲染。数据倒是完整推给前端了，所以这里自己画一个。
    ///
    /// 缺省 **true**：它只读不写、不改页面行为，默认开着没副作用。
    /// ⚠️ serde 的 default 必须与 `Default` 一致，否则老配置反序列化会和首次安装不同。
    #[serde(default = "default_true")]
    pub usage_enabled: bool,
    /// 配置格式版本（见 `migrate`）。老配置文件里没有这个字段 = 0。
    #[serde(default)]
    pub schema: u32,
}

const SCHEMA: u32 = 2;

pub const DEFAULT_WEB_PORT: u16 = 8898;
pub const DEFAULT_SSH_WEB_PORT: u16 = 8897;

fn default_web_port() -> u16 {
    DEFAULT_WEB_PORT
}
fn default_true() -> bool {
    true
}
fn default_ssh_web_port() -> u16 {
    DEFAULT_SSH_WEB_PORT
}

impl Default for Config {
    fn default() -> Self {
        Self {
            web_port: default_web_port(),
            token: String::new(),
            // ---- 首次安装的默认状态 ----
            //
            // 只默认开「本机网页版」—— 这是 App 的核心用途。
            // SSH 相关的两个默认关闭：
            //   · ssh_enabled 会改动 Antigravity 页面（注入 + 刷新一次），
            //     刚装完 App 的人不需要它
            //   · ssh_web_enabled 需要先配好 ~/.ssh/config，没配过的人用不了
            web_enabled: true,
            ssh_enabled: false,
            ssh_web_enabled: false,
            ssh_web_port: default_ssh_web_port(),
            ssh_target: String::new(),
            zh_enabled: false,
            usage_enabled: true,
            schema: SCHEMA,
        }
    }
}

fn config_path() -> PathBuf {
    sys::data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let p = config_path();
        let s = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        let mut c = match serde_json::from_str::<Config>(&s) {
            Ok(c) => c,
            Err(e) => {
                // ---- 配置文件损坏 ----
                //
                // 以前是静默 `unwrap_or_default()`：口令跟着丢了，下次写盘生成新口令，
                // 用户手机上收藏的链接、装好的 PWA 全部 401，且完全不知道为什么。
                //
                // 现在：备份坏文件（只备份一次），尽量把口令捞回来，
                // 并立即写回一份修好的配置 —— 否则网关每秒重读口令时会反复走到这里。
                eprintln!("配置文件解析失败（{}），已备份并使用默认值", e);
                let bak = p.with_extension("json.broken");
                if !bak.exists() {
                    let _ = std::fs::copy(&p, &bak);
                }
                // 口令和端口都是手机上收藏的链接的一部分，尽量都找回来
                let mut c = Self::default();
                c.token = salvage_str(&s, "token")
                    .filter(|t| t.len() >= 8 && t.chars().all(|ch| ch.is_ascii_hexdigit()))
                    .unwrap_or_default();
                if let Some(p) = salvage_num(&s, "web_port") {
                    c.web_port = p;
                }
                if let Some(p) = salvage_num(&s, "ssh_web_port") {
                    c.ssh_web_port = p;
                }
                if let Some(t) = salvage_str(&s, "ssh_target") {
                    c.ssh_target = t;
                }
                c.normalize();
                c.schema = SCHEMA;
                if !c.token.is_empty() {
                    let _ = c.save_inner();
                }
                return c;
            }
        };
        c.migrate();
        c
    }

    /// 老配置升级。
    ///
    /// schema < 2（1.1.0 及以前）：`ssh_enabled` 不可信。
    /// 1.0.0 默认把它设成 true 并写进了配置，而那时的 SSH 钩子有语法错误、
    /// 从来没生效过 —— 用户其实没有"在用"这个功能。
    /// 直接沿用的话，升级后第一次启动就会不经确认刷新 Antigravity 的所有窗口。
    /// 所以升级时把它关掉，用户需要时在菜单里打开（会先弹确认）。
    fn migrate(&mut self) {
        if self.schema < 2 {
            self.ssh_enabled = false;
        }
        self.schema = SCHEMA;
    }

    pub fn save(&self) -> std::io::Result<()> {
        // ⚠️ 保存前必须保证 token 非空。
        //
        // status 子命令故意"不落盘"，读取时 token 为空会用临时值显示。
        // 写盘时绝不能写空 token，否则用户口令凭空消失。
        let mut me = self.clone();
        if me.token.is_empty() {
            me.token = gen_token();
        }
        me.save_inner()
    }

    fn save_inner(&self) -> std::io::Result<()> {
        let p = config_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
            sys::chmod_700(parent);
        }
        let s = serde_json::to_string_pretty(self)?;
        // 先写临时文件再 rename：避免写到一半断电/被杀导致配置损坏。
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, s)?;
        // 口令在这个文件里，只给本人读
        sys::chmod_600(&tmp);
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// 某个服务配置里记录的端口。
    pub fn port_of(&self, kind: crate::state::Kind) -> u16 {
        match kind {
            crate::state::Kind::Web => self.web_port,
            crate::state::Kind::SshWeb => self.ssh_web_port,
        }
    }

    /// 校正端口：确保两个服务端口不同、且不为 0。
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;

        if self.web_port == 0 {
            self.web_port = default_web_port();
            changed = true;
        }
        if self.ssh_web_port == 0 || self.ssh_web_port == self.web_port {
            // 只保证端口互不相同，**不检查系统占用**：
            // 端口到底能不能用，留给启动那一刻判断。
            let mut p = default_ssh_web_port() as u32;
            while p == self.web_port as u32 && p < 65535 {
                p += 1;
            }
            self.ssh_web_port = p as u16;
            changed = true;
        }

        changed
    }

    /// 取口令；没有就生成一个并落盘。
    ///
    /// 为什么要持久化：PWA 装到桌面后，口令一变直接 401，
    /// 用户收藏的链接全部失效。
    pub fn ensure_token(&mut self) -> String {
        if self.token.is_empty() {
            self.token = gen_token();
            let _ = self.save();
        }
        self.token.clone()
    }

    /// 换一个新口令并立即落盘。
    ///
    /// 正在运行的网关每秒会重读一次配置里的口令（见 server::TokenSource），
    /// 所以换完立刻生效，不需要重启服务。
    pub fn rotate_token(&mut self) -> String {
        self.token = gen_token();
        let _ = self.save();
        self.token.clone()
    }
}

/// 损坏的配置文本里 `"key":` 后面的内容（文件被截断时 JSON 解析会整体失败）。
fn salvage_after<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\"", key);
    let i = s.find(&pat)?;
    s[i + pat.len()..].trim_start().strip_prefix(':').map(|r| r.trim_start())
}

/// 找回字符串字段。必须完整（有结尾引号）：截断到一半的值不能用。
fn salvage_str(s: &str, key: &str) -> Option<String> {
    let rest = salvage_after(s, key)?.strip_prefix('"')?;
    let end = rest.find('"')?;
    let v = &rest[..end];
    if v.contains('\\') || v.is_empty() {
        return None;
    }
    Some(v.to_string())
}

/// 找回数字字段。必须完整（后面跟着 , 或 } 或换行）。
fn salvage_num(s: &str, key: &str) -> Option<u16> {
    let rest = salvage_after(s, key)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let after = rest[digits.len()..].trim_start();
    if !(after.starts_with(',') || after.starts_with('}')) {
        return None;
    }
    digits.parse().ok().filter(|p| *p >= 1024)
}

/// 生成 16 位十六进制口令（64 bit 系统随机数）。
pub fn gen_token() -> String {
    sys::random_hex(8)
}

/// 端口是否空闲。
///
/// 两步都要过：
///   1. 能绑 0.0.0.0（网关要监听所有网卡）
///   2. 127.0.0.1 上没人在听 —— macOS 上别的程序只绑了 127.0.0.1:port 时，
///      带 SO_REUSEADDR 的 0.0.0.0 绑定仍会成功，但本机访问会被那个程序截走。
pub fn port_free(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() && !sys::tcp_listening(port)
}

/// 智能选端口：优先用期望端口，被占了就往后找（最多 100 个），并避开 `avoid`。
pub fn pick_port(preferred: u16, avoid: u16) -> u16 {
    let base = if preferred == 0 { DEFAULT_WEB_PORT } else { preferred } as u32;
    for offset in 0..=100u32 {
        let p = base + offset;
        if p > 65535 {
            break;
        }
        let p = p as u16;
        if p != avoid && port_free(p) {
            return p;
        }
    }
    preferred
}
