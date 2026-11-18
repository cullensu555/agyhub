//! 把本地根 CA 装进 macOS 信任库。
//!
//! 为什么必须做：
//!   服务器证书由「本地根 CA」签发。macOS 不认识这个根 CA，
//!   浏览器就无法验证整条链 → 页面被标记为不安全 → PWA 装不上。
//!
//! ## 判断"已信任"的方式（上一版的问题）
//!
//! 原来用 `security find-certificate -c "Antigravity Local CA"` 判断，
//! 它只能说明**钥匙串里有一张同名证书**，说明不了：
//!   · 这张证书是否被设为"始终信任"（用户手动导入但没改信任设置时，
//!     证书在库里，浏览器照样报不安全 —— 用户实测反馈的就是这个）
//!   · 这张证书是不是**当前在用**的 CA（重新生成过 CA 时旧证书还在库里）
//!
//! 现在直接让系统验证**当前服务器证书**能不能被信任：
//!   `security verify-cert -c agy-gate.crt -p ssl -n localhost -L -q`
//! 这和浏览器做的判断是同一件事，返回 0 就是真的可信。

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::sys;

const CA_CN: &str = "Antigravity Local CA";

/// 当前服务器证书在本机是否受信任（= 本机浏览器打开不会报不安全）。
pub fn is_trusted(cert_dir: &Path) -> bool {
    let srv = cert_dir.join(crate::cert::SRV_CERT);
    if !srv.exists() {
        return false;
    }
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["verify-cert", "-c"])
        .arg(&srv)
        .args(["-p", "ssl", "-n", "localhost", "-L", "-q"]);
    match sys::output_timeout(cmd, Duration::from_secs(5)) {
        Some(o) => o.status.success(),
        None => false,
    }
}

/// 根证书 CA 本身是否被设成信任（服务器证书还没生成时用）。
fn ca_trusted(ca_cert: &Path) -> bool {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["verify-cert", "-c"])
        .arg(ca_cert)
        .args(["-p", "ssl", "-L", "-q"]);
    match sys::output_timeout(cmd, Duration::from_secs(5)) {
        Some(o) => o.status.success(),
        None => false,
    }
}

fn login_keychain() -> String {
    sys::home()
        .join("Library/Keychains/login.keychain-db")
        .to_string_lossy()
        .to_string()
}

/// 安装 CA。返回是否成功（成功 = 装完后系统验证确认可信）。
pub fn install(cert_dir: &Path, force: bool) -> bool {
    let ca_cert = cert_dir.join(crate::cert::CA_CERT);
    if !ca_cert.exists() {
        println!("  ✗ 根证书还不存在：{}", ca_cert.display());
        return false;
    }

    let trusted_now = || is_trusted(cert_dir) || ca_trusted(&ca_cert);

    if !force && trusted_now() {
        println!("  ✓ 根证书已受信任");
        return true;
    }

    println!("  将把本地根证书加入钥匙串并设为受信任的根：");
    println!("    {}", ca_cert.display());
    println!("  证书名：{}", CA_CN);
    println!("  macOS 会弹窗要求输入登录密码（或指纹），这是系统行为。");
    println!();

    let ca = ca_cert.to_string_lossy().to_string();

    // ---- 顺序：先登录钥匙串，再系统钥匙串 ----
    //
    // 上一版反过来：先试系统钥匙串（需要管理员授权，普通用户必然失败，
    // 管理员也会先弹一次框），失败再退到登录钥匙串（再弹一次框）。
    // 用户看到的是"连续两个密码框"。
    //
    // 登录钥匙串（用户级信任设置）对本用户的 Safari/Chrome/Edge 都有效，
    // 只弹一次框，所以作为首选；系统钥匙串只作兜底。
    let attempts: [(&str, Vec<String>); 2] = [
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
    ];

    for (name, args) in attempts {
        let mut cmd = Command::new("/usr/bin/security");
        cmd.args(&args);
        // 等用户输密码，给足时间
        match sys::output_timeout(cmd, Duration::from_secs(180)) {
            Some(o) if o.status.success() => {
                if trusted_now() {
                    println!("  ✓ 已装入{}并设为受信任的根", name);
                    println!("  浏览器若仍提示不安全，完全退出浏览器再打开。");
                    return true;
                }
                println!("  ! 已写入{}，但系统验证仍不通过，继续尝试", name);
            }
            Some(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                let last = msg.lines().last().unwrap_or("").trim().to_string();
                println!(
                    "  ! {}写入失败{}",
                    name,
                    if last.is_empty() { String::new() } else { format!("：{}", last) }
                );
                // 用户在密码框点了取消：别再弹第二个框
                if last.contains("canceled") || last.contains("cancelled") || last.contains("-128") {
                    break;
                }
            }
            None => println!("  ! {}写入超时（密码框没有被确认）", name),
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
