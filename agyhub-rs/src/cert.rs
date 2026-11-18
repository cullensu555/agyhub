//! 本地根 CA + 服务器证书。
//!
//! 为什么需要：浏览器要求 HTTPS 才肯用 HTTP/2，而 HTTP/2 是必须的——
//! Antigravity 前端会同时开 6 个永不结束的流式订阅，HTTP/1.1 下把
//! 浏览器的 6 个连接槽位占死，后续请求全部超时（表现为页面能看、
//! 一操作就卡）。
//!
//! 自签单证书（issuer == subject）在任何浏览器里都不被信任，页面不算
//! 「安全上下文」，PWA 装不了、Service Worker 也注册不了。所以用
//! 「本地根 CA 签发服务器证书」：根证书装进信任库一次，长期有效。
//!
//! 判据：`security find-certificate -c "Antigravity Local CA" ~/Library/Keychains/login.keychain-db`
//! 有输出 = 已装。

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub const CA_CERT: &str = "agy-ca.crt";
pub const CA_KEY: &str = "agy-ca.key";
pub const SRV_CERT: &str = "agy-gate.crt";
pub const SRV_KEY: &str = "agy-gate.key";

/// 证书与私钥的 PEM 内容。
pub struct CertBundle {
    pub cert_pem: String,
    pub key_pem: String,
    /// 根证书 PEM，供客户端下载安装。
    pub ca_pem: String,
}

