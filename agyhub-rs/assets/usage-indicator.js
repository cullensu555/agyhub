/*
 * 上下文与用量指示器（网页版）
 *
 * ⚠️ 这个文件原样注入页面（include_str!），不经过任何模板替换。
 * 改动后必须 `node --check agyhub-rs/assets/usage-indicator.js`。
 *
 * ## 为什么是注入而不是改网关转发
 *
 * 数据（contextWindowUsage / modelUsage）是页面自己 fetch 回来的 gRPC-Web 流，
 * 网关只做透明转发，看不到解出来的字段。要在浏览器里旁路读同一份响应，
 * 最省事也最稳的是包一层 window.fetch。
 *
 * ## 为什么不用 CDP
 *
 * 桌面版注入靠调试端口，2.33.0 起还要带参数启动才开。而这个指示器
 * 手机端也要用，走网关注入的 HTML 里塞 <script> 就行，不依赖调试端口。
 *
 * ## 三个必须守住的约束（都是实测踩出来的）
 *
 * 1. **绝不能阻塞主流程**：页面同时开 6 个常驻流式订阅，
 *    任何一个被拖住，整个界面都会卡。所以一律 res.clone() 后异步读，
 *    原响应立刻返回给前端。
 *
 * 2. **必须跨 chunk 保留残留字节**：一个 gRPC-Web 帧会跨多次 read() 到达
 *    （实测 62 KB 分 11 个 chunk）。解析器要把读不全的尾巴留到下一轮。
 *
 * 3. **数值是字符串**：inputTokens 是 "21580" 而不是 21580，用之前要转数字。
 */
