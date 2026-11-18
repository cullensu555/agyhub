//! HTML 注入与内置资源。
//!
//! 注入的目的只有一个：让网页版**看起来像 App**，并且能装成 App。
//!   - 软化移动端输入框缩放（iOS 对 <16px 的 input 会强制放大页面）
//!   - 钉住标题与图标（React 挂载后会改写，需要反复钉）
//!   - 提供 manifest（Chromium 判"能否安装"的硬门槛之一）
//!   - 窄屏给一条安装提示
//!
//! 注入内容的每一段都对应一个踩过的坑，注释写在原处，别随手删。

use std::sync::OnceLock;

// ============================================================
// 内置路由（免口令）
// ============================================================

/// 内置静态资源。
pub struct Asset {
    pub body: Vec<u8>,
    pub content_type: &'static str,
    /// None 表示不给 cache-control 头。
    pub cache: Option<&'static str>,
}

const PATH_CA: &str = "/__agy/ca.crt";
const PATH_MANIFEST: &str = "/__agy/manifest.webmanifest";
const PATH_ICON: &str = "/__agy/icon.png";
const PATH_DIAG: &str = "/__agy/diag";

/// 查内置资源。返回 None 表示不是内置路径，交给反向代理。
///
/// 路径刻意做成**固定集合**，不接受任何拼接 —— 天然免疫目录穿越。
pub fn builtin_asset(clean_path: &str, ca_pem: &str) -> Option<Asset> {
    match clean_path {
        PATH_CA => Some(Asset {
            // 只发公钥证书，私钥永远不出磁盘
            body: ca_pem.as_bytes().to_vec(),
            content_type: "application/x-x509-ca-cert",
            // no-cache：重装 CA 后必须立刻能下到新的
            cache: Some("no-cache"),
        }),

        PATH_MANIFEST => Some(Asset {
            body: manifest_json().into_bytes(),
            content_type: "application/manifest+json; charset=utf-8",
            // manifest 改了要能立刻生效，不用长缓存
            cache: Some("no-cache"),
        }),

        PATH_ICON => icon_png(192).map(|b| Asset {
            body: b,
            content_type: "image/png",
            // no-cache：换图标必须立刻生效。
            // 曾用过 24h 缓存，改完手机上还是旧图，白折腾一轮。
            cache: Some("no-cache"),
        }),

        PATH_DIAG => Some(Asset {
            body: DIAG_HTML.as_bytes().to_vec(),
            content_type: "text/html; charset=utf-8",
            cache: Some("no-store"),
        }),

        // 带尺寸的图标：/__agy/icon-192.png、/__agy/icon-512.png、...-maskable.png
        _ => {
            if let Some((size, _maskable)) = parse_icon_path(clean_path) {
                icon_png(size).map(|b| Asset {
                    // maskable 直接复用原图：App 标志只占中间一部分、四周是白底，
                    // 裁成圆形后露出的仍是白底，不会切到图形本体。
                    // 试过再补一圈背景色，结果变成「白色圆角块 + 米色边框」两层，更脏。
                    body: b,
                    content_type: "image/png",
                    cache: Some("no-cache"),
                })
            } else {
                None
            }
        }
    }
}

fn parse_icon_path(p: &str) -> Option<(u32, bool)> {
    let rest = p.strip_prefix("/__agy/icon-")?;
    let rest = rest.strip_suffix(".png")?;
    let (num, maskable) = match rest.strip_suffix("-maskable") {
        Some(n) => (n, true),
        None => (rest, false),
    };
    let size: u32 = num.parse().ok()?;
    if size == 0 || size > 1024 {
        return None;
    }
    Some((size, maskable))
}

