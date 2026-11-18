//! SSH 入口解锁与中文还原。
//!
//! 这两件事原先由外部 node 脚本完成：
//!
//! | 原脚本 | 现在 |
//! |---|---|
//! | `agy-ssh-unlock.cjs` | 本文件的 `unlock_ssh_entry()` |
//! | `stop-zh.cjs` | 本文件的 `restore_zh_ui()` |
//!
//! 注入用的 JS 内容编译进二进制（`include_str!`），
//! 所以分发出去不需要任何外部文件。

use serde_json::json;

use crate::cdp::{js_json, Cdp};

/// 注入到页面的钩子：篡改 GetMendelFlags 响应，删掉 disabled 标记。
///
/// 内容来自原 `agy-ssh-unlock.cjs` 的 `PAGE_HOOK`。
/// 原理：Antigravity 服务端的 experiments 里本来就有
/// `enable-ssh-remote` 这一条，但带 `"disabled": true`。
/// 这个钩子在响应经过时把该字段删掉，前端就会渲染出 SSH 入口。
const SSH_UNLOCK_HOOK: &str = include_str!("../assets/ssh-unlock-hook.js");

/// 关闭中文时用来还原界面的 JS。
const STOP_ZH_RESTORE: &str = include_str!("../assets/stop-zh-restore.js");

/// 一次性解锁 SSH 入口（给当前所有页面装钩子）。
///
/// 注意这只是"装钩子"，钩子要等到下一次 `GetMendelFlags` 请求
/// 才会真正改写响应。用户打开或切换对话时就会触发。
///
/// **不能** reload 页面：那会造成无限刷新循环
/// （attachedToTarget → 注入 → reload → 又被捕获 → …），
/// 表现是新窗口一直闪白，用户根本来不及操作。
pub async fn unlock_ssh_entry() -> Result<String, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    if pages.is_empty() {
        return Err("没有找到 Antigravity 页面".into());
    }

    let mut injected = 0;
    let mut detail = Vec::new();

    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(e) => {
                detail.push(format!("{}: attach 失败 {}", p.url, e));
                continue;
            }
        };
        let _ = cdp.send("Page.enable", json!({}), Some(&sid)).await;

        // 装钩子：对后续导航生效
        let r = cdp
            .send(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": SSH_UNLOCK_HOOK }),
                Some(&sid),
            )
            .await;

        match r {
            Ok(_) => {
                // 对当前已加载的文档，立刻执行一次
                let _ = cdp.eval(&sid, SSH_UNLOCK_HOOK).await;
                injected += 1;
                detail.push(format!("已注入: {}", truncate(&p.url, 60)));
            }
            Err(e) => detail.push(format!("注入失败 {}: {}", truncate(&p.url, 40), e)),
        }
    }

    if injected == 0 {
        return Err(format!("全部页面注入失败：{}", detail.join("; ")));
    }

    Ok(format!("已给 {} 个页面装好钩子", injected))
}

/// 查询当前钩子的命中情况（用于诊断）。
pub async fn ssh_unlock_status() -> Result<serde_json::Value, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    let mut out = Vec::new();

    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let expr = r#"(function(){
            var s = window.__AGY_SSH_UNLOCK__;
            return {
                hook: !!s,
                hits: s ? s.hits : -1,
                patched: s ? s.patched : -1
            };
        })()"#;
        if let Ok(v) = cdp.eval(&sid, expr).await {
            out.push(json!({ "url": truncate(&p.url, 60), "state": v }));
        }
    }

    Ok(json!({ "pages": out }))
}

/// 关闭中文后还原界面。
///
/// 为什么光杀守护进程不够：翻译是注入到 DOM 里的，
/// 文字已经被改成中文了。注入脚本的 `destroy()` 只是断开
/// MutationObserver，**不会把已经改掉的字改回去**。
/// 所以必须清标记 + 刷新页面，让浏览器重新拉一份原始 HTML。
pub async fn restore_zh_ui() -> Result<String, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    if pages.is_empty() {
        return Err("没有找到 Antigravity 页面".into());
    }

    let mut n = 0;
    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = cdp.send("Page.enable", json!({}), Some(&sid)).await;

        // ---- 关键：先移除所有"导航后自动执行"的注入脚本 ----
        //
        // 这一条是必须的，而且顺序不能反。
        //
        // `zh on` 时我们用 Page.addScriptToEvaluateOnNewDocument 装了
        // 一段脚本，它会在**每次页面加载时自动执行**。如果只跑还原脚本
        // 再 reload，刷新后那段脚本立刻又跑一遍，页面又变回中文 ——
        // 表现就是"关掉了但界面还是中文"。
        //
        // 必须先 Page.removeScriptToEvaluateOnNewDocument 把注册的脚本
        // 全部摘掉，再刷新，才能真正回到英文。
        if let Ok(res) = cdp.send("Page.getFrameTree", json!({}), Some(&sid)).await {
            let _ = res; // 只是确认会话可用
        }

        // 取当前注册的脚本 id 列表并逐个移除。
        // CDP 没有"列出所有已注册脚本"的接口，只能靠自己记录 ——
        // 但 zh on 是另一个进程执行的，这里拿不到它的 id。
        // 所以换个更可靠的办法：直接清空 by 重新 attach。
        //
        // 实测可行的做法：刷新前先 eval 还原脚本，刷新时
        // addScriptToEvaluateOnNewDocument 的效果**不会**跨进程保留 ——
        // 但同一个 CDP 连接内会保留。这里我们每次都是新连接，
        // 所以只要不重新注册就不会执行。
        let _ = cdp.eval(&sid, STOP_ZH_RESTORE).await;

        // 刷新：新页面不再有注入脚本（本连接没注册过），
        // 拿到的就是原始英文 HTML。
        let _ = cdp.reload(&sid).await;
        n += 1;
    }

    Ok(format!("已还原并刷新 {} 个页面", n))
}

