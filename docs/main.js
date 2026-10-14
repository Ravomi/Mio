/**
 * Mio Official Website — Interactive Engine
 * Visual inspiration: ThoughtDAG (https://chenxiachan.github.io/thoughtdag/)
 * Functional inspiration: Pi Coding Agent (https://pi.dev/)
 */
(function () {
  'use strict';

  var currentLang = 'zh';
  var currentScenario = 'edit';
  var currentStageMode = 'cli'; // 'cli' | 'jsonl' | 'rust'
  var currentStepIndex = 0;
  var isPlaying = true;
  var playbackSpeed = 1;
  var stepTimer = null;

  function announce(msg) {
    var live = document.getElementById('copy-status-live');
    if (!live) return;
    live.textContent = '';
    setTimeout(function () {
      live.textContent = msg;
    }, 40);
  }

  // =========================================================================
  // 1. Theme Engine (Light / Dark / Auto)
  // =========================================================================
  function setupThemeToggle() {
    var btn = document.getElementById('themeToggleBtn');
    var iconEl = document.getElementById('themeIcon');
    var labelEl = document.getElementById('themeLabel');
    if (!btn) return;

    var media = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;

    function resolveTheme(mode) {
      if (mode === 'auto') {
        return media && media.matches ? 'dark' : 'light';
      }
      return mode === 'dark' ? 'dark' : 'light';
    }

    function applyThemeMode(mode, persist) {
      var resolved = resolveTheme(mode);
      document.documentElement.dataset.theme = resolved;
      document.documentElement.dataset.themeMode = mode;
      if (persist) {
        try {
          localStorage.setItem('mio:theme', mode);
        } catch (e) {}
      }
      if (iconEl) {
        iconEl.textContent = mode === 'dark' ? '☾' : mode === 'auto' ? '◐' : '☀';
      }
      if (labelEl) {
        var labels = {
          light: { zh: '浅色', en: 'Light' },
          dark: { zh: '深色', en: 'Dark' },
          auto: { zh: '自动', en: 'Auto' }
        };
        var item = labels[mode] || labels.light;
        labelEl.dataset.zh = item.zh;
        labelEl.dataset.en = item.en;
        labelEl.textContent = currentLang === 'en' ? item.en : item.zh;
      }
    }

    var initialMode = document.documentElement.dataset.themeMode || 'light';
    applyThemeMode(initialMode, false);

    btn.addEventListener('click', function () {
      var cur = document.documentElement.dataset.themeMode || 'light';
      var next = cur === 'light' ? 'dark' : cur === 'dark' ? 'auto' : 'light';
      applyThemeMode(next, true);
    });

    if (media && media.addEventListener) {
      media.addEventListener('change', function () {
        if ((document.documentElement.dataset.themeMode || 'light') === 'auto') {
          applyThemeMode('auto', false);
        }
      });
    }
  }

  // =========================================================================
  // 2. Bilingual Switcher (中文 / EN)
  // =========================================================================
  function setupLanguageToggle() {
    var btn = document.getElementById('langToggleBtn');
    if (!btn) return;

    function applyLanguage(lang) {
      currentLang = lang;
      document.documentElement.lang = lang === 'en' ? 'en' : 'zh-CN';
      btn.textContent = lang === 'en' ? '中文' : 'EN';

      document.querySelectorAll('[data-zh][data-en]').forEach(function (el) {
        el.textContent = lang === 'en' ? el.dataset.en : el.dataset.zh;
      });

      document.querySelectorAll('[data-zh-html][data-en-html]').forEach(function (el) {
        el.innerHTML = lang === 'en' ? el.dataset.enHtml : el.dataset.zhHtml;
      });

      renderStageScenario(currentScenario, false);
      renderBenchmarkChart(currentBenchMetric);
    }

    btn.addEventListener('click', function () {
      applyLanguage(currentLang === 'zh' ? 'en' : 'zh');
    });
  }

  // =========================================================================
  // 3. Brand Logo Context Menu & Hero Logo Stage Pulse (Pi style)
  // =========================================================================
  function setupLogoInteractions() {
    var brandTrigger = document.getElementById('brandLogoTrigger');
    var menu = document.getElementById('logoContextMenu');
    var copySvgBtn = document.getElementById('copyLogoSvgBtn');
    var heroStage = document.getElementById('heroLogoStage');
    var heroBadge = document.getElementById('heroLogoBadge');

    if (brandTrigger && menu) {
      brandTrigger.addEventListener('contextmenu', function (e) {
        e.preventDefault();
        menu.hidden = !menu.hidden;
      });

      document.addEventListener('click', function (e) {
        if (!menu.hidden && !menu.contains(e.target)) {
          menu.hidden = true;
        }
      });

      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !menu.hidden) {
          menu.hidden = true;
        }
      });
    }

    if (copySvgBtn) {
      copySvgBtn.addEventListener('click', function () {
        var svgMarkup = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64" fill="none"><path d="M13 24 C14.5 14 20 8 24.5 8 C28.2 8 30.8 13.5 33.5 16.2 C37.2 17.4 40.5 17.8 43.8 16.5 C47.2 14.2 50.8 12.2 53.5 13.2 C56.8 14.5 57.8 20.5 58.8 26.2 C61.8 31.8 62.5 38.2 61 44.5 C58.8 53.8 48.2 59.5 34.5 59.5 C19.5 59.5 8.2 52.2 6.5 41.2 C5.2 33.2 8.8 27.5 13 24 Z" fill="#211f29"/><path d="M22.5 27.5 L31.5 33.5 L22.5 39.5" stroke="#ffffff" stroke-width="4.6" stroke-linecap="round" stroke-linejoin="round"/><line x1="35.5" y1="38.5" x2="46.5" y2="38.5" stroke="#ffffff" stroke-width="4.6" stroke-linecap="round"/></svg>';
        copyText(svgMarkup, function () {
          announce('已复制 Mio SVG 图标到剪贴板');
          if (menu) menu.hidden = true;
        });
      });
    }

    if (heroStage && heroBadge) {
      var badges = ['>_ mio', '⚡ 2.5ms', '5.6MB RSS', '🦀 rust 2024', '⑂ 4 subagents'];
      var badgeIdx = 0;

      function triggerHeroPulse() {
        heroStage.classList.remove('is-pulsing');
        void heroStage.offsetWidth;
        heroStage.classList.add('is-pulsing');
        badgeIdx = (badgeIdx + 1) % badges.length;
        heroBadge.textContent = badges[badgeIdx];
      }

      heroStage.addEventListener('click', triggerHeroPulse);
      heroStage.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          triggerHeroPulse();
        }
      });
    }
  }

  // =========================================================================
  // 4. Multi-Tab Install Command Switcher & Universal Clipboard
  // =========================================================================
  var installHints = {
    cargo: { zh: '需要 Rust 1.95+ · 锁定依赖构建', en: 'Requires Rust 1.95+ · Locked build' },
    curl: { zh: 'Linux & macOS 预编译脚本 · 精确 SHA256 校验（等待发布）', en: 'Linux & macOS prebuilt script · Exact SHA256 (Pending release)' },
    powershell: { zh: 'Windows PowerShell 脚本 · 自动维护 PATH（等待发布 · Windows 未实测）', en: 'Windows PowerShell script · Auto PATH (Pending release · Untested on Windows)' },
    npm: { zh: 'Node.js >=18 全局安装 · 首启自动校验缓存二进制（等待发布）', en: 'Node.js >=18 global install · Auto-validating cache on first run (Pending release)' },
    pnpm: { zh: 'pnpm 全局安装 · 首启自动获取原生二进制（等待发布）', en: 'pnpm global install · Fetches native binary on first run (Pending release)' },
    bun: { zh: 'Bun 全局安装 · 运行 wrapper 需环境具备 node（等待发布）', en: 'Bun global install · Wrapper requires node in PATH (Pending release)' },
    antigravity: { zh: '复用本机已有 Antigravity OAuth 登录凭证（权限 0600）', en: 'Reuses local Antigravity OAuth profile (chmod 0600)' },
    openai: { zh: '兼容任意支持流式工具调用的云端或本地 Ollama / vLLM 服务', en: 'Works with any streaming OpenAI-compatible or local Ollama endpoint' },
    thinking: { zh: '支持 [provider/]model[:thinking] 简写与 4 路并发子代理', en: 'Supports [provider/]model[:thinking] spec & 4 concurrent subagents' },
    crate: { zh: '直接作为 Rust 库嵌入宿主进程，共享 Tokio 运行时', en: 'Embed directly as a Rust crate sharing your Tokio runtime' }
  };

  function setupInstallSwitcher() {
    var tabs = Array.prototype.slice.call(document.querySelectorAll('[data-install-tab]'));
    var panels = Array.prototype.slice.call(document.querySelectorAll('[data-install-panel]'));
    var hintEl = document.getElementById('installTabHint');
    var heroCopyBtn = document.getElementById('heroCopyBtn');

    if (!tabs.length) return;

    function activateInstallTab(tabKey) {
      tabs.forEach(function (t) {
        var active = t.getAttribute('data-install-tab') === tabKey;
        t.classList.toggle('is-active', active);
        t.setAttribute('aria-selected', active ? 'true' : 'false');
        t.tabIndex = active ? 0 : -1;
      });
      panels.forEach(function (p) {
        var active = p.getAttribute('data-install-panel') === tabKey;
        p.hidden = !active;
      });
      if (hintEl && installHints[tabKey]) {
        hintEl.dataset.zh = installHints[tabKey].zh;
        hintEl.dataset.en = installHints[tabKey].en;
        hintEl.textContent = currentLang === 'en' ? installHints[tabKey].en : installHints[tabKey].zh;
      }
    }

    tabs.forEach(function (tab, index) {
      // Ensure initial roving tabindex
      var isSelected = tab.getAttribute('aria-selected') === 'true';
      tab.tabIndex = isSelected ? 0 : -1;

      tab.addEventListener('click', function () {
        activateInstallTab(tab.getAttribute('data-install-tab'));
      });

      tab.addEventListener('keydown', function (e) {
        var newIndex = -1;
        if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
          newIndex = (index + 1) % tabs.length;
        } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
          newIndex = (index - 1 + tabs.length) % tabs.length;
        } else if (e.key === 'Home') {
          newIndex = 0;
        } else if (e.key === 'End') {
          newIndex = tabs.length - 1;
        }

        if (newIndex !== -1) {
          e.preventDefault();
          tabs[newIndex].focus();
          activateInstallTab(tabs[newIndex].getAttribute('data-install-tab'));
        }
      });
    });

    if (heroCopyBtn) {
      heroCopyBtn.addEventListener('click', function () {
        var activePanel = document.querySelector('[data-install-panel]:not([hidden])');
        if (!activePanel) return;
        var clone = activePanel.cloneNode(true);
        var prompts = clone.querySelectorAll('.prompt-sign');
        prompts.forEach(function (p) { p.remove(); });
        var raw = (clone.textContent || '').trim();
        copyText(raw, function () {
          flashCopyButton(heroCopyBtn, currentLang === 'en' ? 'Copied ✓' : '已复制 ✓');
        });
      });
    }
  }

  function setupStepCopyButtons() {
    var btns = document.querySelectorAll('[data-copy-target]');
    btns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var targetId = btn.getAttribute('data-copy-target');
        var el = document.getElementById(targetId);
        if (!el) return;
        var text = (el.innerText || el.textContent || '').trim();
        copyText(text, function () {
          var orig = btn.textContent;
          btn.textContent = currentLang === 'en' ? 'Copied ✓' : '已复制 ✓';
          setTimeout(function () {
            btn.textContent = orig;
          }, 1800);
        });
      });
    });
  }

  function flashCopyButton(btn, msg) {
    var orig = currentLang === 'en' ? (btn.dataset.en || 'Copy') : (btn.dataset.zh || '复制命令');
    btn.textContent = msg;
    btn.classList.add('is-copied');
    announce('命令已复制到剪贴板');
    setTimeout(function () {
      btn.textContent = orig;
      btn.classList.remove('is-copied');
    }, 1800);
  }

  function copyText(text, onSuccess) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(onSuccess, function () {
        fallbackCopyText(text, onSuccess);
      });
    } else {
      fallbackCopyText(text, onSuccess);
    }
  }

  function fallbackCopyText(text, onSuccess) {
    try {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      document.body.removeChild(ta);
      if (onSuccess) onSuccess();
    } catch (e) {}
  }

  // =========================================================================
  // 5. Pi-Style Scrollytelling Interactive Showcase (#showcase)
  // =========================================================================
  var scenariosData = {
    edit: {
      titleZh: '01 · 有界检索与精确替换 (edit + diff)',
      titleEn: '01 · Bounded Search & Exact Edit (edit + diff)',
      captionZh: '所有文件工具均经过 checked_path 校验，拒绝绝对路径、.. 越界与符号链接穿透。',
      captionEn: 'All file tools validate paths via checked_path, rejecting absolute paths, .. traversal, and symlinks.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--cwd</span> ./mio <span class="flag">-p</span> <span class="arg">\'检查 src/workspace.rs 的编辑边界，确保多处匹配时拒绝修改\'</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">grep</span> <code>pattern="execute_edit" path="src/workspace.rs"</code> → 命中第 218 行 (Thompson NFA 有界检索)</div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">read</span> <code>path="src/workspace.rs" offset=215 limit=45</code> (读取 1.8 KiB / 64 KiB 上限)</div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">edit</span> <code>path="src/workspace.rs"</code> 唯一匹配校验通过，生成 Unified Diff：</div>',
        '<div class="term-diff-box"><span class="diff-hdr">@@ -234,4 +234,5 @@ impl Workspace</span>\n<span class="diff-del">-    if matches.len() != 1 { return Err("需唯一匹配".into()); }</span>\n<span class="diff-add">+    if matches.is_empty() { return Err("未找到 old_text 匹配项".into()); }</span>\n<span class="diff-add">+    if matches.len() &gt; 1 { return Err(format!("匹配到 {} 处，拒绝歧义替换", matches.len())); }</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-text">Mio</span> 已完成精确替换并通过 <code>cargo test --test workspace</code>，会话已原子保存至 <code>.mio/sessions/20261014-01.json</code>。</div>'
      ],
      jsonl: [
        '{"type":"tool_started","data":{"operation":"grep","target":"src/workspace.rs"}}',
        '{"type":"tool_finished","data":{"operation":"grep","target":"src/workspace.rs","result":{"Ok":"src/workspace.rs:218: async fn execute_edit..."}}}',
        '{"type":"tool_started","data":{"operation":"read","target":"src/workspace.rs"}}',
        '{"type":"tool_finished","data":{"operation":"read","target":"src/workspace.rs","result":{"Ok":"215 | impl Workspace {\\n..."}}}',
        '{"type":"tool_started","data":{"operation":"edit","target":"src/workspace.rs"}}',
        '{"type":"tool_finished","data":{"operation":"edit","target":"src/workspace.rs","result":{"Ok":"@@ -234,4 +234,5 @@..."}}}',
        '{"type":"text","data":"已完成精确替换并通过测试验证。"}',
        '{"type":"done","data":{"session":".mio/sessions/20261014-01.json","messages":6}}'
      ],
      rust: [
        '<span class="c-kw">use</span> mio_agent::{Workspace, Arguments, Host};',
        '',
        '<span class="c-com">// 绑定规范化工作目录，自动防御 .. 越界与符号链接</span>',
        '<span class="c-kw">let</span> ws = <span class="c-typ">Workspace</span>::<span class="c-fn">new</span>(&amp;cwd, host.<span class="c-fn">clone</span>())?;',
        '<span class="c-kw">let</span> diff = ws.<span class="c-fn">execute</span>(<span class="c-typ">Arguments</span> {',
        '    operation: <span class="c-str">"edit"</span>.<span class="c-fn">into</span>(),',
        '    path: <span class="c-typ">Some</span>(<span class="c-str">"src/workspace.rs"</span>.<span class="c-fn">into</span>()),',
        '    old_text: <span class="c-typ">Some</span>(<span class="c-str">"if matches.len() != 1"</span>.<span class="c-fn">into</span>()),',
        '    new_text: <span class="c-typ">Some</span>(<span class="c-str">"if matches.is_empty()"</span>.<span class="c-fn">into</span>()),',
        '    ..<span class="c-typ">Default</span>::<span class="c-fn">default</span>()',
        '}).<span class="c-kw">await</span>?;'
      ]
    },
    subagents: {
      titleZh: '02 · 原生并发子代理与目录读写锁 (--subagents)',
      titleEn: '02 · Native Concurrent Subagents & Directory Locks',
      captionZh: '最多 4 个并发子代理共享同一 Rust 进程；只读探索并行执行，重叠目录写入自动独占。',
      captionEn: 'Up to 4 concurrent subagents share one Rust process; read-only tasks run in parallel while writes lock overlapping paths.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--subagents</span> <span class="flag">-p</span> <span class="arg">\'并行委派两个只读子代理审计代码结构与测试缺口，汇总后补齐单测\'</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">subagent:delegate</span> <code>session="scan-arch" read_only=true</code> → <code>run_id="9b1e...4f2a"</code> (Running)</div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">subagent:delegate</span> <code>session="audit-tests" read_only=true</code> → <code>run_id="3c8a...7d10"</code> (Running)</div>',
        '<div class="term-line"><span class="term-event-badge badge-lock">lock:shared-read</span> 2 个只读子任务在 <code>./src</code> 与 <code>./tests</code> 并发执行，总内存增量仅 <strong>+1.8 MB</strong></div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">subagent:status</span> 长轮询等待完成：<code>scan-arch (Completed)</code> · <code>audit-tests (Completed)</code></div>',
        '<div class="term-line"><span class="term-event-badge badge-lock">lock:exclusive-write</span> 主 Agent 获取 <code>tests/subagent.rs</code> 独占写锁，安全写入新增断言。</div>'
      ],
      jsonl: [
        '{"type":"tool_started","data":{"operation":"subagent","target":"delegate:scan-arch"}}',
        '{"type":"tool_finished","data":{"operation":"subagent","target":"delegate:scan-arch","result":{"Ok":"{\\"runId\\":\\"9b1e4f2a\\",\\"status\\":\\"running\\",\\"readOnly\\":true}"}}}',
        '{"type":"tool_started","data":{"operation":"subagent","target":"delegate:audit-tests"}}',
        '{"type":"tool_finished","data":{"operation":"subagent","target":"delegate:audit-tests","result":{"Ok":"{\\"runId\\":\\"3c8a7d10\\",\\"status\\":\\"running\\",\\"readOnly\\":true}"}}}',
        '{"type":"tool_started","data":{"operation":"subagent","target":"status:9b1e4f2a"}}',
        '{"type":"tool_finished","data":{"operation":"subagent","target":"status:9b1e4f2a","result":{"Ok":"{\\"status\\":\\"completed\\",\\"lastTool\\":\\"grep\\"}"}}}',
        '{"type":"text","data":"两个只读子任务已完成汇总，关键并发边界测试已补齐。"}'
      ],
      rust: [
        '<span class="c-kw">use</span> mio_agent::{run_with_tools, RunTools, subagent::SubagentManager};',
        '',
        '<span class="c-com">// 创建进程内子代理管理器（最多 4 并发，自动协调目录读写锁）</span>',
        '<span class="c-kw">let</span> manager = <span class="c-typ">SubagentManager</span>::<span class="c-fn">new</span>(config.<span class="c-fn">clone</span>(), &amp;cwd, options.<span class="c-fn">clone</span>())?;',
        '<span class="c-kw">let</span> tools = <span class="c-typ">RunTools</span> {',
        '    options,',
        '    read_only: <span class="c-kw">false</span>,',
        '    subagents: <span class="c-typ">Some</span>(manager),',
        '    ..<span class="c-typ">Default</span>::<span class="c-fn">default</span>()',
        '};',
        '<span class="c-kw">let</span> history = <span class="c-fn">run_with_tools</span>(config, history, prompt, &amp;cwd, host, services, tools).<span class="c-kw">await</span>?;'
      ]
    },
    context: {
      titleZh: '03 · 上下文工程：AGENTS.md、Skills 与模板',
      titleEn: '03 · Context Engineering: AGENTS.md, Skills & Templates',
      captionZh: 'Skills 默认只向模型提供单行摘要，正文按需读取，避免长文档挤占上下文窗口。',
      captionEn: 'Skills inject compact one-line summaries by default; full SKILL.md bodies are read on demand.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--skill</span> code-review <span class="flag">--template</span> audit <span class="flag">-p</span> <span class="arg">\'检查最近一次提交的错误处理\'</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-text">context:load</span> 自动按祖先层级加载 <code>./AGENTS.md</code> + <code>.mio/SYSTEM.md</code> (共 2.1 KiB / 128 KiB 预算)</div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">skills:index</span> 发现 3 个项目 Skills，按需挂载 <code>.mio/skills/code-review/SKILL.md</code></div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">template</span> 展开 <code>.mio/prompts/audit.md</code> 中的 <code>$ARGUMENTS</code> 占位符</div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">grep</span> <code>pattern="map_err" path="src/"</code> → 遵循项目 AGENTS.md 规范执行核对</div>',
        '<div class="term-line"><span class="term-event-badge badge-text">Mio</span> 审计完成：所有外部错误均已收敛为用户可读提示，未暴露底层堆栈。</div>'
      ],
      jsonl: [
        '{"type":"tool_started","data":{"operation":"read","target":".mio/skills/code-review/SKILL.md"}}',
        '{"type":"tool_finished","data":{"operation":"read","target":".mio/skills/code-review/SKILL.md","result":{"Ok":"---\\nname: code-review\\n..."}}}',
        '{"type":"tool_started","data":{"operation":"grep","target":"src/"}}',
        '{"type":"tool_finished","data":{"operation":"grep","target":"src/","result":{"Ok":"src/lib.rs:241: .map_err..."}}}',
        '{"type":"text","data":"已按 AGENTS.md 与 code-review 技能规范完成错误处理核查。"}'
      ],
      rust: [
        '<span class="c-kw">use</span> mio_agent::{context::ProjectContext, run_with_options, RunOptions};',
        '',
        '<span class="c-com">// 显式加载工作区 AGENTS.md、.mio/SYSTEM.md 与指定 Skills</span>',
        '<span class="c-kw">let</span> ctx = <span class="c-typ">ProjectContext</span>::<span class="c-fn">load</span>(&amp;cwd, &amp;[<span class="c-str">"code-review"</span>.<span class="c-fn">into</span>()])?;',
        '<span class="c-kw">let</span> prompt = ctx.<span class="c-fn">expand_template</span>(&amp;cwd, <span class="c-str">"audit"</span>, <span class="c-str">"检查错误处理"</span>)?;',
        '<span class="c-kw">let</span> opts = <span class="c-typ">RunOptions</span> {',
        '    instructions: ctx.<span class="c-fn">instructions</span>().<span class="c-fn">to_owned</span>(),',
        '    max_turns: <span class="c-num">32</span>,',
        '    ..<span class="c-typ">Default</span>::<span class="c-fn">default</span>()',
        '};'
      ]
    },
    session: {
      titleZh: '04 · 会话分叉 (--fork)、消息树 (/tree) 与压缩 (/compact)',
      titleEn: '04 · Session Forking (--fork), /tree & Compaction (/compact)',
      captionZh: '/compact 自动寻找非工具结果的纯用户消息边界切分，绝不破坏 tool_call / tool_result 配对。',
      captionEn: '/compact splits at plain user-message boundaries so tool-call/tool-result pairs are never broken.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--resume</span> <span class="flag">--fork</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">session:fork</span> 已从 <code>.mio/sessions/20261014-01.json</code> 克隆新分支 <code>20261014-02.json</code> (chmod 0600)</div>',
        '<div class="term-line term-cmd"><span class="ps1">mio&gt;</span>/tree</div>',
        '<div class="term-line">  [0] User: 检查 src/workspace.rs 的编辑边界<br>  [1] Assistant (tools: grep, read, edit) → 唯一匹配校验通过<br>  <span style="color:#6ee7b7;">★ [2] Current Branch Head (24 messages, 48.2 KiB)</span></div>',
        '<div class="term-line term-cmd"><span class="ps1">mio&gt;</span>/compact 保留核心接口签名与测试结论</div>',
        '<div class="term-line"><span class="term-event-badge badge-lock">compact:done</span> 历史已从 <strong>24 条 (48.2 KiB)</strong> 压缩至 <strong>8 条 (9.4 KiB)</strong>，近期 6 条消息原样保留。</div>'
      ],
      jsonl: [
        '{"event":"session_forked","source":".mio/sessions/20261014-01.json","target":".mio/sessions/20261014-02.json"}',
        '{"event":"compaction_completed","original_messages":24,"compacted_messages":8,"original_bytes":49356,"compacted_bytes":9625}',
        '{"type":"text","data":"已在新分支中基于压缩后的上下文继续探索。"}'
      ],
      rust: [
        '<span class="c-kw">use</span> mio_agent::{compact, session::Session};',
        '',
        '<span class="c-com">// 恢复最近会话并创建无损分支，按安全边界压缩早期历史</span>',
        '<span class="c-kw">let</span> loaded = <span class="c-typ">Session</span>::<span class="c-fn">load_latest</span>(&amp;cwd)?;',
        '<span class="c-kw">let</span> (compacted, stats) = compact::<span class="c-fn">compact_history</span>(',
        '    &amp;loaded.messages,',
        '    compact::DEFAULT_KEEP_RECENT_MESSAGES,',
        '    <span class="c-typ">Some</span>(<span class="c-str">"保留接口变更与测试状态"</span>),',
        ');',
        '<span class="c-typ">Session</span>::<span class="c-fn">save_new</span>(&amp;cwd, &amp;compacted)?;'
      ]
    },
    providers: {
      titleZh: '05 · 多模型路由、Antigravity 导入与 7 档思考预算',
      titleEn: '05 · Multi-Provider Routing, Antigravity OAuth & 7 Thinking Levels',
      captionZh: '支持 off / minimal / low / medium / high / xhigh / max 七档思考强度与实时模型切换。',
      captionEn: 'Supports 7 thinking budget levels (off to max) and mid-session /model & /thinking switching.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--import-antigravity</span> ~/.pi/agent</div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">oauth:import</span> 已解析 <code>auth.json</code> 与模型设置，安全保存至 <code>~/.config/mio/antigravity-profile.json</code> (0600)</div>',
        '<div class="term-line term-cmd"><span class="ps1">$</span>mio <span class="flag">--model</span> <span class="arg">antigravity/claude-sonnet-4-6:high</span></div>',
        '<div class="term-line"><span class="term-event-badge badge-text">model:active</span> Provider: <code>antigravity</code> · Model: <code>claude-sonnet-4-6</code> · Thinking: <code>high</code></div>',
        '<div class="term-line term-cmd"><span class="ps1">mio&gt;</span>/thinking xhigh</div>',
        '<div class="term-line"><span class="term-event-badge badge-lock">thinking:updated</span> 思考预算已切换为 <code>xhigh</code>；主 Agent 与子代理自动共享令牌刷新服务。</div>'
      ],
      jsonl: [
        '{"profile":"~/.config/mio/antigravity-profile.json","permissions":"0600","provider":"antigravity"}',
        '{"spec":"antigravity/claude-sonnet-4-6:high","parsed":{"provider":"antigravity","model":"claude-sonnet-4-6","thinking":"high"}}',
        '{"type":"text","data":"已使用高阶思考预算完成复杂并发死锁推演。"}'
      ],
      rust: [
        '<span class="c-kw">use</span> mio_agent::{parse_model_spec, ThinkingLevel, Provider, Config};',
        '',
        '<span class="c-com">// 解析 [provider/]model[:thinking] 简写规格</span>',
        '<span class="c-kw">let</span> (prov, model, thinking) = <span class="c-fn">parse_model_spec</span>(<span class="c-str">"deepseek/deepseek-reasoner:high"</span>);',
        '<span class="c-macro">assert_eq!</span>(prov.<span class="c-fn">as_deref</span>(), <span class="c-typ">Some</span>(<span class="c-str">"deepseek"</span>));',
        '<span class="c-macro">assert_eq!</span>(thinking, <span class="c-typ">Some</span>(<span class="c-typ">ThinkingLevel</span>::High));'
      ]
    },
    embed: {
      titleZh: '06 · 进程内嵌入 Rust 宿主与双向控制 (Host Trait)',
      titleEn: '06 · In-Process Rust SDK & Host Control (Host Trait)',
      captionZh: '宿主不仅能实时消费强类型 AgentEvent，还能通过 approve_tool 与 poll_steering 掌控执行。',
      captionEn: 'Hosts consume typed AgentEvents and control execution via approve_tool and poll_steering.',
      cliSteps: [
        '<div class="term-line term-cmd"><span class="ps1">$</span>cargo run <span class="flag">--example</span> host</div>',
        '<div class="term-line"><span class="term-event-badge badge-text">Host::init</span> 进程内挂载 <code>ExampleHost</code>，配置 <code>ToolFilter::from_cli(Some("+read,+edit"), Some("bash"), false)</code></div>',
        '<div class="term-line"><span class="term-event-badge badge-tool">on_event</span> 收到 <code>AgentEvent::ToolStarted { operation: "edit", target: "src/main.rs" }</code></div>',
        '<div class="term-line"><span class="term-event-badge badge-lock">approve_tool</span> 宿主安全策略审批通过：允许修改 <code>src/main.rs</code></div>',
        '<div class="term-line"><span class="term-event-badge badge-sub">poll_steering</span> 检测到用户实时追加指令：<code>"同时更新模块顶部文档注释"</code>，自动注入下一轮上下文</div>',
        '<div class="term-line"><span class="term-event-badge badge-text">on_event</span> 流式输出完成，返回完整 <code>Vec&lt;Message&gt;</code> 供宿主 UI 持久化。</div>'
      ],
      jsonl: [
        '{"type":"tool_started","data":{"operation":"edit","target":"src/main.rs"}}',
        '{"host_callback":"approve_tool","operation":"edit","target":"src/main.rs","decision":"Allow"}',
        '{"host_callback":"poll_steering","injected":"同时更新模块顶部文档注释"}',
        '{"type":"tool_finished","data":{"operation":"edit","target":"src/main.rs","result":{"Ok":"Updated 1 hunk"}}}',
        '{"type":"text","data":"已按追加指令同步更新文档注释。"}'
      ],
      rust: [
        '<span class="c-kw">impl</span> <span class="c-typ">Host</span> <span class="c-kw">for</span> <span class="c-typ">RavomiDesktopHost</span> {',
        '    <span class="c-kw">fn</span> <span class="c-fn">shell</span>(&amp;<span class="c-kw">self</span>, cmd: &amp;<span class="c-typ">str</span>, cwd: &amp;<span class="c-typ">Path</span>) -&gt; <span class="c-typ">HostFuture</span>&lt;<span class="c-typ">Result</span>&lt;<span class="c-typ">String</span>, <span class="c-typ">String</span>&gt;&gt; {',
        '        mio_agent::shell::<span class="c-fn">execute</span>(cmd, cwd)',
        '    }',
        '    <span class="c-kw">fn</span> <span class="c-fn">approve_tool</span>(&amp;<span class="c-kw">self</span>, op: &amp;<span class="c-typ">str</span>, target: &amp;<span class="c-typ">str</span>) -&gt; <span class="c-typ">Result</span>&lt;(), <span class="c-typ">String</span>&gt; {',
        '        <span class="c-kw">self</span>.policy.<span class="c-fn">verify</span>(op, target) <span class="c-com">// 宿主自定义安全审批门控</span>',
        '    }',
        '    <span class="c-kw">fn</span> <span class="c-fn">poll_steering</span>(&amp;<span class="c-kw">self</span>) -&gt; <span class="c-typ">Option</span>&lt;<span class="c-typ">String</span>&gt; {',
        '        <span class="c-kw">self</span>.steering_rx.<span class="c-fn">try_recv</span>().<span class="c-fn">ok</span>() <span class="c-com">// 运行中实时注入用户纠偏</span>',
        '    }',
        '    <span class="c-kw">fn</span> <span class="c-fn">on_event</span>(&amp;<span class="c-kw">self</span>, ev: <span class="c-typ">AgentEvent</span>) { <span class="c-kw">self</span>.ui.<span class="c-fn">emit</span>(ev); }',
        '}'
      ]
    }
  };

  function renderStageScenario(scenarioKey, resetStep) {
    var data = scenariosData[scenarioKey] || scenariosData.edit;
    currentScenario = scenarioKey;

    var titleEl = document.getElementById('stageScenarioTitle');
    var captionEl = document.getElementById('stageCaptionText');
    var viewport = document.getElementById('stageViewport');
    var progressFill = document.getElementById('stageProgressFill');
    var stepCounter = document.getElementById('stageStepCounter');

    if (titleEl) {
      titleEl.textContent = currentLang === 'en' ? data.titleEn : data.titleZh;
    }
    if (captionEl) {
      captionEl.textContent = currentLang === 'en' ? data.captionEn : data.captionZh;
    }

    // Sync active state on scenario chips and chapter cards
    document.querySelectorAll('[data-scenario-btn]').forEach(function (btn) {
      btn.classList.toggle('is-active', btn.getAttribute('data-scenario-btn') === scenarioKey);
    });
    document.querySelectorAll('.story-chapter[data-scenario]').forEach(function (card) {
      card.classList.toggle('is-active', card.getAttribute('data-scenario') === scenarioKey);
    });

    if (!viewport) return;

    if (currentStageMode === 'jsonl') {
      clearStepTimer();
      var jsonHtml = '<pre class="code-view-pre">' + data.jsonl.map(function (line) {
        return '<span class="c-str">' + escapeHtml(line) + '</span>';
      }).join('\n') + '</pre>';
      viewport.innerHTML = jsonHtml;
      if (progressFill) progressFill.style.width = '100%';
      if (stepCounter) stepCounter.textContent = data.jsonl.length + ' JSONL events';
      return;
    }

    if (currentStageMode === 'rust') {
      clearStepTimer();
      viewport.innerHTML = '<pre class="code-view-pre">' + data.rust.join('\n') + '</pre>';
      if (progressFill) progressFill.style.width = '100%';
      if (stepCounter) stepCounter.textContent = 'examples/host.rs';
      return;
    }

    // CLI animated transcript mode
    var total = data.cliSteps.length;
    if (resetStep === 'replay') {
      currentStepIndex = 1;
      renderCliStepsUpTo(data.cliSteps, currentStepIndex);
      if (isPlaying) {
        scheduleNextCliStep();
      }
      return;
    }

    currentStepIndex = total;
    renderCliStepsUpTo(data.cliSteps, currentStepIndex);
  }

  function renderCliStepsUpTo(steps, count) {
    var viewport = document.getElementById('stageViewport');
    var progressFill = document.getElementById('stageProgressFill');
    var stepCounter = document.getElementById('stageStepCounter');
    if (!viewport) return;

    var total = steps.length;
    var visibleCount = Math.max(1, Math.min(total, count));
    viewport.innerHTML = steps.slice(0, visibleCount).join('');

    if (progressFill) {
      progressFill.style.width = ((visibleCount / total) * 100).toFixed(1) + '%';
    }
    if (stepCounter) {
      stepCounter.textContent = visibleCount + ' / ' + total + ' steps';
    }
  }

  function clearStepTimer() {
    if (stepTimer) {
      clearTimeout(stepTimer);
      stepTimer = null;
    }
  }

  function scheduleNextCliStep() {
    clearStepTimer();
    if (!isPlaying || currentStageMode !== 'cli') return;
    var data = scenariosData[currentScenario] || scenariosData.edit;
    var total = data.cliSteps.length;
    var delay = Math.round(1150 / playbackSpeed);

    stepTimer = setTimeout(function () {
      if (currentStepIndex < total) {
        currentStepIndex += 1;
        renderCliStepsUpTo(data.cliSteps, currentStepIndex);
        scheduleNextCliStep();
      } else {
        // Pause briefly at full completion
        stepTimer = setTimeout(function () {
          if (isPlaying && currentStageMode === 'cli') {
            currentStepIndex = 1;
            renderCliStepsUpTo(data.cliSteps, currentStepIndex);
            scheduleNextCliStep();
          }
        }, 2600);
      }
    }, delay);
  }

  function setupShowcaseStage() {
    var modeBtns = document.querySelectorAll('[data-stage-mode]');
    var scenarioBtns = document.querySelectorAll('[data-scenario-btn]');
    var chapters = document.querySelectorAll('.story-chapter[data-scenario]');
    var playPauseBtn = document.getElementById('stagePlayPauseBtn');
    var replayBtn = document.getElementById('stageReplayBtn');
    var speedBtn = document.getElementById('stageSpeedBtn');
    var progressTrack = document.getElementById('stageProgressTrack');

    modeBtns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        currentStageMode = btn.getAttribute('data-stage-mode') || 'cli';
        modeBtns.forEach(function (b) {
          var active = b === btn;
          b.classList.toggle('is-active', active);
          b.setAttribute('aria-selected', active ? 'true' : 'false');
        });
        renderStageScenario(currentScenario, false);
      });
    });

    scenarioBtns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var key = btn.getAttribute('data-scenario-btn');
        renderStageScenario(key, true);
      });
    });

    chapters.forEach(function (card) {
      card.addEventListener('click', function () {
        var key = card.getAttribute('data-scenario');
        renderStageScenario(key, true);
      });
      card.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          renderStageScenario(card.getAttribute('data-scenario'), true);
        }
      });
    });

    if (playPauseBtn) {
      playPauseBtn.addEventListener('click', function () {
        isPlaying = !isPlaying;
        playPauseBtn.textContent = isPlaying ? '⏸ 暂停' : '▶ 播放';
        if (isPlaying) {
          var data = scenariosData[currentScenario] || scenariosData.edit;
          if (currentStepIndex >= data.cliSteps.length) {
            currentStepIndex = 1;
          }
          renderStageScenario(currentScenario, false);
        } else {
          clearStepTimer();
        }
      });
    }

    if (replayBtn) {
      replayBtn.addEventListener('click', function () {
        isPlaying = true;
        if (playPauseBtn) playPauseBtn.textContent = '⏸ 暂停';
        currentStageMode = 'cli';
        modeBtns.forEach(function (b) {
          var active = b.getAttribute('data-stage-mode') === 'cli';
          b.classList.toggle('is-active', active);
          b.setAttribute('aria-selected', active ? 'true' : 'false');
        });
        renderStageScenario(currentScenario, 'replay');
      });
    }

    if (speedBtn) {
      speedBtn.addEventListener('click', function () {
        playbackSpeed = playbackSpeed === 1 ? 2 : 1;
        speedBtn.textContent = playbackSpeed + '×';
        if (isPlaying) scheduleNextCliStep();
      });
    }

    if (progressTrack) {
      progressTrack.addEventListener('click', function (e) {
        if (currentStageMode !== 'cli') return;
        var rect = progressTrack.getBoundingClientRect();
        var ratio = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
        var data = scenariosData[currentScenario] || scenariosData.edit;
        currentStepIndex = Math.max(1, Math.ceil(ratio * data.cliSteps.length));
        renderCliStepsUpTo(data.cliSteps, currentStepIndex);
      });
    }

    // IntersectionObserver so scrolling chapters updates the sticky terminal stage
    if ('IntersectionObserver' in window && chapters.length && window.innerHeight < 2200) {
      var observer = new IntersectionObserver(function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting && entry.intersectionRatio >= 0.55) {
            var key = entry.target.getAttribute('data-scenario');
            if (key && key !== currentScenario) {
              renderStageScenario(key, false);
            }
          }
        });
      }, { threshold: [0.55] });

      chapters.forEach(function (c) {
        observer.observe(c);
      });
    }

    // Start initial scenario with all steps visible first so immediate inspection is rich,
    // then advance smoothly
    currentStepIndex = scenariosData.edit.cliSteps.length;
    renderStageScenario('edit', false);
  }

  // =========================================================================
  // 6. ThoughtDAG-Style Interactive Context & Subagent Graph Canvas (#story)
  // =========================================================================
  var dagActsInfo = {
    '0': {
      badge: 'ACT 00 · FULL ARCHITECTURE',
      eyebrow: '00 · 全链路架构视图',
      titleZh: '图不是事后装饰，而是每次工具调用与上下文流转的真实边界。',
      titleEn: 'The graph is not decoration — it is the actual execution and context boundary.',
      descZh: '主 Agent 与最多 4 个并发子代理共享同一个 Rust 进程与 Tokio 运行时：只读探索可并行推进，写任务在重叠目录上自动互斥，每次提交都可追溯、可分叉、可压缩。',
      descEn: 'Parent agent and up to 4 concurrent subagents share one Rust process and Tokio runtime: read-only tasks run in parallel, writes lock overlapping paths, and every turn is branchable.'
    },
    '1': {
      badge: 'ACT 01 · CONTEXT INGEST (~180 TOK)',
      eyebrow: '01 · 上下文装填协议',
      titleZh: '只把真正需要的项目规则与技能摘要装进请求。',
      titleEn: 'Only wire the project instructions and skill summaries you actually need.',
      descZh: '按祖先到当前工作区顺序加载 AGENTS.md 与 .mio/SYSTEM.md；项目 Skills 默认只注入单行摘要，模型按需调用 read 读取正文，彻底告别系统提示词膨胀。',
      descEn: 'Loads AGENTS.md and .mio/SYSTEM.md hierarchically; Skills inject one-line summaries first and load full SKILL.md bodies on demand.'
    },
    '2': {
      badge: 'ACT 02 · PARALLEL READ-ONLY SUBAGENTS',
      eyebrow: '02 · 只读并行子代理探索',
      titleZh: '探索支线并发推进，主对话上下文保持干净。',
      titleEn: 'Explore read-only branches in parallel while keeping the main context clean.',
      descZh: '子代理默认以 read_only: true 启动，并发执行 grep、find、read；子代理的中间探索过程保留在各自会话中，只将精炼结论带回主 Agent。',
      descEn: 'Subagents launch with read_only: true by default, running grep/find/read concurrently without polluting the parent conversation history.'
    },
    '3': {
      badge: 'ACT 03 · EXCLUSIVE WRITE LOCK & HOST GATE',
      eyebrow: '03 · 独占写锁与宿主审批',
      titleZh: '读取可以并行，修改必须唯一且受控。',
      titleEn: 'Reads can run in parallel; mutations are exclusive, verified, and gated.',
      descZh: '当主 Agent 或可写子代理执行 edit / write / bash 时，重叠工作目录自动施加写独占锁，经过 Host::approve_tool 审批并校验唯一匹配后才落盘。',
      descEn: 'When edit, write, or bash runs, overlapping directories acquire an exclusive write lock and pass through Host::approve_tool and unique-match checks.'
    },
    '4': {
      badge: 'ACT 04 · SESSION FORK & SAFE COMPACTION',
      eyebrow: '04 · 会话分叉与边界对齐压缩',
      titleZh: '随时从任意节点分叉实验，长对话自动归纳瘦身。',
      titleEn: 'Fork from any saved snapshot and compact long histories safely.',
      descZh: '每次成功轮次原子保存至 .mio/sessions/*.json (0600)。用 --resume --fork 开启新分支，用 /compact 在纯用户消息边界归纳旧轮次，绝不破坏工具调用配对。',
      descEn: 'Every turn saves atomically to .mio/sessions/*.json. Branch with --resume --fork and summarize older turns with /compact without splitting tool pairs.'
    }
  };

  function setupDagCanvas() {
    var actBtns = document.querySelectorAll('.dag-act-btn[data-act]');
    var nodes = document.querySelectorAll('.dag-node[data-node-acts]');
    var edges = document.querySelectorAll('.dag-edge[data-edge-acts]');
    var badgeEl = document.getElementById('dagActiveActBadge');
    var eyebrowEl = document.getElementById('dagBannerEyebrow');
    var titleEl = document.getElementById('dagBannerTitle');
    var descEl = document.getElementById('dagBannerDesc');

    if (!actBtns.length) return;

    function activateAct(actStr) {
      actBtns.forEach(function (btn) {
        btn.classList.toggle('is-active', btn.getAttribute('data-act') === actStr);
      });

      var isAll = actStr === '0';
      nodes.forEach(function (node) {
        var acts = (node.getAttribute('data-node-acts') || '').split(',');
        var match = isAll || acts.indexOf(actStr) !== -1;
        node.classList.toggle('is-dimmed', !match);
        node.classList.toggle('is-focused', !isAll && match);
      });

      edges.forEach(function (edge) {
        var acts = (edge.getAttribute('data-edge-acts') || '').split(',');
        var match = isAll || acts.indexOf(actStr) !== -1;
        edge.classList.toggle('is-dimmed', !match);
        edge.classList.toggle('is-highlighted', !isAll && match);
      });

      var info = dagActsInfo[actStr] || dagActsInfo['0'];
      if (badgeEl) badgeEl.textContent = info.badge;
      if (eyebrowEl) eyebrowEl.textContent = info.eyebrow;
      if (titleEl) titleEl.textContent = currentLang === 'en' ? info.titleEn : info.titleZh;
      if (descEl) descEl.textContent = currentLang === 'en' ? info.descEn : info.descZh;
    }

    actBtns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        activateAct(btn.getAttribute('data-act'));
      });
    });

    nodes.forEach(function (node) {
      node.addEventListener('click', function () {
        var firstAct = (node.getAttribute('data-node-acts') || '1').split(',')[0];
        activateAct(firstAct);
      });
    });
  }

  // =========================================================================
  // 7. Coding Agent Benchmark Interactive Chart & Matrix Filter (#benchmark)
  // =========================================================================
  var currentBenchMetric = 'startup';

  var benchMetricsData = {
    startup: {
      titleZh: 'CLI 冷启动与参数解析耗时 (<agent> --help)',
      titleEn: 'CLI Cold Startup Latency (<agent> --help)',
      unitZh: '单位：毫秒 (ms) · 越短越快 · Linux x86_64 hyperfine 实测',
      unitEn: 'Unit: milliseconds (ms) · Lower is faster · hyperfine measured',
      noteZh: '实测说明：在同一台 Linux x86_64 工作站对各工具实装版本运行 hyperfine -N --warmup 3 测得。',
      noteEn: 'Measured on the same Linux x86_64 workstation via hyperfine -N --warmup 3.',
      rows: [
        { name: 'Mio v0.1.0', winner: '1.0× 最快', stack: 'Rust 2024 · Native Binary', pct: 2.2, val: '2.5 ms', sub: '± 1.1 ms (929 runs)', isMio: true },
        { name: 'Codex CLI v0.162.1', winner: '', stack: 'Rust · Standalone musl', pct: 4.5, val: '9.0 ms', sub: '3.6× slower', isMio: false },
        { name: 'Claude Code v2.1.278', winner: '', stack: 'TypeScript · Bun Native Wrapper', pct: 21.0, val: '111.5 ms', sub: '44.6× slower', isMio: false },
        { name: 'Pi Coding Agent v1.2.0', winner: '', stack: 'TypeScript · Node.js V8', pct: 88.6, val: '520.6 ms', sub: '208.2× slower', isMio: false },
        { name: 'Aider v0.86.2', winner: '', stack: 'Python 3.12 · Venv Runtime', pct: 100.0, val: '587.5 ms', sub: '235.0× slower', isMio: false }
      ]
    },
    memory: {
      titleZh: '峰值常驻内存占用 (Peak RSS · /usr/bin/time -v)',
      titleEn: 'Peak Resident Memory Footprint (Peak RSS · /usr/bin/time -v)',
      unitZh: '单位：兆字节 (MB) · 越低越省内存 · 实测 Maximum resident set size',
      unitEn: 'Unit: Megabytes (MB) · Lower is better · Maximum resident set size',
      noteZh: '实测说明：使用 /usr/bin/time -v 测量进程最大常驻集 (Maximum resident set size)。Mio 仅需 5,732 KB (≈5.6 MB)。',
      noteEn: 'Measured via /usr/bin/time -v Maximum resident set size. Mio uses only 5,732 KB (~5.6 MB).',
      rows: [
        { name: 'Mio v0.1.0', winner: '1.0× 最低内存', stack: 'Rust 2024 · Zero GC Overhead', pct: 3.4, val: '5.6 MB', sub: '5,732 KB RSS', isMio: true },
        { name: 'Codex CLI v0.162.1', winner: '', stack: 'Rust · Standalone musl', pct: 15.4, val: '25.6 MB', sub: '4.6× higher', isMio: false },
        { name: 'Aider v0.86.2', winner: '', stack: 'Python 3.12 · Venv Runtime', pct: 43.7, val: '72.8 MB', sub: '13.0× higher', isMio: false },
        { name: 'Claude Code v2.1.278', winner: '', stack: 'TypeScript · Bun Native Wrapper', pct: 85.0, val: '141.6 MB', sub: '25.3× higher', isMio: false },
        { name: 'Pi Coding Agent v1.2.0', winner: '', stack: 'TypeScript · Node.js V8', pct: 100.0, val: '166.5 MB', sub: '29.7× higher', isMio: false }
      ]
    },
    disk: {
      titleZh: '安装体积与运行时磁盘占用 (Disk Footprint)',
      titleEn: 'Binary & Package Disk Footprint',
      unitZh: '单位：兆字节 (MB) · 越小越轻量 · 无需额外安装数百 MB 虚拟机',
      unitEn: 'Unit: Megabytes (MB) · Smaller is lighter · Zero external VM required',
      noteZh: '实测说明：Mio strip 后单文件仅 14 MB（默认 release 18 MB），不依赖外部 Node.js 或 Python 虚拟环境。',
      noteEn: 'Mio stripped binary is 14 MB (18 MB unstripped) with zero external Node.js or Python runtime dependencies.',
      rows: [
        { name: 'Mio v0.1.0', winner: '14 MB 单文件', stack: 'Stripped ELF Binary (Zero VM)', pct: 3.3, val: '14 MB', sub: '18 MB unstripped', isMio: true },
        { name: 'Pi Coding Agent v1.2.0', winner: '', stack: 'node_modules (+ Node.js Runtime)', pct: 35.5, val: '149 MB', sub: '+ ~100 MB Node.js', isMio: false },
        { name: 'Claude Code v2.1.278', winner: '', stack: 'Standalone Bundled Binary', pct: 53.3, val: '224 MB', sub: '16.0× larger', isMio: false },
        { name: 'Codex CLI v0.162.1', winner: '', stack: 'Standalone musl Binary', pct: 66.9, val: '281 MB', sub: '20.1× larger', isMio: false },
        { name: 'Aider v0.86.2', winner: '', stack: 'Python Venv + Site-Packages', pct: 100.0, val: '~420 MB', sub: '30.0× larger', isMio: false }
      ]
    },
    tokens: {
      titleZh: '默认系统提示词基础开销 (Base System Prompt Overhead)',
      titleEn: 'Default System Prompt Token Overhead',
      unitZh: '单位：Tokens / 请求 · 越少越省钱，留给代码上下文的空间越大',
      unitEn: 'Unit: Tokens per turn · Lower leaves more room for actual code context',
      noteZh: '对比说明：Mio 基础 preamble 仅 84 个汉字，配合 Skills 摘要按需加载，最大化保护 Prompt Cache 与有效窗口。',
      noteEn: 'Mio uses an 84-char base preamble plus on-demand skill summaries, maximizing effective context window.',
      rows: [
        { name: 'Mio v0.1.0', winner: '~180 tok 极简', stack: '84-char Preamble + On-Demand Skills', pct: 4.3, val: '~180 tok', sub: '99.8% 窗口留给代码', isMio: true },
        { name: 'Pi Coding Agent v1.2.0', winner: '', stack: 'Minimal Harness System Prompt', pct: 7.6, val: '~320 tok', sub: '1.8× overhead', isMio: false },
        { name: 'Codex CLI v0.162.1', winner: '', stack: 'Sandbox & Workflow Instructions', pct: 44.0, val: '~1,850 tok', sub: '10.3× overhead', isMio: false },
        { name: 'Aider v0.86.2', winner: '', stack: 'Edit Format & RepoMap Prompts', pct: 57.1, val: '~2,400 tok', sub: '13.3× overhead', isMio: false },
        { name: 'Claude Code v2.1.278', winner: '', stack: 'Built-in Task/Todo/Git/Safety Rules', pct: 100.0, val: '~4,200+ tok', sub: '23.3× overhead', isMio: false }
      ]
    },
    concurrency: {
      titleZh: '五路并发总内存占用 (1 主 Agent + 4 并发子代理)',
      titleEn: '5-Agent Concurrent Memory Footprint (1 Parent + 4 Subagents)',
      unitZh: '单位：兆字节 (MB) · 进程内 Tokio 异步任务 vs 派生多个外部进程',
      unitEn: 'Unit: Megabytes (MB) · In-process Tokio tasks vs spawning multiple OS processes',
      noteZh: '架构对比：Mio 的 --subagents 在同一进程内作为 Tokio 异步任务运行，共享连接池与凭证，5 路并发仅约 9.2 MB。',
      noteEn: 'Mio runs subagents as in-process Tokio tasks sharing HTTP pools and auth (~9.2 MB total for 5 concurrent sessions).',
      rows: [
        { name: 'Mio v0.1.0 (--subagents)', winner: '进程内异步复用', stack: '1 Process · 5 Tokio Tasks · Shared Pool', pct: 1.4, val: '~9.2 MB', sub: '单子任务增量 < 1 MB', isMio: true },
        { name: 'Codex CLI (5 instances)', winner: '', stack: 'Multi-process / Experimental Delegation', pct: 18.6, val: '~128 MB', sub: '13.9× memory', isMio: false },
        { name: 'Aider (5 instances)', winner: '', stack: '5 Python Venv Processes', pct: 52.8, val: '~364 MB', sub: '39.6× memory', isMio: false },
        { name: 'Claude Code (1 + 4 Task)', winner: '', stack: 'Bun Process + Parallel Sub-contexts', pct: 84.0, val: '~580 MB', sub: '63.0× memory', isMio: false },
        { name: 'Pi (5 tmux instances)', winner: '', stack: '5 Separate Node.js V8 Processes', pct: 100.0, val: '~690 MB', sub: '75.0× memory', isMio: false }
      ]
    }
  };

  function renderBenchmarkChart(metricKey) {
    var data = benchMetricsData[metricKey] || benchMetricsData.startup;
    currentBenchMetric = metricKey;

    var titleEl = document.getElementById('benchChartTitle');
    var unitEl = document.getElementById('benchChartUnit');
    var noteEl = document.getElementById('benchChartNote');
    var listEl = document.getElementById('benchBarsList');

    if (titleEl) titleEl.textContent = currentLang === 'en' ? data.titleEn : data.titleZh;
    if (unitEl) unitEl.textContent = currentLang === 'en' ? data.unitEn : data.unitZh;
    if (noteEl) noteEl.textContent = currentLang === 'en' ? data.noteEn : data.noteZh;

    document.querySelectorAll('[data-bench-metric]').forEach(function (btn) {
      var active = btn.getAttribute('data-bench-metric') === metricKey;
      btn.classList.toggle('is-active', active);
      btn.setAttribute('aria-selected', active ? 'true' : 'false');
    });

    if (!listEl) return;

    listEl.innerHTML = data.rows.map(function (row) {
      var winnerHtml = row.winner ? '<span class="winner-pill">' + escapeHtml(row.winner) + '</span>' : '';
      return (
        '<div class="bench-bar-row' + (row.isMio ? ' is-mio' : '') + '">' +
          '<div class="bench-agent-info">' +
            '<div class="bench-agent-name">' + escapeHtml(row.name) + ' ' + winnerHtml + '</div>' +
            '<div class="bench-agent-stack">' + escapeHtml(row.stack) + '</div>' +
          '</div>' +
          '<div class="bench-bar-track">' +
            '<div class="bench-bar-fill" style="width: ' + row.pct + '%;"></div>' +
          '</div>' +
          '<div class="bench-bar-value">' + escapeHtml(row.val) + ' <small>' + escapeHtml(row.sub) + '</small></div>' +
        '</div>'
      );
    }).join('');
  }

  function setupBenchmarks() {
    var metricBtns = document.querySelectorAll('[data-bench-metric]');
    metricBtns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        renderBenchmarkChart(btn.getAttribute('data-bench-metric'));
      });
    });

    var filterBtns = document.querySelectorAll('[data-matrix-filter]');
    var rows = document.querySelectorAll('#matrixTableBody tr[data-cat]');

    filterBtns.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var cat = btn.getAttribute('data-matrix-filter') || 'all';
        filterBtns.forEach(function (b) {
          b.classList.toggle('is-active', b === btn);
        });
        rows.forEach(function (row) {
          var rowCat = row.getAttribute('data-cat');
          row.hidden = !(cat === 'all' || rowCat === cat);
        });
      });
    });
  }

  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  // Initialize all interactive modules
  function init() {
    setupThemeToggle();
    setupLanguageToggle();
    setupLogoInteractions();
    setupInstallSwitcher();
    setupStepCopyButtons();
    setupShowcaseStage();
    setupDagCanvas();
    setupBenchmarks();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
