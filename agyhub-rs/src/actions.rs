//! 桌面版 Antigravity 的注入：SSH 入口解锁与中文界面。
//!
//! 注入用的 JS 编译进二进制（`include_str!`），分发出去不需要任何外部文件。
//!
//! ## 为什么要"巡检 + 补注入"，而不是注入一次就完事
//!
//! 注入走 CDP。有两个事实决定了必须巡检：
//!   1. `Page.addScriptToEvaluateOnNewDocument` 登记的脚本**跟着 CDP 会话走**，
//!      我们的命令一退出、连接一断，登记就失效了。
//!   2. 页面刷新、开新窗口、Antigravity 重启，都会得到一个干净的新页面。
//!
//! 所以"中文界面重启后自动恢复"、"SSH 入口一直在"只能靠：
//! App 每隔几秒调一次 `agyhub ensure-inject`，发现哪个页面缺了就补上。
//! 已经注入过的页面只做一次很轻的检查（一条 Runtime.evaluate），不重复注入。

use serde_json::{json, Value};

use crate::cdp::{js_json, Cdp};
use crate::state;
use crate::sys;

/// 注入到页面的钩子：篡改 GetMendelFlags 响应，删掉 disabled 标记。
///
/// 原理：Antigravity 服务端的 experiments 里本来就有 `enable-ssh-remote`
/// 这一条，但带 `"disabled": true`。钩子在响应经过时把该字段删掉，
/// 前端就会渲染出 SSH 入口。
const SSH_UNLOCK_HOOK: &str = include_str!("../assets/ssh-unlock-hook.js");

/// 关闭中文时用来还原界面的 JS。
const STOP_ZH_RESTORE: &str = include_str!("../assets/stop-zh-restore.js");

/// 关闭 SSH 解锁：让钩子直接放行（下次刷新页面后彻底消失）。
const SSH_UNLOCK_OFF: &str = r#"(function(){
  var S = window.__AGY_SSH_UNLOCK__;
  if (S) { S.off = true; }
  return true;
})()"#;

/// 页面当前注入情况。
const PROBE_JS: &str = r#"(function(){
  var s = window.__AGY_SSH_UNLOCK__;
  return {
    zh: !!window.__AGY_ZH__,
    ssh: !!(s && !s.off),
    usage: !!window.__AGY_USAGE__,
    // 钩子真正改写过 GetMendelFlags 响应的次数。为 0 说明钩子装上了，
    // 但页面还没再请求过开关配置，SSH 入口暂时还不会出现。
    patched: s ? (s.patched || 0) : 0,
    ready: document.readyState
  };
})()"#;

/// 桌面版注入「上下文与用量」指示器。
///
/// ## 为什么现在才做
///
/// 早先只在网关注入（手机/网页版），因为那时以为桌面版用不上。
/// 实际上桌面版同样需要看用量，而它不经过网关，只能走 CDP。
///
/// ## 为什么可行（不是硬凑）
///
/// 和中文注入是同一套机制，两个关键前提都成立：
///
/// 1. **数据通道一样**：桌面版窗口加载的是 `http://127.0.0.1:<port>/`，
///    URL 不带 `wsTransport` 参数 → 前端 `nKd()` 返回 `"off"` →
///    走 gRPC-Web fetch，`StreamAgentStateUpdates` 同样是 fetch 请求。
///    （只有 `?useWebSocket=true` 或 `wsTransport=3` 才切 WebSocket，
///    桌面版两者都不带，已从 app.asar 里确认它加载的就是裸 URL。）
///
/// 2. **DOM 一样**：桌面版和网页版是同一个前端包（同一份 /main.js），
///    `data-testid="send-button"` 这些锚点在两边都成立 ——
///    中文注入能工作本身就是证明。
///
/// ## 不能直接复用网关注入的那段 <script>
///
/// 网关是把 `<script id="agy-pricing">` + `<script id="agy-usage-indicator">`
/// 两个标签插进 HTML。CDP 里没有 HTML 可插，所以：
///   · 价格表要先自己塞一个 `<script id="agy-pricing">` 进 DOM，
///     否则脚本里 `getElementById('agy-pricing')` 拿到 null，金额全算不出来；
///   · 指示器脚本用 eval 直接执行。
const USAGE_INSTALL_JS: &str = include_str!("../assets/usage-indicator.js");

/// 价格表（注入到 DOM 里供指示器读取）。
const PRICING_JSON: &str = include_str!("../assets/pricing.json");