/// 收集本机所有可用于访问的 IP。
///
/// 手机走局域网访问时用的是 en0 的地址，证书 SAN 里必须包含它，
/// 否则手机会报证书不匹配。这里同时收录 127.0.0.1 与所有非回环 IPv4。
pub fn local_ips() -> Vec<IpAddr> {
    let mut out = vec![IpAddr::from([127, 0, 0, 1])];

    // 用 getifaddrs 枚举网卡地址，不依赖外部命令。
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() {
                let sa = ifa.ifa_addr;
                if (*sa).sa_family as i32 == libc::AF_INET {
                    let sin = sa as *const libc::sockaddr_in;
                    let addr = (*sin).sin_addr.s_addr;
                    // s_addr 是网络字节序，转成主机序再拆字节
                    let host = u32::from_be(addr);
                    let ip = IpAddr::from([
                        ((host >> 24) & 0xff) as u8,
                        ((host >> 16) & 0xff) as u8,
                        ((host >> 8) & 0xff) as u8,
                        (host & 0xff) as u8,
                    ]);
                    if !ip.is_loopback() && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }

    out
}

/// 读取或生成证书。
///
/// **优先复用 Python 版（agygate）已有的证书目录。**
///
/// 为什么这条很重要：那份根 CA 早就装进了系统与登录钥匙串，手机上也装过。
/// 如果这里另起炉灶生成新 CA，用户的浏览器会立刻报「不受信任」，
/// 手机上已装的根证书也全部失效 —— 等于逼用户把所有设备重装一遍，
/// 只因为它们看不到的 CA 换了一张。
///
/// 所以顺序是：
///   1. 环境变量 AGEHUB_CERT_DIR 指定的目录（调试用）
///   2. `~/.agyhub/certs`（本程序自己的）
///   3. 项目内 `.agy-certs`（Python 版在用，**大概率已受信任**）
pub fn ensure_certs(dir: &Path) -> io::Result<CertBundle> {
    // 若自己的目录里还没有 CA，先看 Python 版的能不能直接拿来用
    let ca_cert_p = dir.join(CA_CERT);
    let ca_key_p = dir.join(CA_KEY);
    if !(ca_cert_p.exists() && ca_key_p.exists()) {
        if let Some(legacy) = find_legacy_certs() {
            tracing::info!("复用已有根 CA（来自 {}）", legacy.display());
            fs::create_dir_all(dir)?;
            // 复制而不是引用：将来 Python 版被删掉也不影响这里。
            // 只复制 CA，服务器证书按本机当前 IP 重新签。
            fs::copy(legacy.join(CA_CERT), &ca_cert_p)?;
            fs::copy(legacy.join(CA_KEY), &ca_key_p)?;
        }
    }

    fs::create_dir_all(dir)?;

    let srv_cert_p = dir.join(SRV_CERT);
    let srv_key_p = dir.join(SRV_KEY);

    // 根 CA：没有就生成，有就复用
    let (ca_cert_pem, ca_key_pem) = if ca_cert_p.exists() && ca_key_p.exists() {
        (
            fs::read_to_string(&ca_cert_p)?,
            fs::read_to_string(&ca_key_p)?,
        )
    } else {
        let (c, k) = generate_ca()?;
        write_secure(&ca_cert_p, &c)?;
        write_secure(&ca_key_p, &k)?;
        (c, k)
    };

    // 服务器证书：没有就签发一张
    //
    // 注意：如果本机 IP 变了（换 WiFi），旧证书的 SAN 就对不上，
    // 手机会报证书错误。所以这里检查 SAN 是否覆盖当前所有 IP，
    // 不覆盖就重新签发。
    let need_reissue = if srv_cert_p.exists() && srv_key_p.exists() {
        let existing = fs::read_to_string(&srv_cert_p)?;
        !san_covers_local_ips(&existing)
    } else {
        true
    };

    let (srv_cert_pem, srv_key_pem) = if need_reissue {
        let (c, k) = sign_server_cert(&ca_cert_pem, &ca_key_pem)?;
        write_secure(&srv_cert_p, &c)?;
        write_secure(&srv_key_p, &k)?;
        (c, k)
    } else {
        (
            fs::read_to_string(&srv_cert_p)?,
            fs::read_to_string(&srv_key_p)?,
        )
    };

    Ok(CertBundle {
        cert_pem: srv_cert_pem,
        key_pem: srv_key_pem,
        ca_pem: ca_cert_pem,
    })
}

/// 找 Python 版（agygate）留下的证书目录。
///
/// 判据是「有 CA 证书 + 有 CA 私钥」。搜几个可能的位置：
///   1. 环境变量 AGEHUB_LEGACY_CERTS（显式指定，用于特殊场景）
///   2. 本程序可执行文件所在目录往上找 `.agy-certs`
///   3. 当前工作目录往上找 `.agy-certs`
///
/// ⚠️ **不能写死任何绝对路径**。
/// 早先这里有一条 `<开发机上的个人目录>`，
/// 那是开发机上的真实位置 —— 分发给别人时那个目录不存在，
/// 更重要的是它泄露了作者的个人目录结构。已去掉。
///
/// 复用旧 CA 的意义：换了 CA 等于逼用户把所有设备重装一次证书。
/// 但复用只在"能找到"时发生，找不到就正常生成新的，不影响可用性。
fn find_legacy_certs() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(p) = std::env::var("AGEHUB_LEGACY_CERTS") {
        candidates.push(PathBuf::from(p));
    }

    // 从可执行文件位置往上找（target/debug/agyhub → 项目根）
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.as_path();
        for _ in 0..5 {
            if let Some(parent) = p.parent() {
                candidates.push(parent.join(".agy-certs"));
                p = parent;
            } else {
                break;
            }
        }
    }

    // 当前工作目录往上找
    if let Ok(cwd) = std::env::current_dir() {
        let mut p = cwd.as_path();
        for _ in 0..4 {
            candidates.push(p.join(".agy-certs"));
            match p.parent() {
                Some(parent) => p = parent,
                None => break,
            }
        }
    }

    for c in candidates {
        if c.join(CA_CERT).exists() && c.join(CA_KEY).exists() {
            return Some(c);
        }
    }
    None
}

/// 生成根 CA。
fn generate_ca() -> io::Result<(String, String)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Antigravity Local CA");
    dn.push(DnType::OrganizationName, "Antigravity Hub");
    params.distinguished_name = dn;

    let key = KeyPair::generate().map_err(to_io)?;
    let cert = params.self_signed(&key).map_err(to_io)?;

    Ok((cert.pem(), key.serialize_pem()))
}