/// 生成 Web App Manifest。
///
/// 必须字段（Chromium 判"能装"的门槛）：
///   name/short_name、start_url、display ∈ {standalone,fullscreen,minimal-ui}、
///   icons 含 192 与 512 两档、prefer_related_applications 不为 true
///
/// start_url 用相对 "./"，这样将来多端口也能各开各的，不串档。
fn manifest_json() -> String {
    let v = icon_version();
    // 图标 URL 带版本号：WebAPK 把图标固化在本地，URL 不变它就认为
    // "图标没变过"，一直显示旧图。带上版本号 Chrome 才会重新抓。
    serde_json::json!({
        "name": "Antigravity",
        "short_name": "Antigravity",
        "description": "Antigravity 远程界面",
        "start_url": "./",
        "scope": "/",
        "display": "standalone",
        "display_override": ["standalone", "minimal-ui"],
        "orientation": "any",
        "background_color": "#FAF9F5",
        "theme_color": "#FAF9F5",
        "icons": [
            {"src": format!("/__agy/icon-192.png?v={}", v), "sizes": "192x192", "type": "image/png", "purpose": "any"},
            {"src": format!("/__agy/icon-512.png?v={}", v), "sizes": "512x512", "type": "image/png", "purpose": "any"},
            {"src": format!("/__agy/icon-512-maskable.png?v={}", v), "sizes": "512x512", "type": "image/png", "purpose": "maskable"},
        ],
        "prefer_related_applications": false,
    })
    .to_string()
}

// ============================================================
// 图标
// ============================================================

/// 图标缓存目录：`~/.agyhub/cache`
fn icon_cache_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".agyhub").join("cache")
}

/// 图标版本串，取缓存目录里最新的 mtime。
///
/// 取 mtime 而不是内容哈希：目录里有多个尺寸，取最大值即可，
/// 避免每次请求都读几个文件算哈希。
fn icon_version() -> u64 {
    let dir = icon_cache_dir();
    let mut latest = 0u64;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("icon-") && name.ends_with(".png") {
                if let Ok(md) = e.metadata() {
                    if let Ok(t) = md.modified() {
                        if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                            latest = latest.max(d.as_secs());
                        }
                    }
                }
            }
        }
    }
    latest
}

/// 取指定尺寸的 PNG 图标（已做白底合成）。
///
/// 流程：缓存命中 → 直接用；否则调 sips 从 App 的 icns 转一张，
/// 再做白底合成后写入缓存。sips 是 macOS 自带的，不需要额外安装。
///
/// 失败返回 None：前端会退化成没有自定义图标，不影响任何功能。
fn icon_png(size: u32) -> Option<Vec<u8>> {
    let dir = icon_cache_dir();
    let cache = dir.join(format!("icon-{}.png", size));

    if let Ok(data) = std::fs::read(&cache) {
        return Some(data);
    }

    let src = "/Applications/Antigravity.app/Contents/Resources/icon.icns";
    if !std::path::Path::new(src).exists() {
        return None;
    }

    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!("icon-{}.raw.png", size));

    let out = std::process::Command::new("sips")
        .args([
            "-s",
            "format",
            "png",
            "-Z",
            &size.to_string(),
            src,
            "--out",
            tmp.to_str()?,
        ])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let raw = std::fs::read(&tmp).ok()?;
    let flat = flatten_on_white(&raw).unwrap_or(raw);
    let _ = std::fs::remove_file(&tmp);

    // 原子写：先写临时文件再 rename，避免并发请求读到半个文件
    let tmp2 = dir.join(format!(".icon-{}.tmp", size));
    std::fs::write(&tmp2, &flat).ok()?;
    std::fs::rename(&tmp2, &cache).ok()?;

    Some(flat)
}