/// 拼出「给桌面版装指示器」的完整 JS 表达式。
///
/// 拆成两步而不是一个字符串，是因为价格表要原样放进 `<script>` 的
/// textContent，拼错一个引号整段就废了。用 serde 序列化成 JS 字面量最稳。
fn usage_desktop_expr() -> String {
    // 价格表当字符串常量传入，脚本里自己 createElement 挂上去
    let pricing_literal = js_json(&Value::String(PRICING_JSON.to_string()));
    format!(
        r#"(function(){{
  // 价格表：网关注入时是 HTML 里现成的 <script id="agy-pricing">，
  // CDP 注入没有 HTML，得自己造一个。已存在就不重复加。
  try {{
    if (!document.getElementById('agy-pricing')) {{
      var el = document.createElement('script');
      el.id = 'agy-pricing';
      el.type = 'application/json';
      el.textContent = {pricing};
      (document.head || document.documentElement).appendChild(el);
    }}
  }} catch (e) {{}}
}})();
{script}"#,
        pricing = pricing_literal,
        script = USAGE_INSTALL_JS
    )
}


// ## 什么时候允许为 SSH 解锁刷新页面
//
// 解锁要刷新一次页面：钩子必须在页面脚本之前装好，才能截到启动时那次
// GetMendelFlags。但刷新会打断用户正在做的事，所以严格限制：
//
//   · 用户刚打开开关（force）：刷新所有窗口 —— 用户预期会有变化
//   · 新出现的窗口（之前没见过的 target id）：刷新一次
//   · 见过的窗口钩子丢了（用户自己按了 ⌘R）：**不刷新**，只把钩子装进当前页面，
//     之后页面再请求 GetMendelFlags（切换对话等）时生效
//
// 没有这个限制就会变成老 node 版踩过的坑：每次巡检都刷新，窗口不停闪白。

/// 一次巡检的结果。
#[derive(Debug, Default)]
pub struct Report {
    pub pages: usize,
    pub zh_ok: usize,
    pub ssh_ok: usize,
    /// 钩子已经生效（改写过响应）的页面数
    pub ssh_active: usize,
    /// 已装上用量指示器的页面数
    pub usage_ok: usize,
    pub actions: Vec<String>,
    pub errors: Vec<String>,
}

/// 中文注入表达式（注入器 + 词库 + 启动调用，自包含）。
fn zh_expr() -> Result<String, String> {
    let (dict, rules, injector) = load_zh_pack().ok_or("内置中文词库解析失败")?;
    Ok(format!(
        r#"(function(){{
{injector}
function __agyZhStart(){{try{{__AGY_ZH_INSTALL__({dict},{rules});}}catch(e){{console.warn('[agy-zh]',e);}}}}
if(document.body){{__agyZhStart();}}
else{{document.addEventListener('DOMContentLoaded',__agyZhStart,{{once:true}});}}
return true;
}})()"#,
        injector = injector,
        dict = dict,
        rules = rules,
    ))
}

/// 巡检所有 Antigravity 页面，按需补注入。
///
/// - `want_zh` / `want_ssh`：配置里开着的功能
/// - `force_ssh_reload`：用户刚打开 SSH 开关时为 true —— 这时允许刷新页面，
///   不受刷新间隔限制（用户主动操作，预期页面会刷新一下）
pub async fn ensure_injected(
    want_zh: bool,
    want_ssh: bool,
    want_usage: bool,
    force_ssh_reload: bool,
) -> Result<Report, String> {
    let devtools_port = crate::cdp::read_debug_port()?;
    let mut st = state::inject_load();
    if st.devtools_port != devtools_port {
        // Antigravity 重启过：旧的刷新记录作废
        st.reloaded.clear();
    }

    let result =
        ensure_injected_inner(want_zh, want_ssh, want_usage, force_ssh_reload, &mut st).await;

    st.ts = sys::now_secs();
    st.devtools_port = devtools_port;
    match &result {
        Ok(r) => {
            st.pages = r.pages;
            st.zh = want_zh && r.pages > 0 && r.zh_ok == r.pages;
            st.ssh = want_ssh && r.pages > 0 && r.ssh_ok == r.pages;
            st.ssh_active = want_ssh && r.ssh_active > 0;
            st.usage = want_usage && r.pages > 0 && r.usage_ok == r.pages;
            st.error = r.errors.join("; ");
        }
        Err(e) => {
            st.pages = 0;
            st.zh = false;
            st.ssh = false;
            st.ssh_active = false;
            st.usage = false;
            st.error = e.clone();
        }
    }
    state::inject_save(&st);
    result
}

