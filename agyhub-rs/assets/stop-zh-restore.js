
(function () {
  var W = window;
  var KEY = '__AGY_ZH__';
  var removed = 0;

  // 1. 断开监听，防止它继续翻译
  try {
    if (W[KEY] && typeof W[KEY].destroy === 'function') {
      W[KEY].destroy();
    }
  } catch (e) {}

  // 2. 把被翻译过的节点还原。
  //    注入脚本给翻过的节点打了 data-agy-zh 标记，
  //    并且（见 inject.js）在 translate 时把原文存进了 dataset。
  //    这里优先用 dataset 里的原文还原；没有原文的，靠刷新兜底。
  try {
    var nodes = document.querySelectorAll('[data-agy-zh]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      var orig = el.getAttribute('data-agy-zh-orig');
      if (orig) {
        // 文本节点
        if (el.childNodes.length === 1 && el.firstChild.nodeType === 3) {
          el.firstChild.nodeValue = orig;
          removed++;
        }
      }
      el.removeAttribute('data-agy-zh');
      el.removeAttribute('data-agy-zh-orig');
    }
  } catch (e) {}

  W[KEY] = null;
  return removed;
})();
