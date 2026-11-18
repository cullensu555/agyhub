//! 把本地根 CA 装进 macOS 信任库。
//!
//! 为什么必须做：
//!   服务器证书由「本地根 CA」签发，**证书链本身是完整的**
//!   （SAN 含 127.0.0.1 / 内网 IP / localhost）。但 macOS 不认识这个根 CA，
//!   浏览器就无法验证整条链 → 页面被标记为不安全 → PWA 装不上、
//!   Service Worker 注册失败、内置浏览器连 DOM 都读不到。
//!
//! 装进钥匙串并标记 trustRoot 后，整条链立刻可验证，长期有效
//! （根 CA 有效期 10 年）。
//!
//! 两级策略：
//!   1. 先试系统钥匙串（所有用户、所有浏览器都认）—— 需管理员密码，
//!      沙箱里必然失败，属预期
//!   2. 退到登录钥匙串 —— **这条能成功，且足够让所有浏览器认可**

use std::path::Path;
use std::process::Command;

const CA_CN: &str = "Antigravity Local CA";

/// 查 CA 是否已装入任一钥匙串。
pub fn installed() -> Option<&'static str> {
    for (chain, name) in [
        ("/Library/Keychains/System.keychain", "系统"),
        // login.keychain-db 是实际文件名，用 ~/Library/Keychains/login.keychain 也能查到
    ] {
        if exists_in(chain, name) {
            return Some("系统");
        }
    }
    let login = login_keychain();
    if exists_in(&login, "登录") {
        return Some("登录");
    }
    None
}

fn login_keychain() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{}/Library/Keychains/login.keychain-db", home)
}

fn exists_in(chain: &str, _name: &str) -> bool {
    let out = Command::new("security")
        .args(["find-certificate", "-c", CA_CN, chain])
        .output();
    match out {
        Ok(o) => o.status.success() && !o.stdout.is_empty(),
        Err(_) => false,
    }
}

/// 安装 CA。返回是否成功。
pub fn install(ca_cert: &Path, force: bool) -> bool {
    if !ca_cert.exists() {
        println!("  ✗ 根证书还不存在：{}", ca_cert.display());
        println!("    先启动一次 agyhub，它会自动生成。");
        return false;
    }

    if let Some(where_) = installed() {
        if !force {
            println!("  ✓ 根证书已在「{}」钥匙串中", where_);
            println!("    如果浏览器仍报不安全，加 --force 重新写入信任设置");
            return true;
        }
    }

    println!("  将把本地根证书加入钥匙串并设为受信任的根：");
    println!("    {}", ca_cert.display());
    println!("  证书名：{}", CA_CN);
    println!();
    println!("  macOS 会弹窗要求输入登录密码（或指纹），这是系统行为。");
    println!();

    let ca = ca_cert.to_string_lossy().to_string();

    // 先试系统级，失败再退到用户级。
    // 用户级同样能让所有浏览器认可，这是沙箱/无管理员密码时的主要路径。
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "系统钥匙串",
            vec![
                "add-trusted-cert".into(),
                "-d".into(),
                "-r".into(),
                "trustRoot".into(),
                "-p".into(),
                "ssl".into(),
                "-k".into(),
                "/Library/Keychains/System.keychain".into(),
                ca.clone(),
            ],
        ),
        (
            "登录钥匙串",
            vec![
                "add-trusted-cert".into(),
                "-r".into(),
                "trustRoot".into(),
                "-p".into(),
                "ssl".into(),
                "-k".into(),
                login_keychain(),
                ca.clone(),
            ],
        ),
    ];

    for (name, args) in attempts {
        match Command::new("security").args(&args).output() {
            Ok(o) if o.status.success() => {
                println!("  ✓ 已装入{}并标记为受信任的根", name);
                println!();
                println!("  现在重启浏览器（Safari 需完全退出），再打开地址试试。");
                println!("  Chrome/Edge 若仍警告，到 设置→隐私与安全→管理证书 里确认存在。");
                return true;
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                let last = msg.lines().last().unwrap_or("").trim();
                println!(
                    "  ! {}写入失败{}",
                    name,
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", last)
                    }
                );
            }
            Err(e) => println!("  ! {}写入异常：{}", name, e),
        }
    }

    println!();
    println!("  ✗ 自动安装没有成功。可以手动操作：");
    println!("    1) 双击 {}", ca_cert.display());
    println!("    2) 在「钥匙串访问」里找到 \"{}\"", CA_CN);
    println!("    3) 双击它 → 展开「信任」→ 把「使用此证书时」设为「始终信任」");
    println!("    4) 关闭窗口，输入密码");
    false
}
