/**
 * Antigravity 中文注入脚本
 *
 * 这个函数会被序列化后，通过远程调试协议（CDP）注入到 Antigravity 的渲染进程执行。
 * 因此它必须是完全自包含的：不能引用外部变量、不能有闭包依赖。
 *
 * 关键设计（为什么不会误翻你的内容）：
 *  1. 白名单匹配 —— 只有词库中明确存在的词才替换，其余一律不动；
 *  2. 区域排除  —— 代码块、输入框、用户消息、AI 回复正文直接跳过；
 *  3. 幂等标记  —— 已翻译的节点打标记，重复注入不会二次替换；
 *  4. 增量监听  —— 只对新增节点做翻译，不全量重扫，几乎无性能开销。
 *
 * 入参结构：
 *   dict  = { 'English': '中文', ... }        文案词库（大小写不敏感）
 *   rules = { 'data-testid值': '中文', ... }   元素级规则（用于一词多义的按钮）
 */
function __AGY_ZH_INSTALL__(dict, rules) {
  const W = window;
  const KEY = '__AGY_ZH__';

  // 已安装则先卸载旧的，保证重复注入安全
  if (W[KEY] && typeof W[KEY].destroy === 'function') {
    try { W[KEY].destroy(); } catch (e) {}
  }

  const DICT = dict || {};
  const RULES = rules || {};

  // 占位符词条（含 {x}）单独走模板匹配，其余走全等匹配
  const exact = new Map();       // key: 小写英文 -> 中文
  const prefix = [];             // [原模板, 中文]
  for (const k of Object.keys(DICT)) {
    if (k.indexOf('{') !== -1) prefix.push([k, DICT[k]]);
    else exact.set(k.toLowerCase(), DICT[k]);
  }
  prefix.sort((a, b) => b[0].length - a[0].length);

  // 内容保护：这些区域的文字属于"用户/AI 的真实内容"，不翻译。
  // 代码与输入框的硬性拦截在 shouldSkip 里单独处理（优先级最高）。
  const SKIP_SELECTOR = [
    '[data-testid="user-message-bubble"]',      // 用户消息正文
    '[data-testid="planner-response-text"]',    // AI 回复正文
    '[data-testid="changed-files-card"]',       // 文件变更卡片（含代码/路径）
    '.agy-user-bubble-frame .agy-user-bubble',  // 用户气泡内层
    '[data-testid="code-block"]',
  ].join(',');

  const ATTRS = ['placeholder', 'title', 'aria-label'];
  const MARK = 'data-agy-zh';

  // ── 正则规则：只匹配结构固定的界面模式（相对时间等） ──
  const UNIT = { s: '秒', m: '分钟', h: '小时', d: '天', w: '周', mo: '个月', y: '年' };
  const RX_AGO = /^(\d+)\s*(mo|[smhdwy])\s*ago$/i;
  const RX_BARE = /^(\d+)\s*(mo|[smhdwy])$/i;

  function translateByRegex(core) {
    let m = RX_AGO.exec(core);
    if (m) return `${m[1]} ${UNIT[m[2].toLowerCase()]}前`;
    m = RX_BARE.exec(core);
    if (m) return `${m[1]} ${UNIT[m[2].toLowerCase()]}`;
    return null;
  }

  // 元素级规则：按 data-testid 精确定位，解决 "New" 这类一词多义
  function lookupRule(el) {
    if (!el || !el.getAttribute) return null;
    let node = el;
    let depth = 0;
    while (node && node.getAttribute && depth < 4) {
      const tid = node.getAttribute('data-testid');
      if (tid && RULES[tid] != null) return { value: RULES[tid], node };
      node = node.parentElement;
      depth++;
    }
    return null;
  }

  function translate(raw) {
    if (!raw) return null;
    const lead = raw.match(/^\s*/)[0];
    const trail = raw.match(/\s*$/)[0];
    const core = raw.trim();
    if (!core) return null;

    // 已基本是中文则跳过（不能用"含中文就跳"，否则带用户名的界面文案会漏翻）
    const cjk = (core.match(/[\u4e00-\u9fa5]/g) || []).length;
    const visible = core.replace(/\s/g, '').length;
    if (cjk > 0 && cjk >= visible * 0.5) return null;

    // 1) 全等匹配（大小写不敏感）
    const hit = exact.get(core.toLowerCase());
    if (hit != null) return lead + hit + trail;

    // 2) 结构型模式（相对时间等）
    const rx = translateByRegex(core);
    if (rx != null) return lead + rx + trail;

    // 3) 占位符模板匹配，支持一个模板里出现多个占位符
    for (let i = 0; i < prefix.length; i++) {
      const [pat, val] = prefix[i];
      const phs = pat.match(/\{[^}]+\}/g);
      if (!phs || !phs.length) continue;

      // 以第一个占位符切分头尾，中间部分再按后续占位符递归处理
      const parts = pat.split(phs[0]);
      const head = (parts[0] || '').trimEnd();
      const tail = (parts[1] || '').trimStart();

      if (head && !core.toLowerCase().startsWith(head.toLowerCase())) continue;

      let rest = core.slice(head.length);
      if (tail) {
        if (!rest.toLowerCase().endsWith(tail.toLowerCase())) continue;
        rest = rest.slice(0, rest.length - tail.length);
      }

      const mid = rest.trim();
      if (!mid) continue;

      // 占位内容守则：纯时间量、或是不含句子标点的短文本（用户名等）
      const timeVal = /^\d+(\.\d+)?\s*(s|m|h|d|w|mo|y|sec|min|hour|day|week|month|year)?$/i;
      const shortVal = /^[^\n]{1,60}$/;
      if (!(timeVal.test(mid) || (shortVal.test(mid) && !/[。！？]$/.test(mid)))) continue;

      return lead + val.replace(phs[0], mid) + trail;
    }
    return null;
  }

  function shouldSkip(el) {
    if (!el) return true;
    if (!el.closest) return false;

    // 硬规则：代码/输入框/编辑器内一律不碰，优先级最高
    if (el.closest('pre,code,kbd,samp,textarea,input,select,.monaco-editor,.xterm,.diff-view')) {
      return true;
    }

    // 例外：按钮/菜单/提示里的文案属于界面，即使在受保护容器内也应当翻译
    const uiControl = el.closest('button,[role="button"],[role="menuitem"],[role="menuitemradio"],summary');
    if (uiControl) return false;

    if (el.closest(SKIP_SELECTOR)) return true;
    return false;
  }

  let stat = { text: 0, attr: 0, rule: 0 };

  function applyRule(el, raw) {
    const r = lookupRule(el);
    if (!r || r.value == null) return null;
    const lead = raw.match(/^\s*/)[0];
    const trail = raw.match(/\s*$/)[0];
    if (raw.trim() === r.value) return null;
    return lead + r.value + trail;
  }

  function translateTextNode(node) {
    if (!node || node.nodeType !== 3) return;
    if (node[MARK]) return;
    const parent = node.parentElement;
    if (!parent || shouldSkip(parent)) return;

    // 元素级规则优先
    const ruled = applyRule(parent, node.nodeValue);
    const out = ruled != null ? ruled : translate(node.nodeValue);

    if (out == null) return;
    if (out === node.nodeValue) return;
    node[MARK] = 1;
    node.nodeValue = out;
    if (ruled != null) stat.rule++; else stat.text++;
  }

  function translateAttributes(el) {
    if (!el || el.nodeType !== 1 || shouldSkip(el)) return;
    for (let i = 0; i < ATTRS.length; i++) {
      const a = ATTRS[i];
      if (!el.hasAttribute(a)) continue;
      const markKey = MARK + '-' + a;
      if (el[markKey]) continue;
      const out = translate(el.getAttribute(a));
      if (out == null) continue;
      if (out === el.getAttribute(a)) continue;
      el[markKey] = 1;
      el.setAttribute(a, out);
      stat.attr++;
    }
  }

  function scan(root, force) {
    if (!root || !root.isConnected) return;
    const nodeType = root.nodeType;

    if (nodeType === 3) { translateTextNode(root); return; }
    if (nodeType !== 1 && nodeType !== 9 && nodeType !== 11) return;

    // force=true 时清除旧标记，让新规则重新生效
    if (force) {
      const w0 = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
      let t0;
      while ((t0 = w0.nextNode())) t0[MARK] = undefined;
      if (root.querySelectorAll) {
        const all = root.querySelectorAll('*');
        for (let i = 0; i < all.length; i++) {
          const el = all[i];
          for (let j = 0; j < ATTRS.length; j++) el[MARK + '-' + ATTRS[j]] = undefined;
        }
      }
    }

    if (nodeType === 1) translateAttributes(root);

    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
    let n;
    while ((n = walker.nextNode())) translateTextNode(n);

    if (root.querySelectorAll) {
      const els = root.querySelectorAll('[placeholder],[title],[aria-label]');
      for (let i = 0; i < els.length; i++) translateAttributes(els[i]);
    }
  }

  // ── 变化监听：只处理新增节点，不动已有节点 ──
  let pending = new Set();
  let scheduled = false;

  function flush() {
    scheduled = false;
    const batch = pending;
    pending = new Set();
    batch.forEach((n) => {
      try { scan(n); } catch (e) {}
    });
  }

  const observer = new MutationObserver((records) => {
    for (let i = 0; i < records.length; i++) {
      const r = records[i];
      if (r.type === 'characterData') {
        if (r.target && r.target[MARK] && r.target.nodeValue) {
          const out = translate(r.target.nodeValue);
          if (out != null) r.target.nodeValue = out;
          else r.target[MARK] = undefined;
        }
      } else if (r.type === 'attributes') {
        if (r.target && r.target.nodeType === 1) pending.add(r.target);
      } else if (r.addedNodes) {
        for (let j = 0; j < r.addedNodes.length; j++) {
          const n = r.addedNodes[j];
          if (n.nodeType === 1 || n.nodeType === 3) pending.add(n);
        }
      }
    }
    if (pending.size && !scheduled) {
      scheduled = true;
      requestAnimationFrame(flush);
    }
  });

  scan(document.body);

  observer.observe(document.body, {
    childList: true,
    subtree: true,
    characterData: true,
    attributes: true,
    attributeFilter: ATTRS,
  });

  // 兜底补扫：菜单/气泡等按需渲染的浮层偶尔会绕过 MutationObserver 的
  // 增量路径（例如被 React 复用节点后只改文本）。低频补扫代价极小
  // （全量约 220 个文本节点、<1ms），但能显著提升覆盖率。
  let patrol = null;
  function startPatrol() {
    if (patrol) return;
    patrol = setInterval(() => {
      try { scan(document.body); } catch (e) {}
    }, 1200);
  }
  startPatrol();

  W[KEY] = {
    stat,
    rescan: function () { stat = { text: 0, attr: 0, rule: 0 }; scan(document.body, true); return stat; },
    destroy: function () {
      observer.disconnect();
      if (patrol) { clearInterval(patrol); patrol = null; }
      W[KEY] = null;
    },
  };

  return stat.text + stat.attr + stat.rule;
}