async fn ensure_injected_inner(
    want_zh: bool,
    want_ssh: bool,
    want_usage: bool,
    force_ssh_reload: bool,
    st: &mut state::InjectState,
) -> Result<Report, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    let mut rep = Report {
        pages: pages.len(),
        ..Default::default()
    };
    if pages.is_empty() {
        return Err("没有找到 Antigravity 页面（窗口可能还没打开）".into());
    }

    let zh_src = if want_zh { Some(zh_expr()?) } else { None };
    // 用量指示器的注入表达式（价格表 + 脚本），只在开关打开时才拼
    let usage_expr = if want_usage {
        usage_desktop_expr()
    } else {
        String::new()
    };

    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(e) => {
                rep.errors.push(format!("attach 失败: {}", e));
                continue;
            }
        };

        let probe = cdp.eval(&sid, PROBE_JS).await.unwrap_or(Value::Null);
        let has_zh = probe.get("zh").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut has_ssh = probe.get("ssh").and_then(|v| v.as_bool()).unwrap_or(false);

        // ---- SSH 入口 ----
        if want_ssh && !has_ssh {
            let may_reload = force_ssh_reload || !st.reloaded.contains_key(&p.id);
            // ⚠️ 刷新之前就记下，不管结果如何。
            // 如果只在成功时记录，"刷新了但验证失败"的页面下一轮又会被刷新，
            // 就是每几秒刷新一次的死循环。
            if may_reload {
                st.reloaded.insert(p.id.clone(), sys::now_secs());
                // 立刻落盘：这一轮如果超时被中断，结尾的保存不会执行，
                // 下一轮就会把所有窗口再刷新一遍
                state::inject_save(st);
            }
            match install_ssh_hook(&mut cdp, &sid, may_reload).await {
                Ok(reloaded) => {
                    rep.actions.push(format!(
                        "SSH 钩子已装{}: {}",
                        if reloaded { "并刷新" } else { "" },
                        truncate(&p.url, 50)
                    ));
                    has_ssh = true;
                }
                Err(e) => rep.errors.push(format!("SSH 注入失败: {}", e)),
            }
        }
        if has_ssh {
            rep.ssh_ok += 1;
            // 刚刷新过的页面要重新探一次才知道有没有生效
            let patched = cdp
                .eval(&sid, PROBE_JS)
                .await
                .ok()
                .and_then(|v| v.get("patched").and_then(|n| n.as_u64()))
                .unwrap_or(0);
            if patched > 0 {
                rep.ssh_active += 1;
            }
        }

        // ---- 中文 ----
        //
        // 放在 SSH 后面：SSH 解锁可能刷新了页面，刷新后旧的中文注入就没了，
        // 这里要重新检查一次。
        if let Some(src) = &zh_src {
            let still_has_zh = if want_ssh && !probe.get("ssh").and_then(|v| v.as_bool()).unwrap_or(false) {
                // 可能刚刷新过，重新探一次
                cdp.eval(&sid, PROBE_JS)
                    .await
                    .ok()
                    .and_then(|v| v.get("zh").and_then(|b| b.as_bool()))
                    .unwrap_or(false)
            } else {
                has_zh
            };
            if still_has_zh {
                rep.zh_ok += 1;
            } else {
                match cdp.eval(&sid, src).await {
                    Ok(_) => {
                        rep.zh_ok += 1;
                        rep.actions.push(format!("已注入中文: {}", truncate(&p.url, 50)));
                    }
                    Err(e) => rep.errors.push(format!("中文注入失败: {}", e)),
                }
            }
        }

        // ---- 上下文与用量指示器（桌面版）----
        //
        // 放在最后：前面 SSH 可能刷新过页面，刷新后要重新探。
        // 这个指示器**不需要刷新页面** —— 它只是包一层 window.fetch，
        // 装上之后页面下一次发 StreamAgentStateUpdates 就能拿到数据，
        // 不必打断用户当前的操作。
        if want_usage {
            let still_has = cdp
                .eval(&sid, PROBE_JS)
                .await
                .ok()
                .and_then(|v| v.get("usage").and_then(|b| b.as_bool()))
                .unwrap_or(false);
            if still_has {
                rep.usage_ok += 1;
            } else {
                match cdp.eval(&sid, &usage_expr).await {
                    Ok(_) => {
                        rep.usage_ok += 1;
                        rep.actions
                            .push(format!("已注入用量指示器: {}", truncate(&p.url, 50)));
                    }
                    Err(e) => rep.errors.push(format!("用量指示器注入失败: {}", e)),
                }
            }
        }

        cdp.detach(&sid).await;
    }

    // 关掉的窗口不再记录（防止文件无限增长）。
    // 注意：没开 SSH 时不能清，否则之后打开 SSH 会把所有窗口当成"新窗口"刷新。
    if want_ssh {
        st.reloaded.retain(|id, _| pages.iter().any(|p| &p.id == id));
    }

    Ok(rep)
}