/// 把带透明通道的 PNG 合成到白色实底上，返回不含 alpha 的 PNG。
///
/// 为什么必须做：
///   App 原始 icns 转出的 PNG 四角是**完全透明**（RGBA = 0,0,0,0）。
///   Chrome 在 Android 上画桌面图标时会把透明区域**填成黑色**，
///   于是装出来的 App 图标四周出现一圈黑边（实测现象）。
///   预先合成白底后画面里不再有透明像素，任何背景都不会再透出来。
///
/// 只处理 8bit RGB/RGBA 两种常见格式，其它格式原样返回。
/// 用 flate2 而不是 image crate：只需要解压、逐像素合成、再压缩，
/// 拉一个完整的图像库不值得。
fn flatten_on_white(data: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !data.starts_with(SIG) {
        return None;
    }

    let mut pos = 8;
    let mut idat: Vec<u8> = Vec::new();
    let (mut w, mut h) = (0u32, 0u32);
    let mut bitdepth = 0u8;
    let mut colortype = 0u8;

    while pos + 8 <= data.len() {
        let ln = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let typ = &data[pos + 4..pos + 8];
        let body = data.get(pos + 8..pos + 8 + ln)?;
        match typ {
            b"IHDR" => {
                if body.len() < 13 {
                    return None;
                }
                w = u32::from_be_bytes(body[0..4].try_into().ok()?);
                h = u32::from_be_bytes(body[4..8].try_into().ok()?);
                bitdepth = body[8];
                colortype = body[9];
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + ln; // 长度4 + 类型4 + 数据 + CRC4
    }

    if bitdepth != 8 || (colortype != 6 && colortype != 2) {
        // 非 8bit 或非 RGB/RGBA，不冒险处理，原样返回
        return None;
    }
    let channels = if colortype == 6 { 4 } else { 3 };
    let row_len = w as usize * channels;

    let raw = {
        use flate2::read::ZlibDecoder;
        use std::io::Read;
        let mut d = ZlibDecoder::new(&idat[..]);
        let mut v = Vec::new();
        d.read_to_end(&mut v).ok()?;
        v
    };

    // 反滤波：PNG 每行前面有 1 字节滤波类型
    let mut pixels = vec![0u8; row_len * h as usize];
    let mut prev = vec![0u8; row_len];
    let mut rp = 0usize;
    for y in 0..h as usize {
        let ft = *raw.get(rp)?;
        rp += 1;
        let line = raw.get(rp..rp + row_len)?;
        rp += row_len;
        let cur = &mut pixels[y * row_len..(y + 1) * row_len];
        for x in 0..row_len {
            let a = if x >= channels { cur[x - channels] } else { 0 };
            let b = prev[x];
            let c = if x >= channels { prev[x - channels] } else { 0 };
            let v = line[x];
            cur[x] = match ft {
                0 => v,
                1 => v.wrapping_add(a),
                2 => v.wrapping_add(b),
                3 => v.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => {
                    // Paeth
                    let p = a as i16 + b as i16 - c as i16;
                    let pa = (p - a as i16).abs();
                    let pb = (p - b as i16).abs();
                    let pc = (p - c as i16).abs();
                    let pr = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    v.wrapping_add(pr)
                }
                _ => return None,
            };
        }
        prev.copy_from_slice(cur);
    }

    // 合成到白底，输出 RGB
    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
    for i in 0..(w as usize * h as usize) {
        if channels == 4 {
            let r = pixels[i * 4];
            let g = pixels[i * 4 + 1];
            let b = pixels[i * 4 + 2];
            let a = pixels[i * 4 + 3] as u16;
            // src-over：out = src*a + dst*(1-a)，dst 是白 255
            let mix = |s: u8| -> u8 {
                ((s as u16 * a + 255 * (255 - a)) / 255).min(255) as u8
            };
            out.push(mix(r));
            out.push(mix(g));
            out.push(mix(b));
        } else {
            out.extend_from_slice(&pixels[i * 3..i * 3 + 3]);
        }
    }

    encode_png_rgb(w, h, &out)
}

/// 把 RGB 像素编码成 PNG（colortype=2，无 alpha）。
fn encode_png_rgb(w: u32, h: u32, rgb: &[u8]) -> Option<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let row_len = w as usize * 3;
    // 每行前加滤波类型 0（None），最简单也最快
    let mut raw = Vec::with_capacity((row_len + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgb[y * row_len..(y + 1) * row_len]);
    }

    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).ok()?;
    let compressed = enc.finish().ok()?;

    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    // IHDR
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type: RGB
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &compressed);
    push_chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn push_chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(typ);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// CRC32（PNG 用的标准多项式 0xEDB88320）。
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

// ============================================================
// HTML 注入
// ============================================================

