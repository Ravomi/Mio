// Mio Official Site — main.js
// Powered by tasteskill v2 ("The quiet instrument")

(function () {
  'use strict';

  var currentLang = 'zh';

  // 1. Scroll reveal observer (from tasteskill base.js)
  function initScrollReveal() {
    var targets = document.querySelectorAll('[data-reveal]');
    if (!targets.length) return;
    if (!('IntersectionObserver' in window)) {
      targets.forEach(function (el) { el.classList.add('is-in'); });
      return;
    }
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (!entry.isIntersecting) return;
        entry.target.classList.add('is-in');
        io.unobserve(entry.target);
      });
    }, { rootMargin: '0px 0px -10% 0px', threshold: 0.08 });

    targets.forEach(function (el) { io.observe(el); });

    window.addEventListener('load', function () {
      setTimeout(function () {
        targets.forEach(function (el) {
          var r = el.getBoundingClientRect();
          if (r.top < window.innerHeight && r.bottom > 0) {
            el.classList.add('is-in');
          }
        });
      }, 2200);
    });
  }

  // 2. Video reduced-motion sync (from tasteskill references/imagery.md)
  function initVideoMotionGuard() {
    if (!window.matchMedia) return;
    var calm = window.matchMedia('(prefers-reduced-motion: reduce)');
    var videos = document.querySelectorAll('video[autoplay]');
    videos.forEach(function (v) {
      var sync = function () {
        if (calm.matches) {
          v.removeAttribute('autoplay');
          v.pause();
        } else {
          v.play().catch(function () {});
        }
      };
      sync();
      if (calm.addEventListener) {
        calm.addEventListener('change', sync);
      }
    });
  }

  // 3. Hero Live Probe ("Control that reads the page")
  function initHeroLiveProbe() {
    var bgReadout = document.getElementById('probeBgReadout');
    var h1Readout = document.getElementById('probeHeadlineReadout');
    var ptrReadout = document.getElementById('probePointerReadout');
    var domReadout = document.getElementById('probeDomReadout');
    var modeBadge = document.getElementById('probeModeBadge');
    var termOut = document.getElementById('probeTerminalOutput');
    var probeTabs = document.querySelectorAll('[data-probe-op]');
    if (!bgReadout || !termOut) return;

    var currentOp = 'read';
    var pointerX = Math.round(window.innerWidth / 2);
    var pointerY = Math.round(window.innerHeight / 2);

    function rgbToHex(rgbStr) {
      var m = (rgbStr || '').match(/\d+/g);
      if (!m || m.length < 3) return '#1b1d20';
      var toHex = function (n) {
        var h = Number(n).toString(16);
        return h.length === 1 ? '0' + h : h;
      };
      return '#' + toHex(m[0]) + toHex(m[1]) + toHex(m[2]);
    }

    function measurePage() {
      var bodyStyle = window.getComputedStyle(document.body);
      var bgRgb = bodyStyle.backgroundColor;
      var bgHex = rgbToHex(bgRgb);
      bgReadout.textContent = bgHex + ' · ' + bgRgb;

      var h1 = document.querySelector('h1');
      var h1Size = 36;
      var h1Lines = 2;
      if (h1) {
        var h1Cs = window.getComputedStyle(h1);
        h1Size = Math.round(parseFloat(h1Cs.fontSize) * 10) / 10;
        var lh = parseFloat(h1Cs.lineHeight) || h1Size * 1.1;
        h1Lines = Math.max(1, Math.round(h1.getBoundingClientRect().height / lh));
      }
      h1Readout.textContent = h1Size + ' px · ' + h1Lines + ' lines';
      ptrReadout.textContent = 'x: ' + pointerX + ', y: ' + pointerY + ' · ' + window.innerWidth + 'w';

      var nodeCount = document.querySelectorAll('*').length;
      var sectionCount = document.querySelectorAll('section').length;
      domReadout.textContent = nodeCount + ' nodes · ' + sectionCount + ' sections';

      renderProbeTerminal(bgHex, h1Size, h1Lines, nodeCount, sectionCount);
    }

    function renderProbeTerminal(bgHex, h1Size, h1Lines, nodeCount, sectionCount) {
      if (currentOp === 'read') {
        modeBadge.textContent = 'read · active';
        termOut.innerHTML =
          '<code>$ mio --cwd ./docs -p "读取当前页面实测度量"\n' +
          '[read] path="docs/index.html" offset=1 limit=60 (64 KiB cap)\n' +
          '[dom]  nodes=' + nodeCount + ', sections=' + sectionCount + ', viewport=' + window.innerWidth + 'x' + window.innerHeight + '\n' +
          '[type] h1=' + h1Size + 'px (' + h1Lines + ' lines), ground=' + bgHex + ', pointer=(' + pointerX + ', ' + pointerY + ')</code>';
      } else if (currentOp === 'grep') {
        modeBadge.textContent = 'grep · Thompson NFA';
        termOut.innerHTML =
          '<code>$ mio --cwd ./docs -t read,grep,find,ls -p "检索页面中的工作区原语"\n' +
          '[grep] pattern="read|edit|grep|find|ls|bash" regex=true\n' +
          '  docs/index.html:46  data-probe-op="read"\n' +
          '  docs/index.html:104 [grep] pattern="execute_edit" path="src/workspace.rs"\n' +
          '  docs/index.html:356 Built-in workspace primitives (7 matched, 0 symlinks traversed)</code>';
      } else {
        modeBadge.textContent = 'edit · unique match';
        termOut.innerHTML =
          '<code>$ mio --cwd ./docs --approve -p "校验唯一匹配替换边界"\n' +
          '--- a/docs/styles.css\n' +
          '+++ b/docs/styles.css\n' +
          '@@ -6,2 +6,2 @@ :root\n' +
          '<span class="ui-diff-del">-  --bg: ' + bgHex + ';</span>\n' +
          '<span class="ui-diff-add">+  --bg: ' + bgHex + '; /* verified at (' + pointerX + ', ' + pointerY + ') */</span>\n' +
          '[ok] 1 unique match verified; file permissions preserved.</code>';
      }
    }

    probeTabs.forEach(function (tab) {
      tab.addEventListener('click', function () {
        currentOp = tab.getAttribute('data-probe-op') || 'read';
        probeTabs.forEach(function (t) {
          var active = t === tab;
          t.classList.toggle('is-active', active);
          t.setAttribute('aria-selected', active ? 'true' : 'false');
        });
        measurePage();
      });
    });

    window.addEventListener('pointermove', function (e) {
      pointerX = Math.round(e.clientX);
      pointerY = Math.round(e.clientY);
      ptrReadout.textContent = 'x: ' + pointerX + ', y: ' + pointerY + ' · ' + window.innerWidth + 'w';
      if (currentOp === 'read') {
        measurePage();
      }
    }, { passive: true });

    window.addEventListener('resize', measurePage, { passive: true });
    measurePage();
  }

  // 4. Showcase Middle State Live Gate (Approve unique edit vs Reject ambiguous match)
  function initShowcaseGate() {
    var approveBtn = document.getElementById('approveEditBtn');
    var ambigBtn = document.getElementById('simulateAmbiguousBtn');
    var gateBadge = document.getElementById('showcaseGateBadge');
    var thirdWord = document.getElementById('showcaseThirdStateWord');
    var resTitle = document.getElementById('showcaseResultTitle');
    var resBadge = document.getElementById('showcaseResultBadge');
    var resPre = document.getElementById('showcaseResultPre');
    var resFoot = document.getElementById('showcaseResultFoot');
    if (!approveBtn || !ambigBtn || !resPre) return;

    approveBtn.addEventListener('click', function () {
      approveBtn.classList.add('is-active');
      ambigBtn.classList.remove('is-active');
      gateBadge.textContent = '1 unique match · 已放行';
      thirdWord.textContent = currentLang === 'en' ? 'Committed' : '原子快照';
      resTitle.textContent = '.mio/sessions/20261014-01.json · 0600 原子写入完成';
      resBadge.textContent = 'committed · 0600';
      resPre.innerHTML =
        '<code>[edit]  src/workspace.rs 唯一匹配校验通过，原子替换完成 (保留 0644 权限)\n' +
        '[bash]  cargo test --test workspace --locked -> 18 passed; 0 failed (0.42s)\n' +
        '[save]  会话快照已写入 .mio/sessions/20261014-01.json (权限 0600，绝不写入 API Key)\n' +
        '[ready] 可随时运行 mio --resume --fork 从当前节点分叉探索新方案</code>';
      resFoot.textContent = '修改有依据，歧义零容忍';
    });

    ambigBtn.addEventListener('click', function () {
      ambigBtn.classList.add('is-active');
      approveBtn.classList.remove('is-active');
      gateBadge.textContent = '2 matches · 歧义拦截';
      thirdWord.textContent = currentLang === 'en' ? 'Rejected' : '歧义拦截';
      resTitle.textContent = 'mio-coding-agent::WorkspaceError · 拒绝歧义替换，原文件零改动';
      resBadge.textContent = 'rejected · 0 bytes written';
      resPre.innerHTML =
        '<code>[error] edit rejected on path="src/workspace.rs":\n' +
        '        old_text matched 2 locations (lines 233 and 412).\n' +
        '[guard] 文件未被修改 (0 bytes written)，请提供更多上下文行以确保唯一匹配，\n' +
        '        或使用非重叠 edits[] 数组一次性提交精确位置。</code>';
      resFoot.textContent = '绝不猜测匹配位置，杜绝静默改坏代码';
    });
  }

  // 5. Features Section: Fixed Output, Live Controls
  function initFeaturesLiveControls() {
    var toolsReadout = document.getElementById('cfgToolsReadout');
    var subReadout = document.getElementById('cfgSubagentReadout');
    var thinkReadout = document.getElementById('cfgThinkingReadout');
    var modeReadout = document.getElementById('cfgModeReadout');
    var memBadge = document.getElementById('cfgMemoryBadge');
    var stagePre = document.getElementById('cfgStagePre');
    var copyCmdBtn = document.getElementById('copyConfiguredCmdBtn');

    var ctrlToolStatus = document.getElementById('ctrlToolStatus');
    var subRange = document.getElementById('subagentRange');
    var subRangeVal = document.getElementById('subagentRangeVal');
    var ctrlThinkStatus = document.getElementById('ctrlThinkingStatus');
    var ctrlModeStatus = document.getElementById('ctrlModeStatus');

    if (!stagePre) return;

    var state = {
      view: 'cli',
      tools: 'full',
      subagents: 2,
      thinking: 'high',
      mode: 'repl'
    };

    function buildCommand() {
      var parts = ['mio', '--cwd /repo'];
      if (state.tools === 'readonly') {
        parts.push('-t read,grep,find,ls');
      } else if (state.tools === 'approve') {
        parts.push('--approve');
      }
      if (state.subagents > 0) {
        parts.push('--subagents');
      }
      parts.push('--thinking ' + state.thinking);
      if (state.mode === 'json') {
        parts.push('--mode json');
      } else if (state.mode === 'rpc') {
        parts.push('--mode rpc');
      }
      if (state.mode !== 'rpc') {
        parts.push('-p "梳理模块边界并执行安全重构"');
      }
      return parts.join(' ');
    }

    function updateStage() {
      var rss = (5.6 + state.subagents * 0.9).toFixed(1);
      memBadge.textContent = 'RSS ~' + rss + ' MB';

      if (state.tools === 'full') {
        toolsReadout.textContent = '7 个一级工作区工具';
        if (ctrlToolStatus) ctrlToolStatus.textContent = '7 tools active';
      } else if (state.tools === 'readonly') {
        toolsReadout.textContent = '4 个只读工具 (禁写/禁Shell)';
        if (ctrlToolStatus) ctrlToolStatus.textContent = 'read-only (4 tools)';
      } else {
        toolsReadout.textContent = '写与 Shell 需终端确认';
        if (ctrlToolStatus) ctrlToolStatus.textContent = '--approve gate on';
      }

      if (state.subagents === 0) {
        subReadout.textContent = '关闭 · 单主循环';
      } else {
        subReadout.textContent = state.subagents + ' 路只读 · R/W Lock';
      }
      if (subRangeVal) {
        subRangeVal.textContent = state.subagents + ' 路并发 · ~' + rss + ' MB RSS';
      }

      thinkReadout.textContent = state.thinking + ' · 推理预算';
      if (ctrlThinkStatus) ctrlThinkStatus.textContent = 'level: ' + state.thinking;

      var modeMap = {
        repl: '交互式 REPL',
        json: 'JSON Lines 事件流',
        rpc: '双向 Stdio RPC + MCP',
        sdk: 'Rust Host 进程内嵌入'
      };
      modeReadout.textContent = modeMap[state.mode] || '交互式 REPL';
      if (ctrlModeStatus) ctrlModeStatus.textContent = 'mode: ' + state.mode;

      var cmd = buildCommand();
      if (copyCmdBtn) {
        copyCmdBtn.setAttribute('data-copy-text', cmd);
      }

      if (state.view === 'cli' && state.mode !== 'sdk') {
        var lockLine = state.subagents > 0
          ? '[subagent] spawned ' + state.subagents + ' worker(s) (read_only: true, shared dir read-lock)\n[lock]     WorkspaceLock coordinates shared reads vs exclusive parent writes'
          : '[loop]     single-agent deterministic loop (subagents disabled)';
        var gateLine = state.tools === 'readonly'
          ? '[policy]   write, edit, bash omitted at registration & blocked at execution'
          : state.tools === 'approve'
            ? '[approve]  Host::approve_tool prompts [y/N] before edit/write/bash'
            : '[tools]    read, write, edit, grep, find, ls, bash ready (checked_path on)';
        stagePre.innerHTML =
          '<code>$ ' + cmd + '\n\n' +
          '[context]  84-char base prompt (~180 tok) + AGENTS.md · thinking=' + state.thinking + '\n' +
          gateLine + '\n' +
          lockLine + '</code>';
      } else if (state.view === 'jsonl' || state.mode === 'json' || state.mode === 'rpc') {
        stagePre.innerHTML =
          '<code>{"type":"session_start","cwd":"/repo","thinking":"' + state.thinking + '","subagents":' + state.subagents + '}\n' +
          '{"type":"tool_start","tool":"grep","args":{"pattern":"WorkspaceLock","path":"src/"}}\n' +
          '{"type":"tool_result","tool":"grep","matches":4,"truncated":false}\n' +
          (state.mode === 'rpc'
            ? '{"type":"rpc_ready","methods":["prompt","steer","follow_up","abort","compact","fork_session"]}'
            : '{"type":"session_saved","path":".mio/sessions/latest.json","mode":"0600"}') + '</code>';
      } else {
        stagePre.innerHTML =
          '<code>use mio_coding_agent::{Agent, Config, Host, ThinkingLevel};\n\n' +
          'let mut cfg = Config::from_env("/repo")?;\n' +
          'cfg.thinking = ThinkingLevel::' + state.thinking.charAt(0).toUpperCase() + state.thinking.slice(1) + ';\n' +
          'cfg.enable_subagents = ' + (state.subagents > 0 ? 'true' : 'false') + ';\n' +
          'let mut agent = Agent::new(cfg, my_in_process_host)?;\n' +
          'let reply = agent.prompt("检查模块边界并生成精确补丁").await?;</code>';
      }
    }

    document.querySelectorAll('[data-cfg-view]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        state.view = btn.getAttribute('data-cfg-view') || 'cli';
        document.querySelectorAll('[data-cfg-view]').forEach(function (b) {
          var active = b === btn;
          b.classList.toggle('is-active', active);
          b.setAttribute('aria-selected', active ? 'true' : 'false');
        });
        updateStage();
      });
    });

    document.querySelectorAll('[data-ctrl-tools]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        state.tools = btn.getAttribute('data-ctrl-tools') || 'full';
        document.querySelectorAll('[data-ctrl-tools]').forEach(function (b) {
          b.classList.toggle('is-active', b === btn);
        });
        updateStage();
      });
    });

    if (subRange) {
      subRange.addEventListener('input', function () {
        state.subagents = Number(subRange.value) || 0;
        updateStage();
      });
    }

    document.querySelectorAll('[data-ctrl-thinking]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        state.thinking = btn.getAttribute('data-ctrl-thinking') || 'high';
        document.querySelectorAll('[data-ctrl-thinking]').forEach(function (b) {
          b.classList.toggle('is-active', b === btn);
        });
        updateStage();
      });
    });

    document.querySelectorAll('[data-ctrl-mode]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        state.mode = btn.getAttribute('data-ctrl-mode') || 'repl';
        if (state.mode === 'sdk') state.view = 'rust';
        else if (state.mode === 'json' || state.mode === 'rpc') state.view = 'jsonl';
        else state.view = 'cli';

        document.querySelectorAll('[data-cfg-view]').forEach(function (b) {
          var active = b.getAttribute('data-cfg-view') === state.view;
          b.classList.toggle('is-active', active);
          b.setAttribute('aria-selected', active ? 'true' : 'false');
        });
        document.querySelectorAll('[data-ctrl-mode]').forEach(function (b) {
          b.classList.toggle('is-active', b === btn);
        });
        updateStage();
      });
    });

    updateStage();
  }

  // 6. Copy buttons & Theme / Language toggles
  function copyText(text, onDone) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(onDone).catch(function () {
        fallbackCopy(text, onDone);
      });
    } else {
      fallbackCopy(text, onDone);
    }
  }

  function fallbackCopy(text, onDone) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); } catch (e) {}
    document.body.removeChild(ta);
    if (onDone) onDone();
  }

  function initCopyAndToggles() {
    var live = document.getElementById('copy-status-live');
    document.addEventListener('click', function (e) {
      var targetBtn = e.target.closest('[data-copy-target]');
      if (targetBtn) {
        var id = targetBtn.getAttribute('data-copy-target');
        var el = document.getElementById(id);
        if (!el) return;
        var text = (el.innerText || el.textContent || '').trim();
        copyText(text, function () {
          var prev = targetBtn.textContent;
          targetBtn.textContent = currentLang === 'en' ? 'Copied' : '已复制';
          if (live) live.textContent = 'Copied to clipboard';
          setTimeout(function () { targetBtn.textContent = prev; }, 1500);
        });
        return;
      }

      var directBtn = e.target.closest('[data-copy-text]');
      if (directBtn) {
        var raw = directBtn.getAttribute('data-copy-text') || '';
        copyText(raw, function () {
          var prev = directBtn.textContent;
          directBtn.textContent = currentLang === 'en' ? 'Copied' : '已复制';
          if (live) live.textContent = 'Copied to clipboard';
          setTimeout(function () { directBtn.textContent = prev; }, 1500);
        });
      }
    });

    var themeBtn = document.getElementById('themeToggleBtn');
    if (themeBtn) {
      var updateThemeLabel = function () {
        var cur = document.documentElement.dataset.theme || 'dark';
        if (cur === 'dark') {
          themeBtn.textContent = currentLang === 'en' ? 'Light slate' : '浅灰台面';
        } else {
          themeBtn.textContent = currentLang === 'en' ? 'Matte graphite' : '哑光石墨';
        }
      };
      updateThemeLabel();
      themeBtn.addEventListener('click', function () {
        var cur = document.documentElement.dataset.theme || 'dark';
        var next = cur === 'dark' ? 'light' : 'dark';
        document.documentElement.dataset.theme = next;
        document.documentElement.dataset.themeMode = next;
        try { localStorage.setItem('mio:theme', next); } catch (e) {}
        updateThemeLabel();
        window.dispatchEvent(new Event('resize'));
      });
    }

    var langBtn = document.getElementById('langToggleBtn');
    if (langBtn) {
      langBtn.addEventListener('click', function () {
        currentLang = currentLang === 'zh' ? 'en' : 'zh';
        document.documentElement.lang = currentLang === 'en' ? 'en' : 'zh-CN';
        langBtn.textContent = currentLang === 'en' ? '中文' : 'EN';
        document.querySelectorAll('[data-zh][data-en]').forEach(function (el) {
          el.textContent = currentLang === 'en' ? el.getAttribute('data-en') : el.getAttribute('data-zh');
        });
      });
    }
  }

  document.addEventListener('DOMContentLoaded', function () {
    initScrollReveal();
    initVideoMotionGuard();
    initHeroLiveProbe();
    initShowcaseGate();
    initFeaturesLiveControls();
    initCopyAndToggles();
  });
})();
