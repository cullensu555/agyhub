
(function(){
  // ⚠️ 这个文件是原样注入页面的（include_str!），不经过任何模板替换。
  // 从 node 版迁移时这里残留过 `${JSON.stringify(TARGET_FLAG)}`，
  // 那是 node 模板字符串的占位符，在普通 JS 里是语法错误 ——
  // 整个钩子从来没执行成功过。改动后请用 `node --check` 检查一遍。
  var S = window.__AGY_SSH_UNLOCK__;
  if (S && !S.off) return true;
  if (S && S.off && S.realFetch) {
    // 之前被关掉过：重新打开即可，不要再包一层 fetch
    S.off = false;
    return true;
  }
  window.__AGY_SSH_UNLOCK__ = { hits: 0, patched: 0, errors: [], off: false, realFetch: null };

  var TARGET = 'enable-ssh-remote';

  function patchFrames(bytes) {
    var chunks = [], pos = 0, changed = false, touched = 0;

    while (pos + 5 <= bytes.length) {
      var flagByte = bytes[pos];
      var len = ((bytes[pos + 1] << 24) | (bytes[pos + 2] << 16) |
                 (bytes[pos + 3] << 8) | bytes[pos + 4]) >>> 0;
      var end = pos + 5 + len;
      if (end > bytes.length) break;

      if (flagByte === 0 && len > 0) {
        try {
          var text = new TextDecoder('utf-8').decode(bytes.subarray(pos + 5, end));
          var obj = JSON.parse(text);
          var cfg = obj && obj.experimentConfig;
          if (cfg && Array.isArray(cfg.experiments)) {
            for (var i = 0; i < cfg.experiments.length; i++) {
              var e = cfg.experiments[i];
              if (e && e.keyString === TARGET && e.disabled) {
                delete e.disabled;
                touched++;
              }
            }
            if (touched > 0) {
              var nb = new TextEncoder().encode(JSON.stringify(obj));
              var hdr = new Uint8Array(5);
              hdr[0] = 0;
              hdr[1] = (nb.length >>> 24) & 255;
              hdr[2] = (nb.length >>> 16) & 255;
              hdr[3] = (nb.length >>> 8) & 255;
              hdr[4] = nb.length & 255;
              chunks.push(hdr, nb);
              changed = true;
              pos = end;
              continue;
            }
          }
        } catch (err) {
          window.__AGY_SSH_UNLOCK__.errors.push(String(err).slice(0, 80));
        }
      }

      chunks.push(bytes.subarray(pos, end));
      pos = end;
    }

    if (pos < bytes.length) chunks.push(bytes.subarray(pos));
    if (!changed) return null;

    var total = 0;
    for (var k = 0; k < chunks.length; k++) total += chunks[k].length;
    var merged = new Uint8Array(total);
    var off = 0;
    for (var j = 0; j < chunks.length; j++) {
      merged.set(chunks[j], off);
      off += chunks[j].length;
    }
    return merged;
  }

  var realFetch = window.fetch;
  window.__AGY_SSH_UNLOCK__.realFetch = realFetch;
  window.fetch = function (input, init) {
    var url = String(
      typeof input === 'string' ? input : (input && input.url) || ''
    );
    if (window.__AGY_SSH_UNLOCK__.off || url.indexOf('GetMendelFlags') === -1) {
      return realFetch.apply(this, arguments);
    }

    window.__AGY_SSH_UNLOCK__.hits++;

    return realFetch.apply(this, arguments).then(function (res) {
      // 用 tee() 分流，避免自己吃掉调用方的 body
      if (!res.body || typeof res.body.tee !== 'function') return res;

      var pair = res.body.tee();
      var reader = pair[0].getReader();
      var parts = [];

      var stream = new ReadableStream({
        start: function (ctrl) {
          (function pump() {
            reader.read().then(function (step) {
              if (step.done) {
                var size = 0;
                for (var i = 0; i < parts.length; i++) size += parts[i].length;
                var all = new Uint8Array(size);
                var at = 0;
                for (var i = 0; i < parts.length; i++) {
                  all.set(parts[i], at);
                  at += parts[i].length;
                }

                var fixed = patchFrames(all);
                if (fixed) {
                  window.__AGY_SSH_UNLOCK__.patched++;
                  ctrl.enqueue(fixed);
                } else {
                  ctrl.enqueue(all);
                }
                ctrl.close();
              } else {
                parts.push(step.value);
                pump();
              }
            }).catch(function (err) {
              window.__AGY_SSH_UNLOCK__.errors.push(String(err).slice(0, 80));
              try { ctrl.error(err); } catch (_) {}
            });
          })();
        }
      });

      return new Response(stream, {
        status: res.status,
        statusText: res.statusText,
        headers: res.headers
      });
    });
  };
  return true;
})();