/// 生成网页版中文注入片段。关闭或词库缺失时返回空串。
///
/// ## 为什么 Rust 侧也要实现一份
///
/// 桌面版中文靠 CDP 注入；网页版要在 HTML 里塞 <script>。
/// 而本机网页版（8898）是 **Rust 内核**自己转发的，不走 agygate.py，
/// 所以 Python 那边加的中文注入对本机版无效 —— 实测：8897（Python）有中文，
/// 8898（Rust）没有。两边都要实现。
///
/// 词库直接复用 zh-pack/dict.json，和桌面版同一份，不重复维护。
fn zh_snippet() -> String {
    // 开关在 ~/.agyhub/config.json 的 zh_enabled
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return String::new(),
    };
    let cfg_path = format!("{}/.agyhub/config.json", home);
    let enabled = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("zh_enabled").and_then(|b| b.as_bool()))
        .unwrap_or(false);
    if !enabled {
        return String::new();
    }

    // 词库直接从 actions::load_zh_pack() 取。
    //
    // 它优先读 ~/.agyhub/zh-pack/（用户可自行更新词库），
    // 没有就用编译进二进制的内置副本。
    // 所以**不依赖工作区里的中文包目录**，可以分发给别人。
    let (dict_str, rules_str, injector) = match crate::actions::load_zh_pack() {
        Some(v) => v,
        None => return String::new(),
    };

    // 缓存：词库 665 条、注入器 10KB，每次请求都重新拼太浪费。
    static CACHE: std::sync::OnceLock<std::sync::Mutex<String>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(String::new()));

    if let Ok(guard) = cache.lock() {
        if !guard.is_empty() {
            return guard.clone();
        }
    }

    let _ = home; // 保留变量，避免与上面的 cfg 读取冲突
    // ⚠️ 必须等 DOM 就绪再调用。
    //
    // inject.js 一上来就 scan(document.body) + observer.observe(document.body)，
    // 而这段 script 插在 </head> 之前 —— 那时 body 还是 null，
    // 直接调用会抛 TypeError，注入静默失败。
    // 表现：页面源码里明明有中文词库，界面却纹丝不动。
    let out = format!(
        "<script>\n(function(){{\n{}\nfunction __agyZhStart(){{try{{__AGY_ZH_INSTALL__({},{});}}catch(e){{console.warn('[agy-zh]',e);}}}}\nif(document.body){{__agyZhStart();}}else{{document.addEventListener('DOMContentLoaded',__agyZhStart,{{once:true}});}}\n}})();\n</script>\n",
        injector, dict_str, rules_str,
    );

    if let Ok(mut guard) = cache.lock() {
        *guard = out.clone();
    }
    out
}

/// 对 HTML 做注入。没有 </head> 时退到 <body>，再退到末尾。
pub fn patch_html(html: &str) -> String {
    let base = patch_snippet();
    // 中文注入拼在基础补丁后面；关闭时 zh_snippet() 返回空串，不影响其他功能
    let zh = zh_snippet();
    let patch: String = if zh.is_empty() {
        base.to_string()
    } else {
        format!("{}{}", base, zh)
    };
    let patch = patch.as_str();
    let mut out = String::with_capacity(html.len() + patch.len());

    // 标题先在服务端钉一次：先于任何 JS 执行，避免标签页闪一下旧名字。
    let html = set_title(html);

    // 注入点必须在 </head> 之前，但匹配要**不区分大小写**：
    // 上游模板用的是小写，将来若改风格不能因此失效。
    if let Some(idx) = find_ci(&html, "</head>") {
        out.push_str(&html[..idx]);
        out.push_str(&patch);
        out.push_str(&html[idx..]);
        return out;
    }
    if let Some(idx) = find_ci(&html, "<body") {
        if let Some(gt) = html[idx..].find('>') {
            let at = idx + gt + 1;
            out.push_str(&html[..at]);
            out.push_str(&patch);
            out.push_str(&html[at..]);
            return out;
        }
    }
    out.push_str(&html);
    out.push_str(&patch);
    out
}

/// 把 <title> 换成 App 名字。
fn set_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = match lower.find("<title") {
        Some(i) => i,
        None => return html.to_string(),
    };
    let close = match lower[open..].find("</title>") {
        Some(i) => open + i + "</title>".len(),
        None => return html.to_string(),
    };
    let mut s = String::with_capacity(html.len());
    s.push_str(&html[..open]);
    s.push_str("<title>Antigravity</title>");
    s.push_str(&html[close..]);
    s
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.find(&n)
}

/// 注入片段。全局只拼一次。
fn patch_snippet() -> &'static str {
    static SNIPPET: OnceLock<String> = OnceLock::new();
    SNIPPET.get_or_init(|| {
        let mut s = String::with_capacity(8192);
        s.push_str(BASE_STYLE);
        s.push_str(BRANDING_JS);
        s.push_str(MANIFEST_TAGS);
        s.push_str(INSTALL_HINT);
        s.push_str(CLEAN_JS);
        s
    })
}