/// 页面里注入中文翻译。
///
/// 与网页版不同：这里是**桌面版**（Electron 渲染进程），
/// 页面早已加载完成，所以不需要等 DOMContentLoaded。
pub async fn apply_zh(dict_json: &str, rules_json: &str, injector_js: &str) -> Result<String, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    if pages.is_empty() {
        return Err("没有找到 Antigravity 页面".into());
    }

    // 整个注入器 + 词库 + 启动调用，拼成一段自包含表达式。
    // 和网页版那边一样，必须处理 body 尚未就绪的情况 ——
    // 桌面版一般已就绪，但切页面时可能碰上中间态。
    let expr = format!(
        r#"(function(){{
{injector}
function __agyZhStart(){{try{{__AGY_ZH_INSTALL__({dict},{rules});}}catch(e){{console.warn('[agy-zh]',e);}}}}
if(document.body){{__agyZhStart();}}
else{{document.addEventListener('DOMContentLoaded',__agyZhStart,{{once:true}});}}
return true;
}})()"#,
        injector = injector_js,
        dict = dict_json,
        rules = rules_json,
    );

    let mut n = 0;
    let mut errors = Vec::new();
    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("attach: {}", e));
                continue;
            }
        };

        // ① 先对**当前文档**执行一次。
        //    addScriptToEvaluateOnNewDocument 只对之后的导航生效，
        //    对已经加载完的页面完全没用 —— 这是必须分开做的原因。
        match cdp.eval(&sid, &expr).await {
            Ok(v) => {
                // 注入器返回翻译的节点数；false/null 说明启动函数没跑起来
                if !v.is_null() && v != serde_json::Value::Bool(false) {
                    n += 1;
                } else {
                    errors.push(format!("{}: 注入返回空", truncate(&p.url, 30)));
                }
            }
            Err(e) => errors.push(format!("{}: {}", truncate(&p.url, 30), e)),
        }

        // ② 同时装成"每次导航后自动注入"，否则刷新/切页面就丢
        let _ = cdp
            .send(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": expr }),
                Some(&sid),
            )
            .await;
    }

    if n == 0 {
        return Err(format!(
            "全部页面注入失败：{}",
            if errors.is_empty() { "无页面".into() } else { errors.join("; ") }
        ));
    }

    Ok(format!("已对 {} 个页面注入中文", n))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// 把中文词库打包成 (dict_json, rules_json, injector_js)。
///
/// 词库来源优先级：
///   1. `~/.agyhub/zh-pack/`（用户自己放的，便于更新词库而不换 App）
///   2. 编译进二进制的内置副本
pub fn load_zh_pack() -> Option<(String, String, String)> {
    // 内置副本
    const BUILTIN_DICT: &str = include_str!("../assets/zh-dict.json");
    const BUILTIN_INJECT: &str = include_str!("../assets/zh-inject.js");
    const BUILTIN_RULES: &str = include_str!("../assets/zh-rules.json");

    let home = std::env::var("HOME").ok()?;
    let dir = format!("{}/.agyhub/zh-pack", home);

    let read_or = |name: &str, builtin: &str| -> String {
        let p = format!("{}/{}", dir, name);
        std::fs::read_to_string(&p).unwrap_or_else(|_| builtin.to_string())
    };

    let dict_raw = read_or("dict.json", BUILTIN_DICT);
    let inject = read_or("inject.js", BUILTIN_INJECT);
    let rules_raw = read_or("rules.json", BUILTIN_RULES);

    // dict.json 里可能同时含 DICT 和 RULES
    let (dict, rules) = match serde_json::from_str::<serde_json::Value>(&dict_raw) {
        Ok(v) => {
            let d = v.get("DICT").cloned().unwrap_or(v.clone());
            let r = v.get("RULES").cloned().unwrap_or_else(|| json!({}));
            (d, r)
        }
        Err(_) => return None,
    };

    let rules = if rules.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        serde_json::from_str::<serde_json::Value>(&rules_raw).unwrap_or_else(|_| json!({}))
    } else {
        rules
    };

    Some((js_json(&dict), js_json(&rules), inject))
}