(function () {
  'use strict';

  var KEY = '__AGY_USAGE__';

  // 幂等：重复注入（App 巡检、页面重载）时先卸载旧的
  if (window[KEY] && typeof window[KEY].destroy === 'function') {
    try { window[KEY].destroy(); } catch (e) {}
  }

  var W = window;
  var doc = document;

  // ---- 状态 ----
  var state = {
    used: 0,
    max: 0,
    groups: [],          // [{name, numTokens}]
    snapshots: 0,        // 收到过几次更新，用于判断是否真的在动
    compacted: [],       // 发生过压缩的步数
    conversationId: '',  // 这些数据属于哪个会话（切会话要整份重置）
    model: '',           // 当前模型（解释窗口上限为何不同）
    // 整个会话的累计 token（官方只给每步的，这里按计费口径汇总）
    session: { input: 0, cached: 0, output: 0, thinking: 0, response: 0, steps: 0 },
    counted: {},         // 已累加过的 messageId，防重复
    pending: []          // 还没等到上下文快照的步（其 inputTokens），后续结算缓存
  };

  var destroyed = false;
  var origFetch = W.fetch ? W.fetch.bind(W) : null;
  var renderTimer = null;
  var root = null;      // 我们自己挂的容器
  var face = null;      // 容器里显示圆环+百分比的那一层（会被反复重绘）
  var panel = null;     // 展开的明细面板
  var panelOpen = false;

  // ---------------------------------------------------------------
  // gRPC-Web 帧解析
  //
  // 帧格式：1 字节 flag（0x00 数据 / 0x80 trailer）+ 4 字节大端长度 + 载荷。
  // 只看 flag=0 的数据帧，载荷是 JSON（gRPC-Web+json）。
  // ---------------------------------------------------------------
  function Framer() {
    this.rest = new Uint8Array(0);
  }

  Framer.prototype.push = function (chunk, onJson) {
    var buf;
    if (this.rest.length) {
      buf = new Uint8Array(this.rest.length + chunk.length);
      buf.set(this.rest, 0);
      buf.set(chunk, this.rest.length);
    } else {
      buf = chunk;
    }
    var pos = 0;
    while (pos + 5 <= buf.length) {
      var flag = buf[pos];
      var len = ((buf[pos + 1] << 24) | (buf[pos + 2] << 16) |
                 (buf[pos + 3] << 8) | buf[pos + 4]) >>> 0;
      var end = pos + 5 + len;
      if (end > buf.length) { break; }   // 半帧：留到下一轮
      if (flag === 0x00 && len > 0) {
        try {
          var txt = new TextDecoder('utf-8', { fatal: true })
            .decode(buf.subarray(pos + 5, end)).trim();
          if (txt && (txt.charAt(0) === '{' || txt.charAt(0) === '[')) {
            onJson(JSON.parse(txt));
          }
        } catch (e) {
          // 不是 JSON 帧（比如二进制 protobuf），忽略
        }
      }
      pos = end;
    }
    this.rest = buf.subarray(pos);
  };

  // ---------------------------------------------------------------
  // 从一帧 update 里抽取用量信息
  // ---------------------------------------------------------------
  function num(v) {
    var n = Number(v);
    return isFinite(n) ? n : 0;
  }

  /// 清空用量数据（切换会话 / 回到首页时用）。
  function resetUsage() {
    state.used = 0;
    state.max = 0;
    state.groups = [];
    state.compacted = [];
    state.conversationId = '';
    state.model = '';
    state.session = { input: 0, cached: 0, output: 0, thinking: 0, response: 0, steps: 0 };
    state.counted = {};
    state.pending = [];
  }

  function absorbUpdate(u) {
    if (!u) { return; }
    var changed = false;

    // ⚠️ contextWindowUsage 是**按会话**存的（和 conversationId 同级）。
    // 切到别的会话时必须整份重置，否则会把上一个会话的数字当成当前会话的，
    // 表现为「刚打开一个新对话就显示 4.2%」（用户实测反馈过）。
    var cid = u.conversationId || '';
    if (cid && state.conversationId && cid !== state.conversationId) {
      resetUsage();
    }
    if (cid) { state.conversationId = cid; }

    if (u.contextWindowUsage) {
      var c = u.contextWindowUsage;
      state.used = num(c.estimatedTokensUsed);
      state.max = num(c.maxContextTokens);
      state.groups = (c.tokenGroups || []).map(function (g) {
        return { name: g.name || '', tokens: num(g.numTokens), type: g.type || '' };
      });
      changed = true;
    }

    // 会话累计（按计费口径汇总整段会话）。
    //
    // ⚠️ 必须去重：stepsUpdate 多次下发会**重复包含**同一步
    // （实测同一帧里能拿到已经统计过的 step），不去重会越滚越大。
    // 用 messageId 做键（每步唯一，形如 bot-<uuid>）。
    //
    // ## 三个数的口径（都和账单对得上）
    //
    //   · 输入（未命中缓存）= 各步 inputTokens 之和。
    //     实测它就是"本步真正新喂进去的 token"，命中隐式缓存的部分不算。
    //   · 输出 = 各步 outputTokens 之和（= 思考 + 回复）。
    //   · 缓存读取 = 各步 cacheReadTokens 之和（官方字段，见下）。
    //
    // ## 缓存读取：官方有字段，别再推算（2026-11-23 纠正）
    //
    // `modelUsage` 是 `exa.codeium_common_pb.ModelUsageStats`，proto 字段号：
    //   2=input 3=output 4=cacheWrite 5=cacheRead 9=thinking 10=response。
    // **field 5 就是缓存读取的精确值**，之前只解析了 2/3/9/10，把 4/5 漏了。
    //
    // 实抓验证（网页版流里的 protojson，键名 camelCase）：
    //   首步 input=21648，字段里**没有** cacheReadTokens（缓存为 0）
    //   第二轮 input=2160，**有** cacheReadTokens=19591，且 2160+19591≈21648
    //   → 它是 `omitempty`：**缓存为 0 时键不出现**，必须 `|| 0` 兜住。
    //
    // ⚠️ 以前的算法是 Σ(上下文总量 − 该步 inputTokens)，问题是：
    //   ① 上下文压缩时（实测见过 4 次，缓存读数直接归零）那一步会算出负数，
    //      被 `frameCtx > inp` 挡掉 → **这一步的缓存量静默丢失，再也补不回来**；
    //   ② frameCtx 取自"当帧 contextWindowUsage"，而 steps 常常先于 cw 到达，
    //      挂起队列里的多步会共用**同一个** cw 结算，上下文不一致时集体算错。
    //   现在优先用官方字段，只有字段缺失（首步/缓存真的是 0）才退回推算。
    var steps = (u.mainTrajectoryUpdate &&
                 u.mainTrajectoryUpdate.stepsUpdate &&
                 u.mainTrajectoryUpdate.stepsUpdate.steps) || [];
    // 这一帧带的上下文总量（可能为空，那就沿用上一次已知值）
    var frameCtx = u.contextWindowUsage
      ? num(u.contextWindowUsage.estimatedTokensUsed) : 0;

    // 有新的上下文快照 → 先给之前挂起的步结算
    if (frameCtx > 0 && state.pending.length) {
      for (var q = 0; q < state.pending.length; q++) {
        if (frameCtx > state.pending[q]) {
          state.session.cached += (frameCtx - state.pending[q]);
        }
      }
      state.pending = [];
      changed = true;
    }

    if (steps.length) {
      for (var s2 = 0; s2 < steps.length; s2++) {
        var st2 = steps[s2];
        var mu2 = st2 && st2.metadata && st2.metadata.modelUsage;
        if (!mu2 || !mu2.messageId) { continue; }
        if (state.counted[mu2.messageId]) { continue; }
        state.counted[mu2.messageId] = 1;

        var inp = num(mu2.inputTokens);
        state.session.input += inp;
        state.session.output += num(mu2.outputTokens);
        state.session.thinking += num(mu2.thinkingOutputTokens);
        state.session.response += num(mu2.responseOutputTokens);
        state.session.steps += 1;

        // 缓存读取：官方字段优先（精确，压缩归零也天然正确）
        // 字段是 omitempty，缓存为 0 时整个键不存在 → `|| 0` 兜住。
        // 判据用 `!= null` 而不是 `> 0`：值真的是 0 时（首步、压缩后）
        // 说明官方明确告诉"这步没有缓存读取"，不能再去推算，
        // 否则又会把"上下文 − 输入"当成缓存算进来。
        var cr = mu2.cacheReadTokens;
        if (cr !== undefined && cr !== null) {
          state.session.cached += num(cr);
        } else if (frameCtx > 0) {
          // 老版本/异常情况没有该字段 → 保留原推算作兜底
          if (frameCtx > inp) { state.session.cached += (frameCtx - inp); }
        } else {
          state.pending.push(inp);
        }
        changed = true;
      }
    }

    // 当前模型名：用来解释「为什么这次的窗口上限和上次不一样」。
    // 模型选择器里那个 MODEL_PLACEHOLDER_M4xx 不好读，
    // `plannerConfig.modelName` 才是 "gemini-4-carbon-high" 这种可读名。
    var gens = (u.mainTrajectoryUpdate &&
                u.mainTrajectoryUpdate.generatorMetadatasUpdate &&
                u.mainTrajectoryUpdate.generatorMetadatasUpdate.generatorMetadatas) || [];
    for (var k = gens.length - 1; k >= 0; k--) {
      var pcfg = gens[k] && gens[k].plannerConfig;
      if (pcfg && pcfg.modelName) {
        if (state.model !== pcfg.modelName) {
          state.model = pcfg.modelName;
          changed = true;
        }
        break;
      }
    }

    var cp = u.compactionInfo;
    if (cp && cp.compactedAtStepIndices && cp.compactedAtStepIndices.length) {
      state.compacted = cp.compactedAtStepIndices.slice(0, 8);
      changed = true;
    }

    if (changed) {
      state.snapshots++;
      scheduleRender();
    }
  }

  // ---------------------------------------------------------------
  // 旁路挂到 fetch 上
  // ---------------------------------------------------------------
  function wrapFetch() {
    if (!origFetch) { return; }
    W.fetch = function (input, init) {
      var p = origFetch.apply(this, arguments);
      try {
        var url = (typeof input === 'string') ? input
          : (input && input.url) ? input.url : '';
        if (url.indexOf('StreamAgentStateUpdates') < 0) { return p; }
        p.then(function (res) {
          // clone 之后立刻返回，原流不受影响
          var clone;
          try { clone = res.clone(); } catch (e) { return; }
          if (!clone.body || !clone.body.getReader) { return; }
          var rd = clone.body.getReader();
          var framer = new Framer();
          (function pump() {
            if (destroyed) { try { rd.cancel(); } catch (e) {} return; }
            rd.read().then(function (r) {
              if (r.done) { return; }
              try {
                framer.push(r.value, function (obj) {
                  if (obj && obj.update) { absorbUpdate(obj.update); }
                });
              } catch (e) {}
              pump();
            }).catch(function () { /* 流被取消是正常的 */ });
          })();
        }).catch(function () {});
      } catch (e) {}
      return p;
    };
  }

  // ---------------------------------------------------------------
  // 格式化
  // ---------------------------------------------------------------
  // 分组显示名。
  //
  // 为什么不用后端给的 `name`：本地中文注入会把其中一部分翻掉
  // （Rules→规则、Skills→技能），另一部分（Tools、System Prompt）不翻，
  // 结果同一张面板里中英混杂（实测踩过）。这里按 `type` 统一映射，
  // 保证整块面板语言一致；遇到没见过的类型才回落到原名。
  var GROUP_NAMES = {
    TOKEN_TYPE_SYSTEM_PROMPT: '系统提示',
    TOKEN_TYPE_SUBAGENTS: '子智能体',
    TOKEN_TYPE_SKILLS: '技能',
    TOKEN_TYPE_PLUGINS: '插件',
    TOKEN_TYPE_RULES: '规则',
    TOKEN_TYPE_TOOLS: '工具',
    TOKEN_TYPE_CHAT_MESSAGES: '对话消息',
    TOKEN_TYPE_CONTEXT: '附加上下文',
    TOKEN_TYPE_MCP_TOOLS: 'MCP 工具'
  };

  function groupName(g) {
    return GROUP_NAMES[g.type] || g.name || '其他';
  }

  // 进制转换：token 数一般 4~7 位，k 比完整数字好读，但明细面板里保留完整值
  function fmt(n) {
    if (!n) { return '0'; }
    if (n >= 1000000) { return (n / 1000000).toFixed(2).replace(/\.?0+$/, '') + 'M'; }
    if (n >= 1000) { return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k'; }
    return String(Math.round(n));
  }

  function pct() {
    if (!state.max) { return 0; }
    return Math.min(100, Math.max(0, (state.used / state.max) * 100));
  }

  function toneColor(p) {
    // 容量语义：越满越危险（与股市涨跌配色无关）
    if (p >= 80) { return '#e5484d'; }
    if (p >= 50) { return '#f5a524'; }
    return '#30a46c';
  }

  // ---------------------------------------------------------------
  // 价格换算
  //
  // 数据来自 <script id="agy-pricing"> 里的 pricing.json（网关注入）。
  // 只是「按 Gemini API 公开价换算」的参考值 —— Antigravity 本身是订阅制，
  // 这些数字不等于实际账单，所以要明确标注。
  // ---------------------------------------------------------------
  var PRICING = null;

  function loadPricing() {
    if (PRICING) { return PRICING; }
    var el = doc.getElementById('agy-pricing');
    if (!el) { return null; }
    try {
      PRICING = JSON.parse(el.textContent || el.innerText || '{}');
    } catch (e) {
      PRICING = null;
    }
    return PRICING;
  }

  /// 找出当前模型对应的费率。
  ///
  /// `state.model` 是 `plannerConfig.modelName`，形如 "gemini-4-argon-high"，
  /// 而价格表里的 id 是 "gemini-4-argon"。所以用「前缀匹配 + 最长优先」，
  /// 避免 gemini-4-argon 被 gemini-4 之类误配。
  function findRate(modelId) {
    var p = loadPricing();
    if (!p || !p.models || !modelId) { return null; }
    var m = String(modelId).toLowerCase();
    var best = null;
    for (var i = 0; i < p.models.length; i++) {
      var cand = p.models[i];
      var id = String(cand.id).toLowerCase();
      if (m === id || m.indexOf(id) === 0) {
        if (!best || id.length > String(best.id).length) { best = cand; }
      }
    }
    return best ? { rate: best, unit: p.unit || 1000000 } : null;
  }

  /// 现在是否还在促销期（promoUntil 之前）。
  function inPromo(rate) {
    if (!rate || !rate.promoUntil) { return false; }
    // 截止日当天仍算促销期，所以用「当日 23:59:59」比较
    var end = new Date(rate.promoUntil + 'T23:59:59');
    return Date.now() <= end.getTime();
  }

  /// 某项费用是否已经生效（如缓存写入 2027-01-12 才收费）。
  /// 未到生效日 → 现在按 0 计，但要告诉用户"以后会收"。
  function notYetCharged(fromDate) {
    if (!fromDate) { return false; }
    var d = new Date(fromDate + 'T00:00:00');
    return Date.now() < d.getTime();
  }

  /// 算钱。
  ///
  /// 支持官方这几种情况：
  ///  · 促销（promoUntil + regular）：Carbon/Argon 是「净支出 50% 赠金返还」，
  ///    单价本身不变，所以这里按单价算钱，另外标出「有返还」；
  ///    Flash 系列是真的打折（单价直接低），到期后按 regular 涨回去。
  ///  · 缓存写入（cacheWrite + cacheWriteFrom）：只有 Neon/Hydrogen 有，
  ///    且 2027-01-12 才生效，在那之前不计费。
  ///  · 长上下文档（longContext）：3.1 Pro 输入 >20 万 token 时全部按高档计。
  ///
  /// 没有费率时返回 null（UI 就不显示价格）。
  function costOf(ss) {
    var found = findRate(state.model);
    if (!found) { return null; }
    var r = found.rate, unit = found.unit;
    var promo = inPromo(r);

    // 促销期用当前价，过期用 regular。注意 Carbon/Argon 的「返还」不改变单价，
    // 所以它们的 input/output 本来就是原价，regular 只是到期后的新价。
    var cur = promo ? r : (r.regular || r);

    // 长上下文：输入超过阈值就整体按高档费率
    var lc = r.longContext;
    if (lc && ss.input > lc.threshold) {
      cur = { input: lc.input, cache: lc.cache, output: lc.output };
    }

    function calc(rate) {
      var ci = ss.input / unit * rate.input;
      var cc = ss.cached / unit * (rate.cache || 0);
      var co = ss.output / unit * rate.output;
      // 缓存写入：不计入金额。
      // 原因：官方不下发写入量，拿读取量代进去会算出一个假数字。
      // 未到生效日（2027-01-12）本来就该是 0；生效后也仍然无从统计，
      // 与其编一个数，不如在 UI 上明说"未提供"，金额留空。
      return { input: ci, cache: cc, output: co, cacheWrite: 0,
               total: ci + cc + co };
    }

    var after = r.regular ? calc(r.regular) : null;
    // 实际生效的单价（可能已被长上下文覆盖）。面板里展示它，
    // 用户拿「token × 单价」能自己核对上面的金额。
    var rateForShow = {
      input: cur.input,
      cache: cur.cache || 0,
      output: cur.output,
      cacheWrite: r.cacheWrite || 0,
      longCtx: (lc && ss.input > lc.threshold) ? null : (lc || null)
    };
    return {
      now: calc(cur),
      after: after,
      rate: rateForShow,
      // 原价单价（写在提示里说明"涨的是单价，不是总额"）
      afterUnit: r.regular ? {
        input: r.regular.input.toFixed(2),
        cache: r.regular.cache.toFixed(3),
        output: r.regular.output.toFixed(2)
      } : null,
      promo: promo,
      until: r.promoUntil || '',
      // 官方原话「净支出 50% 赠金返还」—— 钱照收，返额度。所以不能对半砍显示。
      rebate: promo && /赠金返还/.test(r._promoNote || '') ? 0.5 : 0,
      // 缓存写入什么时候开始收（有这科目且还没生效时提示）
      cacheWriteFrom: r.cacheWrite ? (r.cacheWriteFrom || '') : '',
      cacheWritePending: !!(r.cacheWrite && notYetCharged(r.cacheWriteFrom)),
      cacheWriteRate: r.cacheWrite || 0,
      currency: (loadPricing() || {}).currency || 'USD'
    };
  }

  /// 美元金额格式化：小额要足够精度，否则全显示 $0.00 没意义。
  function money(v) {
    if (!isFinite(v)) { return '-' }
    if (v >= 1) { return '$' + v.toFixed(2); }
    if (v >= 0.01) { return '$' + v.toFixed(3); }
    if (v > 0) { return '$' + v.toFixed(4); }
    return '$0';
  }

  // ---------------------------------------------------------------
  // 渲染
  // ---------------------------------------------------------------
  var CSS = [
    '#agy-usage{position:relative;display:inline-flex;align-items:center;',
    'justify-content:center;flex:none;',
    'margin-right:2px;font:12px/1 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;',
    'cursor:pointer;user-select:none;-webkit-user-select:none;',
    'padding:3px 5px;border-radius:8px;color:var(--agy-usage-fg,#5f6368);',
    'background:transparent;transition:background .12s;white-space:nowrap;',
    // ⚠️ 页面全局样式会给 [role="button"] / [tabindex] 加边框和 outline，
    // 在我们这个小图标上就变成一个难看的方框（实测踩过）。必须强制清掉。
    'border:0 !important;outline:0 !important;box-shadow:none !important;',
    'appearance:none;-webkit-appearance:none}',
    '#agy-usage:focus{outline:0 !important;box-shadow:none !important}',
    '#agy-usage:hover{background:rgba(128,128,128,.12)}',
    // flex:none 很关键：父容器是 flex，SVG 会被压缩变形，圆环会画成方框。
    // ⚠️ box-shadow 必须强制 none：页面全局给 svg 加了
    // `rgb(95,99,104) 0 0 0 1px` 的投影，在我们这个环外画出一个难看的方框（实测踩过）。
    '#agy-usage .ring{display:block;flex:none;width:14px;height:14px;overflow:visible;',
    'border:0 !important;outline:0 !important;box-shadow:none !important;',
    'background:transparent !important}',
    '#agy-usage .txt{margin-left:3px;font-variant-numeric:tabular-nums;',
    'letter-spacing:-.2px;line-height:1}',
    // 明细面板用 fixed 定位，按指示器自身位置算 ——
    // 输入框底栏有 overflow 和层叠上下文，absolute 会被裁掉。
    '#agy-usage-panel{position:fixed;z-index:2147483000;',
    'width:268px;padding:12px 14px;border-radius:12px;',
    'background:var(--agy-usage-bg,#fff);color:var(--agy-usage-fg,#1d1d1f);',
    'box-shadow:0 8px 32px rgba(0,0,0,.16),0 0 0 1px rgba(0,0,0,.06);',
    'font:12px/1.5 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;',
    'display:none;text-align:left}',
    '#agy-usage-panel.on{display:block}',
    '#agy-usage-panel h4{margin:0 0 2px;font-size:12px;font-weight:600}',
    '#agy-usage-panel .sub{opacity:.6;margin-bottom:8px;font-variant-numeric:tabular-nums}',
    '#agy-usage-panel .bar{height:6px;border-radius:3px;background:rgba(128,128,128,.2);',
    'overflow:hidden;display:flex;margin-bottom:10px}',
    '#agy-usage-panel .bar i{display:block;height:100%;opacity:.85}',
    '#agy-usage-panel .row{display:flex;align-items:baseline;gap:6px;',
    'padding:2px 0;font-variant-numeric:tabular-nums}',
    '#agy-usage-panel .row .sw{width:7px;height:7px;border-radius:2px;flex:none}',
    '#agy-usage-panel .row .nm{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}',
    '#agy-usage-panel .row .vv{opacity:.8}',
    '#agy-usage-panel .row .pp{opacity:.5;width:34px;text-align:right;flex:none}',
    '#agy-usage-panel .sep{height:1px;background:rgba(128,128,128,.18);margin:9px 0}',
    '#agy-usage-panel .kv{display:flex;align-items:baseline;padding:1px 0;',
    'font-variant-numeric:tabular-nums}',
    // 三列定宽布局：名字左、token 右对齐、金额右对齐。
    // token 列必须给固定宽度 + text-align:right，否则
    // 8,448,807 和 8,086 这种位数差异会让整列参差不齐（用户明确要求对齐）。
    '#agy-usage-panel .kv .label{flex:1 1 auto;min-width:0}',
    '#agy-usage-panel .kv .tok{flex:none;width:82px;text-align:right;font-weight:600;',
    'font-variant-numeric:tabular-nums}',
    '#agy-usage-panel .kv b{font-weight:600}',
    '#agy-usage-panel .money{color:var(--agy-usage-dim,#6b7280);flex:none;',
    'width:62px;text-align:right;margin-left:10px;font-variant-numeric:tabular-nums}',
    // 合计行：加重 + 上分隔线
    '#agy-usage-panel .kv.total{margin-top:6px;padding-top:6px;',
    'border-top:1px solid rgba(128,128,128,.18);font-weight:600}',
    // 折扣标记：橙色小标签，提醒"这个价会涨"
    '#agy-usage-panel .badge{display:inline-block;margin-top:7px;padding:2px 8px;',
    'border-radius:999px;font-size:11px;line-height:1.5;',
    'background:rgba(245,165,36,.16);color:#b45309;font-weight:500}',
    '#agy-usage-panel .note{opacity:.55;margin-top:8px;font-size:11px;line-height:1.45}',
    // 说明文字里要强调关键数字时用（note 整体是半透明的，里面要提亮）
    '#agy-usage-panel .note .hl{opacity:1;font-weight:600;',
    'color:var(--agy-usage-fg,#1d1d1f)}'
  ].join('');

  // 分组配色：按占比从大到小取色，保证视觉上主次分明
  var PALETTE = ['#5b8def', '#8b5cf6', '#ec4899', '#f59e0b',
                 '#10b981', '#06b6d4', '#94a3b8'];

  function ensureStyle() {
    if (doc.getElementById('agy-usage-style')) { return; }
    var st = doc.createElement('style');
    st.id = 'agy-usage-style';
    st.textContent = CSS;
    (doc.head || doc.documentElement).appendChild(st);
  }

  // 发送按钮：尽量贴近它，贴不上就退到输入框上方
  //
  // 实测的 DOM 结构（2.33.0）：
  //   BUTTON[data-testid=send-button]            ← 28×28 的圆按钮
  //     └ DIV.flex                                 ← 只包按钮的壳
  //         └ DIV.flex.items-center.gap-1.5        ← 按钮组（权限选择器等也在这层）
  //             └ DIV.agy-composer-footer          ← 整条底栏，左侧是模型选择器
  //
  // 所以插到 `gap-1.5` 那一层的**最前面**，就会紧贴在按钮组左边，
  // 且和它们一起右对齐、垂直居中 —— 不需要自己算位置。
  //
  // ⚠️ agent 运行中时 send-button 会**换成停止按钮** `input-cancel-button`
  // （源码里 `w ? <Y1c/> : <send-button>`）。只认 send-button 的话，
  // 一运行就找不到锚点，兜底去抓 textarea，结果挂进 AskQuestion 卡片里
  // 「Other (write your answer)」那一行（用户实测反馈过）。
  // 所以两个 testid 都要认。
  var BUTTON_IDS = ['[data-testid="send-button"]',
                    '[data-testid="input-cancel-button"]',
                    '[data-testid="send-button-loading"]'];

  // AskQuestion 卡片的输入框，绝不能当成"主输入框"的兜底锚点
  var ASK_SEL = '[data-testid="ask-question-writein"], ' +
                '[data-testid^="ask-question"]';

  function composerButton() {
    for (var i = 0; i < BUTTON_IDS.length; i++) {
      var b = doc.querySelector(BUTTON_IDS[i]);
      if (b) { return b; }
    }
    return null;
  }

  // 页面主输入框（排除 AskQuestion 卡片里的那些）
  function mainEditor() {
    var list = doc.querySelectorAll('[contenteditable="true"], textarea');
    for (var i = 0; i < list.length; i++) {
      var el = list[i];
      if (el.closest && el.closest(ASK_SEL)) { continue; }
      // 题卡里的 textarea 通常很短、又在对话区里
      if (el.closest && el.closest('[data-testid^="ask-question"]')) { continue; }
      return el;
    }
    return null;
  }

  function findAnchor() {
    var btn = composerButton();
    if (btn) {
      var shell = btn.parentElement;            // lvl1
      var group = shell && shell.parentElement;  // lvl2
      if (group) { return { node: group, before: group.firstChild }; }
      if (shell) { return { node: shell, before: btn }; }
    }
    var ed = mainEditor();
    if (ed) {
      var box = ed.closest('form') || ed.parentElement;
      if (box && box.parentElement) { return { node: box.parentElement, before: box }; }
    }
    return null;
  }

  function buildRing(p) {
    var C = 2 * Math.PI * 6.5;
    var off = C - (p / 100) * C;
    var color = toneColor(p);
    return '<svg class="ring" width="14" height="14" viewBox="0 0 16 16" ' +
      'style="width:14px;height:14px;flex:none">' +
      '<circle cx="8" cy="8" r="6.5" fill="none" ' +
      'stroke="rgba(128,128,128,.28)" stroke-width="2.5"></circle>' +
      '<circle cx="8" cy="8" r="6.5" fill="none" stroke="' + color + '" ' +
      'stroke-width="2.5" stroke-dasharray="' + C.toFixed(2) + '" ' +
      'stroke-dashoffset="' + off.toFixed(2) + '" stroke-linecap="round" ' +
      'transform="rotate(-90 8 8)"></circle></svg>';
  }

  function renderButton() {
    if (!root || !face) { return; }
    // 没有数据时显示 `--`，不要显示 0.0%。
    // 0.0% 会让人以为"已经统计过了，占用是 0"，其实是"还没有会话在跑"。
    if (!state.max) {
      face.innerHTML = buildRing(0) + '<span class="txt">--</span>';
      root.title = '还没有用量数据，在会话里发一条消息后出现';
      return;
    }
    var p = pct();
    // ⚠️ 只能改 face 的 innerHTML，绝不能碰 root。
    // 以前写的是 root.innerHTML = ...，每次都把挂在 root 下的明细面板一起清掉，
    // 表现为「点开面板什么都没发生」。
    face.innerHTML = buildRing(p) +
      '<span class="txt">' + (p < 10 ? p.toFixed(1) : Math.round(p)) + '%</span>';
    root.title = '上下文 ' + state.used.toLocaleString() + ' / ' +
                 state.max.toLocaleString() + '（点击看明细）';
  }

  function renderPanel() {
    if (!panel) { return; }
    var p = pct();
    var total = state.used || 1;
    var groups = state.groups.slice().sort(function (a, b) {
      return b.tokens - a.tokens;
    });

    // 没有数据：一句话说明即可
    if (!state.max) {
      panel.innerHTML = '<h4>上下文占用</h4>' +
        '<div class="note" style="margin-top:6px">发一条消息后才有数据。</div>';
      return;
    }

    var h = '';
    h += '<h4>上下文占用</h4>';
    h += '<div class="sub">' + state.used.toLocaleString() + ' / ' +
         state.max.toLocaleString() + ' · ' +
         (p < 10 ? p.toFixed(1) : Math.round(p)) + '%</div>';
    // 模型名放在副标题下面：换模型时窗口上限会跟着变（128K/512K/1M），
    // 看到模型就知道为什么这一次的上限和上一次不一样。
    if (state.model) {
      h += '<div class="sub" style="margin-top:-6px">模型 ' +
           escapeHtml(state.model) + '</div>';
    }

    // 堆叠条
    h += '<div class="bar">';
    for (var i = 0; i < groups.length; i++) {
      var w = (groups[i].tokens / total) * 100;
      h += '<i style="width:' + w.toFixed(2) + '%;background:' +
           PALETTE[i % PALETTE.length] + '"></i>';
    }
    // 剩余空间用极淡底色表示（bar 自身背景）
    h += '</div>';

    if (groups.length) {
      for (var j = 0; j < groups.length; j++) {
        var g = groups[j];
        var gp = (g.tokens / total) * 100;
        h += '<div class="row">' +
          '<span class="sw" style="background:' + PALETTE[j % PALETTE.length] + '"></span>' +
          '<span class="nm">' + escapeHtml(groupName(g)) + '</span>' +
          '<span class="vv">' + g.tokens.toLocaleString() + '</span>' +
          '<span class="pp">' + gp.toFixed(0) + '%</span>' +
          '</div>';
      }
    } else {
      h += '<div class="row"><span class="nm" style="opacity:.6">还没有数据（发一条消息后出现）</span></div>';
    }

    // ---- 本会话 token + 估算费用（按计费口径罗列）----
    //
    // 用户要的是「在这个会话里一共花了多少」，不是每一步的明细，
    // 所以这里只给累计值，并且照账单的科目排：
    //   输入 / 缓存读取 / 缓存写入 / 输出 / 合计
    // 每一行右边同时给 token 数和按公开价换算的金额（能算出来才显示）。
    var ss = state.session;
    if (ss.steps > 0) {
      var cost = costOf(ss);
      h += '<div class="sep"></div>';
      h += '<div class="sub" style="margin-bottom:6px">本会话 token（' +
           ss.steps + ' 步）</div>';

      // 一行 = 名字 / token / 金额。
      // token 和金额都用固定的右对齐列，否则长短数字一多就参差不齐。
      function line(name, tokens, amount) {
        var amt = amount === null || amount === undefined
          ? '<span class="money"></span>'
          : '<span class="money">' + money(amount) + '</span>';
        return '<div class="kv"><span class="label">' + name + '</span>' +
          '<b class="tok">' + tokens.toLocaleString() + '</b>' + amt + '</div>';
      }

      h += line('输入', ss.input, cost ? cost.now.input : null);
      // 「缓存读取」是官方原词（命中缓存后按低价计的输入）
      h += line('缓存读取', ss.cached, cost ? cost.now.cache : null);
      // 缓存写入：只有 Neon/Hydrogen 有，2027-01-12 生效。
      // ⚠️ proto 里确实有 `cacheWriteTokens`（field 4），但实测**恒为 0**
      // （web 流和 CLI 两条通道的抓包都没出现过该键）→ 官方没有真正下发写入量。
      // 绝不能拿 cache 读取量顶替 —— 会显示出两行相同的假数字。只能标「—」。
      if (cost && cost.cacheWriteFrom) {
        h += '<div class="kv"><span class="label">缓存写入</span>' +
             '<b class="tok" style="font-weight:400;opacity:.35">—</b>' +
             '<span class="money"></span></div>';
      }
      h += line('输出', ss.output, cost ? cost.now.output : null);

      // 合计 = 输入 + 缓存读取 + 输出
      var sum = ss.input + ss.cached + ss.output;
      h += '<div class="kv total"><span class="label">合计</span>' +
           '<b class="tok">' + sum.toLocaleString() + '</b>' +
           (cost ? '<span class="money">' + money(cost.now.total) + '</span>'
                 : '<span class="money"></span>') + '</div>';

      // 当前模型单价：让上面的金额可追溯 —— 用户能自己乘一遍核对。
      // 单位统一为「每百万 token」，和官方定价页一致。
      // 用 price() 而不是 money()：单价永远是完整的 2/3 位小数，
      // 而 money() 会给小额显示 4 位（$0.0750），放在单价表里反而不齐。
      if (cost && cost.rate) {
        // 单价格式化：一律保证「同列位数一致」才好对齐。
        //  <1  → 3 位小数（$0.100 / $0.075 / $0.025）
        //  ≥1  → 2 位小数；若第三位有意义（如 1.875）则保留原值
        function price(v) {
          var n = Number(v);
          if (n < 1) { return '$' + n.toFixed(3); }
          var two = Math.round(n * 100) / 100;
          return Math.abs(n - two) < 1e-9 ? '$' + n.toFixed(2) : '$' + n;
        }
        // 单价行：复用 .kv 的三列骨架，但右侧用 .money 位置放单价，
        // token 列留空 —— 这样单价的右边缘和上面金额列的右边缘对齐。
        function priceLine(name, v) {
          return '<div class="kv"><span class="label" style="opacity:.8">' + name +
                 '</span><b class="tok"></b><span class="money">' +
                 price(v) + '</span></div>';
        }
        h += '<div class="sub" style="margin-top:9px">当前单价（每百万）</div>';
        h += priceLine('输入', cost.rate.input);
        h += priceLine('缓存读取', cost.rate.cache);
        h += priceLine('输出', cost.rate.output);
        if (cost.rate.cacheWrite) {
          h += priceLine('缓存写入', cost.rate.cacheWrite);
        }
        if (cost.rate.longCtx) {
          h += '<div class="note">输入超 ' +
               (cost.rate.longCtx.threshold / 10000) + ' 万时整体按 ' +
               price(cost.rate.longCtx.input) + ' / ' +
               price(cost.rate.longCtx.cache) + ' / ' +
               price(cost.rate.longCtx.output) + ' 计。</div>';
        }
      }

      // 促销/涨价：一行给结论。
      // $10.32 = 按当前单价开票额（$2/$10 本身就是促销费率）；
      // 50% 赠金另返，实付一半；12-01 起单价翻倍且不再返还。
      if (cost && cost.promo && cost.until) {
        if (cost.rebate > 0) {
          var net = cost.now.total * (1 - cost.rebate);
          var tail = cost.after ? ' · 12-01 起 ' + money(cost.after.total) : '';
          h += '<div class="badge">实付约 ' + money(net) + '（返 50%）' +
               tail + '</div>';
        } else if (cost.after) {
          h += '<div class="badge">促销价 · 之后约 ' +
               money(cost.after.total) + '</div>';
        }
      }
      if (cost && cost.cacheWriteFrom) {
        h += '<div class="note">缓存写入 ' + cost.cacheWriteFrom + ' 起按 $' +
             (cost.cacheWriteRate || 0).toFixed(3) + '/百万计费，官方暂不下发用量。</div>';
      }
      if (!cost && state.model) {
        h += '<div class="note">价格表无 ' + escapeHtml(state.model) + '</div>';
      }
    }

    if (state.compacted.length) {
      h += '<div class="note">上下文压缩过 ' + state.compacted.length + ' 次。</div>';
    }

    // 一句免责：这些金额是按公开价换算的估算，不是真实账单
    h += '<div class="note">按公开价估算，非实际账单。</div>';

    panel.innerHTML = h;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function scheduleRender() {
    if (renderTimer || destroyed) { return; }
    renderTimer = setTimeout(function () {
      renderTimer = null;
      renderButton();
      if (panelOpen) { renderPanel(); placePanel(); }
    }, 120);
  }

  function mount() {
    if (destroyed) { return; }
    // ⚠️ 这里**不能**因为"已经在 DOM 里"就提前返回 ——
    // 那样位置错了也永远纠正不过来（首次挂载常落在输入框兜底位置）。
    // 真正的幂等由下面 insertBefore 前的判断保证。

    var a = findAnchor();
    if (!a || !a.node) { return; }

    ensureStyle();

    if (!root) {
      root = doc.createElement('div');
      root.id = 'agy-usage';
      // 不设 role="button"：页面全局样式会按按钮给它套边框（实测变成方框）。
      root.setAttribute('tabindex', '0');
      root.title = '上下文与用量';

      face = doc.createElement('span');
      face.className = 'face';
      face.style.cssText = 'display:inline-flex;align-items:center;flex:none';
      root.appendChild(face);

      panel = doc.createElement('div');
      panel.id = 'agy-usage-panel';
      // ⚠️ 面板挂到 body 而不是 root 里：
      // 输入框底栏有 overflow/transform，会把 fixed 变成相对祖先定位，面板会被裁掉。
      (doc.body || doc.documentElement).appendChild(panel);

      // 关键：按键/点击的监听挂在 root 上，但面板内的点击不能触发 toggle，
      // 也不能冒泡出去让 document 的"点别处收起"立刻又把它关掉。
      root.addEventListener('click', function (ev) {
        ev.stopPropagation();
        if (panel && panel.contains(ev.target)) { return; }
        togglePanel();
      });
      root.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' || ev.key === ' ') {
          ev.preventDefault();
          ev.stopPropagation();
          togglePanel();
        }
      });
      panel.addEventListener('click', function (ev) { ev.stopPropagation(); });

      // 点别处收起
      doc.addEventListener('click', onDocClick, true);
      doc.addEventListener('keydown', onKeyDown, true);
    }

    // 每次重新插入（React 重渲染会把我们摘掉）
    try {
      if (a.before && a.before.parentNode === a.node) {
        a.node.insertBefore(root, a.before);
      } else {
        a.node.insertBefore(root, a.node.firstChild);
      }
    } catch (e) {
      try { a.node.appendChild(root); } catch (e2) { return; }
    }

    // 深色模式跟随页面。
    // ⚠️ 两处都要设：面板挂的是 document.body，不是 root 里的节点，
    // 只设在 root 上的 CSS 变量面板继承不到（会一直白底黑字）。
    try {
      var dark = W.matchMedia &&
        W.matchMedia('(prefers-color-scheme: dark)').matches;
      [root, panel].forEach(function (el) {
        if (!el || !el.style) { return; }
        if (dark) {
          el.style.setProperty('--agy-usage-bg', '#2a2a28');
          el.style.setProperty('--agy-usage-fg', '#e8e8e6');
          el.style.setProperty('--agy-usage-dim', '#a1a1a0');
        } else {
          el.style.removeProperty('--agy-usage-bg');
          el.style.removeProperty('--agy-usage-fg');
          el.style.removeProperty('--agy-usage-dim');
        }
      });
    } catch (e) {}

    renderButton();
    if (panelOpen) { renderPanel(); placePanel(); }
  }

  function togglePanel() {
    if (panelOpen) { closePanel(); return; }

    panelOpen = true;
    if (panel && panel.classList) { panel.classList.add('on'); }
    // ⚠️ 顺序很关键：先渲染内容 → 加 on 显示出来 → 下一帧再算位置。
    // 因为 display:none 时 offsetHeight 是 0，会算出一个溢出屏幕的 top（踩过）。
    renderPanel();
    requestAnimationFrame(function () {
      if (!panelOpen) { return; }
      placePanel();
    });
    W.addEventListener('scroll', placePanel, true);
    W.addEventListener('resize', placePanel);
  }

  /// 面板贴着指示器上方展开，左边超出屏幕就往右推回来。
  function placePanel() {
    if (!root || !panel || !panelOpen) { return; }
    var r = root.getBoundingClientRect();
    var w = 268;
    var left = r.left + r.width / 2 - 22;   // 稍微偏左，避免完全挤压右侧
    if (left + w > W.innerWidth - 8) { left = W.innerWidth - w - 8; }
    if (left < 8) { left = 8; }

    // fixed 定位下只用 top：同时设 top/bottom 会把面板拉伸。
    // 兜底高度按实际内容量给足（7 个分组 + 3 行 token ≈ 270），宁可保守也不要溢出。
    var estH = panel.offsetHeight || 300;
    var top = r.top - estH - 10;
    if (top < 8) { top = r.bottom + 10; }    // 上方放不下就翻到下方
    if (top + estH > W.innerHeight - 8) {    // 下方也放不下就顶到屏幕内
      top = Math.max(8, W.innerHeight - estH - 8);
    }
    panel.style.top = top + 'px';
    panel.style.bottom = 'auto';
    panel.style.left = left + 'px';
    panel.style.right = 'auto';
  }

  /// 收起面板。所有收起路径都必须走这里，
  /// 否则会漏掉「解绑滚动监听」这一步，面板虽然看不见但监听一直挂着。
  function closePanel() {
    if (!panelOpen) { return; }
    panelOpen = false;
    if (panel) { panel.classList.remove('on'); }
    W.removeEventListener('scroll', placePanel, true);
    W.removeEventListener('resize', placePanel);
  }

  function onDocClick(ev) {
    if (!panelOpen) { return; }
    if (root && root.contains(ev.target)) { return; }
    if (panel && panel.contains(ev.target)) { return; }
    closePanel();
  }

  function onKeyDown(ev) {
    if (ev.key === 'Escape') { closePanel(); }
  }

  /// 页面「离开当前对话」时收起面板、并清掉上个会话的用量。
  ///
  /// 为什么需要：
  ///  1. 面板挂在 document.body 上（为了不被输入框的 overflow 裁切），
  ///     而这是 SPA —— 从对话退回首页时 React 只换内容、**不重建 body**，
  ///     面板会一直浮着（用户实测反馈过）。
  ///  2. `contextWindowUsage` 是按会话存的，退回首页后还留着上个会话的数字，
  ///     就会让人以为「新对话一打开就占了 4.2%」（同一个反馈）。
  ///
  /// 判据用 `home-greeting`（首页问候语，只在没进会话时存在）：
  /// 比「输入框在不在」可靠 —— 首页也有输入框。
  function checkStillInConversation() {
    var onHome = !!doc.querySelector('[data-testid="home-greeting"]');
    if (onHome) {
      closePanel();
      // 首页没有会话，数据必须清空；否则残留的百分比会误导人
      if (state.max || state.used || state.groups.length) {
        resetUsage();
        state.snapshots = 0;
        renderButton();
      }
    }
  }

  // ---------------------------------------------------------------
  // 守卫：React 重渲染会把我们的节点摘掉，需要重新挂回去
  // ---------------------------------------------------------------
  var guardTimer = null;
  function startGuard() {
    // 500ms：这个轮询同时负责「离开会话收起面板」，
    // 太慢的话返回后残留面板会明显停留（1.5s 时肉眼可见）。
    guardTimer = setInterval(function () {
      if (destroyed) { return; }
      // 面板开着但已经离开会话（比如点了返回）→ 立刻收起
      checkStillInConversation();
      // ⚠️ 不能只看 isConnected。
      // 页面首次挂载时发送按钮还没渲染，会退到"输入框上方"那个兜底位置；
      // 之后节点一直在 DOM 里，isConnected 恒为真，就永远停在错误位置了。
      // 所以要校验父节点是否就是当前该去的锚点，不一致就重新挂。
      if (!root || !root.isConnected || !inRightPlace()) { mount(); }
    }, 500);
    // 浏览器后退/前进也收起
    W.addEventListener('popstate', closePanel);
  }

  /// 指示器现在是否挂在「应该挂的位置」上。
  function inRightPlace() {
    if (!root || !root.parentNode) { return false; }
    // ⚠️ 绝对不能在 AskQuestion 卡片里。
    // 用户实测反馈过：agent 运行中按钮换成停止按钮，旧逻辑找不到锚点就兜底抓
    // textarea，正好抓中题卡的「Other (write your answer)」，圆环跑到选项行里。
    if (root.closest && root.closest(ASK_SEL)) { return false; }
    var a = findAnchor();
    if (!a || !a.node) { return true; }   // 锚点都没了，别乱动
    return root.parentNode === a.node;
  }

  // ---------------------------------------------------------------
  // 启动
  // ---------------------------------------------------------------
  function start() {
    wrapFetch();
    mount();
    startGuard();

    // React 首次渲染晚于 DOMContentLoaded，这里高频补挂一段时间。
    // 判据同样是「位置对不对」：发送按钮一旦渲染出来，就应立刻挪到它旁边。
    var tries = 0;
    var boot = setInterval(function () {
      tries++;
      if (destroyed || tries > 60) { clearInterval(boot); return; }
      if (!root || !root.isConnected || !inRightPlace()) { mount(); }
      else { clearInterval(boot); }
    }, 200);
  }

  function destroy() {
    destroyed = true;
    if (renderTimer) { clearTimeout(renderTimer); }
    if (guardTimer) { clearInterval(guardTimer); }
    try {
      if (origFetch) { W.fetch = origFetch; }
    } catch (e) {}
    try { doc.removeEventListener('click', onDocClick, true); } catch (e) {}
    try { doc.removeEventListener('keydown', onKeyDown, true); } catch (e) {}
    try { W.removeEventListener('popstate', closePanel); } catch (e) {}
    if (root && root.parentNode) { root.parentNode.removeChild(root); }
    if (panel && panel.parentNode) { panel.parentNode.removeChild(panel); }
    root = null;
    face = null;
    panel = null;
    var st = doc.getElementById('agy-usage-style');
    if (st && st.parentNode) { st.parentNode.removeChild(st); }
  }

  W[KEY] = { destroy: destroy, state: state, mount: mount };

  if (doc.readyState === 'loading') {
    doc.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    start();
  }
})();