// ---- 基础样式 ----
//
// iOS 对 font-size < 16px 的输入框会**强制放大整个页面**，且不会自动还原，
// 用户会卡在放大状态里出不来。这是移动端最影响可用性的一个问题。
const BASE_STYLE: &str = r#"<style id="agy-mobile-patch">
  html, body { -webkit-text-size-adjust: 100%; }
  input, textarea, select { font-size: 16px !important; }
  * { -webkit-overflow-scrolling: touch; }
</style>
"#;

// ---- 品牌：标题与图标 ----
//
// React 挂载后会改写 <title> 和图标，所以必须用 MutationObserver 反复钉。
// 关键是**防重入**：paint() 自己也会改 DOM，观察器若盯 head 会自我触发，
// 形成死循环把页面卡住（踩过）。
const BRANDING_JS: &str = r#"<script>
(function () {
  var ICON = '/__agy/icon.png';
  var TITLE = 'Antigravity';
  var busy = false;
  function paint() {
    if (busy) { return; }
    busy = true;
    try {
      if (document.title !== TITLE) { document.title = TITLE; }
      var old = document.querySelectorAll(
        "link[rel*='icon'], link[rel='apple-touch-icon'], link[rel='mask-icon']");
      for (var i = 0; i < old.length; i++) { old[i].parentNode.removeChild(old[i]); }
      var defs = [['icon','192x192'],['apple-touch-icon','180x180'],['shortcut icon','192x192']];
      for (var j = 0; j < defs.length; j++) {
        var l = document.createElement('link');
        l.rel = defs[j][0]; l.type = 'image/png'; l.sizes = defs[j][1]; l.href = ICON;
        document.head.appendChild(l);
      }
    } finally { busy = false; }
  }
  paint();
  try {
    var t = document.querySelector('title');
    var timer = null;
    new MutationObserver(function () {
      if (timer) { return; }
      timer = setTimeout(function () { timer = null; paint(); }, 50);
    }).observe(t || document.head, { childList: true, subtree: true, characterData: true });
  } catch (e) {}
})();
</script>
"#;

// ---- PWA 标签 ----
//
// 只加这几个。"能不能装"由两件事决定：
//   1) 页面是安全上下文（证书必须被设备信任，否则 Chromium 一律拒绝）
//   2) manifest 里 name/start_url/display/icons(192+512) 齐全
// 这里补第 2 条；第 1 条靠本地根 CA 解决。
//
// 注意用 r##"..."## ：内容里有 `"#FAF9F5"`，单井号的 raw string 会被 `"#` 提前截断。
const MANIFEST_TAGS: &str = r##"<link rel="manifest" href="/__agy/manifest.webmanifest">
<meta name="theme-color" content="#FAF9F5">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
"##;

// ---- 安装提示（只在窄屏）----
//
// 三分支，文案不同：
//   1) 已经是独立窗口（装过了）→ 不显示
//   2) isSecureContext=false（根证书没装）→ 提示下载 ca.crt
//      —— 这是 Chromium 拒绝安装的**真实原因**，必须点出来，
//         否则用户只看到"无法安装"却不知道为什么
//   3) 安全上下文正常 → 告诉用户去哪点"安装"
const INSTALL_HINT: &str = r#"<style id="agy-install-hint-style">
#agy-hint{position:fixed;left:0;right:0;bottom:0;z-index:2147483000;
  background:#2c2c2a;color:#fff;font:13px/1.5 -apple-system,system-ui,sans-serif;
  padding:10px 14px calc(10px + env(safe-area-inset-bottom));
  display:flex;gap:10px;align-items:flex-start;
  box-shadow:0 -2px 12px rgba(0,0,0,.18)}
#agy-hint .t{flex:1;-webkit-user-select:none;user-select:none}
#agy-hint b{font-weight:600}
#agy-hint a{color:#9ecbff;text-decoration:underline}
#agy-hint button{background:none;border:0;color:#9a9a96;font-size:18px;
  line-height:1;padding:0 2px;cursor:pointer}