/// 给一个页面装 SSH 钩子。返回是否刷新了页面。
///
/// 只对当前文档执行一次钩子是不够的：Antigravity 在页面启动时就请求了
/// GetMendelFlags，钩子装晚了截不到。所以（允许时）要：
///   1. addScriptToEvaluateOnNewDocument 登记钩子
///   2. 刷新页面 —— 新文档在任何页面脚本之前先跑钩子
///   3. 等页面加载完、确认钩子在，才断开连接
///      （断开后登记失效，但钩子已经在新页面里运行了，不受影响）
async fn install_ssh_hook(cdp: &mut Cdp, sid: &str, may_reload: bool) -> Result<bool, String> {
    // 先对当前文档执行一次（语法错误会在这里直接暴露出来）
    cdp.eval(sid, SSH_UNLOCK_HOOK).await?;
    if !may_reload {
        return Ok(false);
    }

    let _ = cdp.send("Page.enable", json!({}), Some(sid)).await;
    cdp.send(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": SSH_UNLOCK_HOOK }),
        Some(sid),
    )
    .await?;
    cdp.reload(sid).await?;

    // 等新页面加载出来并确认钩子在（最多 15 秒）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if let Ok(v) = cdp.eval(sid, PROBE_JS).await {
            let ssh = v.get("ssh").and_then(|b| b.as_bool()).unwrap_or(false);
            let ready = v.get("ready").and_then(|s| s.as_str()).unwrap_or("");
            if ssh && ready != "loading" {
                return Ok(true);
            }
        }
    }
    Err("刷新后没有检测到钩子".into())
}

/// 关闭 SSH 入口解锁：让所有页面的钩子放行。
pub async fn ssh_unlock_off() -> Result<String, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    let mut n = 0;
    for p in &pages {
        if let Ok(sid) = cdp.attach(&p.id).await {
            if cdp.eval(&sid, SSH_UNLOCK_OFF).await.is_ok() {
                n += 1;
            }
            cdp.detach(&sid).await;
        }
    }
    let mut st = state::inject_load();
    st.ssh = false;
    st.ssh_active = false;
    state::inject_save(&st);
    Ok(format!("已关闭 {} 个页面的 SSH 入口钩子（刷新页面后入口消失）", n))
}

/// 关闭中文后还原界面。
///
/// 翻译是直接改 DOM 文字的，注入脚本的 destroy() 只是断开监听，
/// 不会把字改回去。所以：先尽量还原 + 清标记，再刷新页面。
/// 刷新后不会再被注入 —— 登记的脚本随上一次 CDP 会话结束已经失效，
/// 而配置里的 zh_enabled 在调用前已经关掉，巡检也不会再补。
///
/// `keep_ssh`：SSH 入口开着时，刷新要带上 SSH 钩子，
/// 否则关个中文就把 SSH 入口一起弄没了。
pub async fn restore_zh_ui(keep_ssh: bool) -> Result<String, String> {
    let mut cdp = Cdp::connect().await?;
    let pages = cdp.list_pages().await?;
    if pages.is_empty() {
        return Err("没有找到 Antigravity 页面".into());
    }

    let mut st = state::inject_load();
    let mut n = 0;
    for p in &pages {
        let sid = match cdp.attach(&p.id).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = cdp.send("Page.enable", json!({}), Some(&sid)).await;
        let _ = cdp.eval(&sid, STOP_ZH_RESTORE).await;
        if keep_ssh {
            st.reloaded.insert(p.id.clone(), sys::now_secs());
            state::inject_save(&st);
            if install_ssh_hook(&mut cdp, &sid, true).await.is_err() {
                let _ = cdp.reload(&sid).await;
            }
        } else {
            let _ = cdp.reload(&sid).await;
        }
        n += 1;
        cdp.detach(&sid).await;
    }

    st.zh = false;
    state::inject_save(&st);
    Ok(format!("已还原并刷新 {} 个页面", n))
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
    const BUILTIN_DICT: &str = include_str!("../assets/zh-dict.json");
    const BUILTIN_INJECT: &str = include_str!("../assets/zh-inject.js");
    const BUILTIN_RULES: &str = include_str!("../assets/zh-rules.json");

    let dir = sys::data_dir().join("zh-pack");
    let read_or = |name: &str, builtin: &str| -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_else(|_| builtin.to_string())
    };

    let dict_raw = read_or("dict.json", BUILTIN_DICT);
    let inject = read_or("inject.js", BUILTIN_INJECT);
    let rules_raw = read_or("rules.json", BUILTIN_RULES);

    // dict.json 里可能同时含 DICT 和 RULES。
    // 用户自己放的词库坏了时退回内置版，而不是整个功能失效。
    let parsed = serde_json::from_str::<Value>(&dict_raw)
        .or_else(|_| serde_json::from_str::<Value>(BUILTIN_DICT))
        .ok()?;
    let dict = parsed.get("DICT").cloned().unwrap_or(parsed.clone());
    let rules = parsed.get("RULES").cloned().unwrap_or_else(|| json!({}));

    let rules = if rules.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        serde_json::from_str::<Value>(&rules_raw).unwrap_or_else(|_| json!({}))
    } else {
        rules
    };

    Some((js_json(&dict), js_json(&rules), inject))
}