/// 用根 CA 签发服务器证书。
fn sign_server_cert(ca_cert_pem: &str, ca_key_pem: &str) -> io::Result<(String, String)> {
    let ca_key = KeyPair::from_pem(ca_key_pem).map_err(to_io)?;
    let ca_params = CertificateParams::from_ca_cert_pem(ca_cert_pem).map_err(to_io)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(to_io)?;

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Antigravity Hub");
    dn.push(DnType::OrganizationName, "Antigravity Hub");
    params.distinguished_name = dn;

    // SAN 必须覆盖所有访问方式：
    //   - 127.0.0.1 / localhost：本机浏览器
    //   - 局域网 IP：手机访问
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().map_err(to_io)?),
        SanType::IpAddress(IpAddr::from([127, 0, 0, 1])),
    ];
    for ip in local_ips() {
        if !ip.is_loopback() {
            params.subject_alt_names.push(SanType::IpAddress(ip));
        }
    }

    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    let key = KeyPair::generate().map_err(to_io)?;
    let cert = params.signed_by(&key, &ca_cert, &ca_key).map_err(to_io)?;

    Ok((cert.pem(), key.serialize_pem()))
}

/// 检查已有服务器证书的 SAN 是否覆盖当前所有本机 IP。
///
/// 这里只做粗略的文本匹配：把 IP 字面量在 PEM 解码后的 der 里找。
/// 更严谨的做法是解析 X.509，但为了少一个依赖，够用。
fn san_covers_local_ips(cert_pem: &str) -> bool {
    // 把 PEM 里的 base64 解开成 der，在字节里找 IP
    let der = match pem_to_der(cert_pem) {
        Some(d) => d,
        None => return false,
    };

    for ip in local_ips() {
        if ip.is_loopback() {
            continue;
        }
        let octets = match ip {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        if !der.windows(octets.len()).any(|w| w == octets.as_slice()) {
            return false;
        }
    }
    true
}

/// PEM -> DER（只取第一个 CERTIFICATE 块）。
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN CERTIFICATE-----")?;
    let rest = &pem[start..];
    let body_start = rest.find('\n')? + 1;
    let end = rest.find("-----END CERTIFICATE-----")?;
    let b64: String = rest[body_start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64_decode(&b64)
}

/// 最小 base64 解码，避免为一个功能引入新依赖。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    let _ = T;
    Some(out)
}

/// 写文件并收紧权限（私钥不能给别人读）。
fn write_secure(path: &PathBuf, data: &str) -> io::Result<()> {
    fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(0o600);
        fs::set_permissions(path, perm)?;
    }
    Ok(())
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// 把证书/私钥 PEM 装成 rustls 的 TlsAcceptor。
///
/// 只启用 h2 与 http/1.1 两个 ALPN 协议，且 **h2 排在前面**：
/// 浏览器在 ALPN 里挑第一个自己能用的，h2 优先才能拿到多路复用。
/// 若把 http/1.1 放前面，浏览器就用 HTTP/1.1 —— 6 个流式订阅会占满
/// 连接槽位，页面能看但一操作就卡。
pub fn tls_acceptor(bundle: &CertBundle) -> io::Result<tokio_rustls::TlsAcceptor> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::ServerConfig;
    use std::io::BufReader;

    // rustls-pemfile 2.x 的 API：返回迭代器
    let mut cert_reader = BufReader::new(bundle.cert_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(to_io)?;
    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "证书为空"));
    }

    let mut key_reader = BufReader::new(bundle.key_pem.as_bytes());
    let key: PrivateKeyDer<'static> = match rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .next()
    {
        Some(Ok(k)) => PrivateKeyDer::Pkcs8(k),
        _ => {
            // rcgen 输出的是 PKCS#8，但万一是别的格式也能兜一下
            let mut r2 = BufReader::new(bundle.key_pem.as_bytes());
            let mut k = None;
            for item in rustls_pemfile::read_all(&mut r2) {
                if let Ok(rustls_pemfile::Item::Pkcs1Key(p)) = item {
                    k = Some(PrivateKeyDer::Pkcs1(p));
                    break;
                }
            }
            k.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "私钥解析失败"))?
        }
    };

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(to_io)?;

    // ALPN：h2 必须在最前
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(cfg)))
}