</style>
<script>
(function () {
  function standalone() {
    try {
      if (window.matchMedia &&
          window.matchMedia('(display-mode: standalone), (display-mode: window-controls-overlay)').matches) {
        return true;
      }
      return window.navigator.standalone === true;
    } catch (e) { return false; }
  }
  function narrow() {
    var w = (document.body && document.body.clientWidth) || window.innerWidth || 0;
    return w > 0 && w < 640;
  }
  if (standalone()) { return; }
  try { if (localStorage.getItem('__agyHintOff') === '1') { return; } } catch (e) {}

  // 证书是否受信任。
  //
  // isSecureContext 是浏览器给的权威判定：
  //   · true  → 证书链验证通过（系统信任了我们的根 CA）
  //   · false → 要么没装根证书，要么装了但没设成"信任"
  //
  // 用户实测的困惑："安装证书后页面依然提示不安全"。
  // 原因通常是**装了但没信任** —— macOS/iOS/Android 都要求两步：
  // 先导入证书，再去设置里把它标成"受信任"。
  // 所以提示里必须把这一点写清楚，否则用户永远装不对。
  var insecure = !window.isSecureContext;

  function mount() {
    if (document.getElementById('agy-hint')) { return; }
    var d = document.createElement('div');
    d.id = 'agy-hint';

    // 证书不受信任：不管屏幕宽窄都要显示 ——
    // 因为这时连 PWA 都装不了，功能实际是残的。
    // 而"可以装成应用"这种锦上添花的提示只在窄屏（手机）显示。
    if (insecure) {
      var mobile = narrow();
      var msg = '<b>⚠️ 证书未受信任</b><br>'
        + '这会让页面显示「不安全」，也无法装成应用。'
        + '<br><br>'
        + '<b>怎么修：</b><br>'
        + '1. <a href="/__agy/ca.crt" rel="noopener">下载根证书 ca.crt</a><br>'
        + '2. 在系统里导入它<br>'
        + '3. <b>关键一步</b>：去设置里把它设为「信任 / 始终信任」<br>'
        + '4. 完全关闭浏览器再打开本页<br><br>'
        + '<span style="opacity:.7;font-size:12px">'
        + (mobile
            ? 'iPhone：设置 → 通用 → 关于本机 → 证书信任设置 → 打开开关<br>'
              + '安卓：设置 → 安全 → 加密与凭据 → 安装证书 → CA 证书'
            : 'macOS：双击 ca.crt → 钥匙串访问 → 找到证书 → 改为「始终信任」')
        + '</span>';
      d.innerHTML = '<div class="t">' + msg + '</div>'
        + '<button aria-label="关闭">&times;</button>';
      d.querySelector('button').addEventListener('click', function () {
        try { localStorage.setItem('__agyHintOff', '1'); } catch (e) {}
        d.remove();
      });
      document.body.appendChild(d);
      return;
    }

    // 证书正常：只在手机上提示"可以装成应用"
    if (!narrow()) { return; }
    var msg2 = '<b>可以装成应用。</b><br>'
      + '安卓 Chrome：右上角菜单 →「安装应用 / 添加到主屏幕」；'
      + 'iPhone Safari：分享 →「添加到主屏幕」。'
      + '<br><span style="opacity:.65">装完后全屏无地址栏，像原生 App。</span>';
    d.innerHTML = '<div class="t">' + msg2 + '</div>'
      + '<button aria-label="关闭">&times;</button>';
    d.querySelector('button').addEventListener('click', function () {
      try { localStorage.setItem('__agyHintOff', '1'); } catch (e) {}
      d.remove();
    });
    document.body.appendChild(d);
  }
  if (document.body) { mount(); }
  else { document.addEventListener('DOMContentLoaded', mount); }
})();
</script>
"#;

// ---- 清理历史残留 ----
//
// 早期版本往 localStorage 写过 host=extension 的覆盖键，那个设置会让
// 点击文件/openDiff 全部静默失效。这里只在带自家标记时才删，
// 避免误删用户自己写的东西。
const CLEAN_JS: &str = r#"<script>
(function () {
  window.__AGY_GATEWAY__ = true;
  document.documentElement.setAttribute('data-agy-gateway', '1');
  try {
    var KEY = 'jetski.developer.featureEnvironmentOverride';
    var raw = localStorage.getItem(KEY);
    if (raw && raw.indexOf('__agyShell') >= 0) { localStorage.removeItem(KEY); }
  } catch (e) {}
})();
</script>
"#;

// ---- 诊断页 ----
const DIAG_HTML: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>诊断 · Antigravity Hub</title>
<style>
  body{font:14px/1.7 -apple-system,BlinkMacSystemFont,sans-serif;
       max-width:640px;margin:0 auto;padding:24px;color:#1d1d1f;background:#faf9f5}
  h1{font-size:18px}
  table{border-collapse:collapse;width:100%;margin-top:12px}
  td{padding:7px 4px;border-bottom:1px solid #e5e5e0;vertical-align:top}
  td:first-child{color:#6e6e73;width:42%}
  .ok{color:#1a7f37;font-weight:600}
  .no{color:#cf222e;font-weight:600}
</style></head><body>
<h1>诊断</h1>
<table>
  <tr><td>安全上下文</td><td id="sec">—</td></tr>
  <tr><td>服务工作者</td><td id="sw">—</td></tr>
  <tr><td>独立窗口</td><td id="standalone">—</td></tr>
  <tr><td>屏幕宽度</td><td id="w">—</td></tr>
  <tr><td>协议</td><td id="proto">—</td></tr>
</table>
<script>
(function () {
  function set(id, txt, ok) {
    var e = document.getElementById(id);
    e.textContent = txt;
    e.className = ok === true ? 'ok' : (ok === false ? 'no' : '');
  }
  set('sec', window.isSecureContext ? '是' : '否（根证书未受信任）', !!window.isSecureContext);
  var swOk = 'serviceWorker' in navigator;
  set('sw', swOk ? '可用' : '不可用（需要安全上下文）', swOk);
  var s = window.navigator.standalone === true ||
    (window.matchMedia && window.matchMedia('(display-mode: standalone)').matches);
  set('standalone', s ? '是（已装成应用）' : '否（在浏览器里）', null);
  document.getElementById('w').textContent = window.innerWidth + ' px';
  document.getElementById('proto').textContent = location.protocol;
})();
</script>
</body></html>
"#;

// ============================================================
// 引导页：Antigravity 不可用时给用户看得懂的提示
// ============================================================
//
// ## 为什么需要这个
//
// 原来上游连不上时只返回一行纯文本 `Antigravity not running`，
// 用户在手机浏览器里看到这行字，完全不知道该干什么 ——
// 是没装？没开？没登录？还是网络问题？
//
// 这三种情况要引导的动作完全不同，所以必须分开判断、分开提示。
//
// ## 状态判定
//
// | 状态 | 判据 |
// |---|---|
// | 未安装 | /Applications/Antigravity.app 不存在 |
// | 未运行 | 装了但没有进程 |
// | 未登录 | 进程在但没有可用的登录态 |
//
// 判定结果是**建议**，不是绝对结论 —— 所以页面上同时给出
// 手动操作指引，即使判定错了用户也能自己解决。

/// Antigravity 当前处于什么状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgyStatus {
    /// 没装
    NotInstalled,
    /// 装了但没运行
    NotRunning,
    /// 运行中，但找不到上游端口（多半是没登录 / 还在启动）
    NoUpstream,
}

/// 判断 Antigravity 的状态。
pub fn detect_status() -> AgyStatus {
    // 1. 装了吗
    let installed = std::path::Path::new("/Applications/Antigravity.app").exists()
        || std::path::Path::new("/Applications/Antigravity Beta.app").exists();
    if !installed {
        return AgyStatus::NotInstalled;
    }

    // 2. 运行了吗
    //
    // 用 pgrep 而不是 pidof：macOS 没有 pidof。
    // 匹配主可执行文件名足够，不必精确到路径（路径可能变）。
    let running = std::process::Command::new("pgrep")
        .args(["-f", "Antigravity.app/Contents/MacOS"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if !running {
        return AgyStatus::NotRunning;
    }

    AgyStatus::NoUpstream
}

/// 生成引导页 HTML。
///
/// 每个状态给出**该做的事**，而不是一句错误。
pub fn guide_html(status: AgyStatus, lan_ip: &str, port: u16) -> String {
    let (title, icon, body) = match status {
        AgyStatus::NotInstalled => (
            "还没安装 Antigravity",
            "📦",
            r#"
            <p>这台电脑上找不到 Antigravity 应用。</p>
            <p>Antigravity 是 Google 的 AI 编程工具，需要先装上它，
               这个网关才能把你的编程界面转给手机。</p>
            <ol>
              <li>去 <a href="https://antigravity.google" target="_blank">antigravity.google</a> 下载</li>
              <li>拖进「应用程序」文件夹</li>
              <li>打开并完成登录</li>
              <li>再刷新这个页面</li>
            </ol>
            "#,
        ),
        AgyStatus::NotRunning => (
            "Antigravity 没有打开",
            "😴",
            r#"
            <p>应用装好了，但当前没有在运行。</p>
            <p>网关要转发它的界面，所以它必须开着。</p>
            <ol>
              <li>在这台电脑上打开 Antigravity</li>
              <li>等界面完全加载出来</li>
              <li>再刷新这个页面</li>
            </ol>
            <p class="hint">小提示：勾选菜单栏 AgyHub 里的「登录时启动」，
               以后开机就会自动就绪。</p>
            "#,
        ),
        AgyStatus::NoUpstream => (
            "还差一步：登录",
            "🔑",
            r#"
            <p>Antigravity 正在运行，但读不到它的界面端口。</p>
            <p>最常见的原因是<strong>还没登录 Google 账号</strong> ——
               未登录时它不会启动本地服务。</p>
            <ol>
              <li>看这台电脑上的 Antigravity 窗口</li>
              <li>如果显示登录页，完成登录</li>
              <li>如果卡在加载，等它加载完（首次启动较慢）</li>
              <li>再刷新这个页面</li>
            </ol>
            "#,
        ),
    };

    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>{title} · AgyHub</title>
<style>
  :root {{
    --bg: #0f1115; --card: #181b22; --text: #e6e8ee;
    --dim: #9aa0ab; --accent: #6ea8fe; --warn: #f5a524;
  }}
  @media (prefers-color-scheme: light) {{
    :root {{
      --bg: #f6f7f9; --card: #ffffff; --text: #1a1d23;
      --dim: #5b6270; --accent: #2563eb; --warn: #b45309;
    }}
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; padding: 24px 20px 48px;
    background: var(--bg); color: var(--text);
    font: 16px/1.7 -apple-system, "SF Pro Text", "PingFang SC", sans-serif;
    -webkit-text-size-adjust: 100%;
  }}
  .wrap {{ max-width: 560px; margin: 0 auto; }}
  .icon {{ font-size: 44px; line-height: 1; margin-bottom: 12px; }}
  h1 {{ font-size: 21px; margin: 0 0 16px; letter-spacing: -0.01em; }}
  .card {{
    background: var(--card); border-radius: 14px;
    padding: 22px 20px; margin-bottom: 16px;
  }}
  p {{ margin: 0 0 12px; }}
  ol {{ margin: 14px 0 0; padding-left: 22px; }}
  li {{ margin-bottom: 10px; }}
  strong {{ color: var(--warn); }}
  a {{ color: var(--accent); }}
  .hint {{
    margin-top: 16px; padding-top: 14px;
    border-top: 1px solid rgba(128,128,128,.2);
    font-size: 14px; color: var(--dim);
  }}
  .meta {{
    font-size: 13px; color: var(--dim); text-align: center;
    margin-top: 26px; line-height: 1.8;
  }}
  .meta code {{
    background: rgba(128,128,128,.15); padding: 2px 6px;
    border-radius: 4px; font-size: 12px;
  }}
  .btn {{
    display: block; width: 100%; margin-top: 6px;
    padding: 13px; border: 0; border-radius: 10px;
    background: var(--accent); color: #fff;
    font-size: 16px; font-weight: 500; cursor: pointer;
  }}
  .btn:active {{ opacity: .8; }}
</style>
</head>
<body>
<div class="wrap">
  <div class="icon">{icon}</div>
  <h1>{title}</h1>

  <div class="card">{body}</div>

  <button class="btn" onclick="location.reload()">重新检测</button>

  <div class="meta">
    网关运行正常 · 端口 {port}<br>
    本机地址 <code>{lan_ip}</code>
  </div>
</div>
</body>
</html>"#,
        title = title,
        icon = icon,
        body = body,
        port = port,
        lan_ip = lan_ip,
    )
}
