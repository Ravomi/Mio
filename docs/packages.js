// Mio Official Plugin Catalog — packages.js

(function () {
  'use strict';

  var currentLang = 'zh';
  var activeTopic = '';

  var BUILTIN_PLUGINS = [
    {
      name: 'git-workflow',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 1,
      publishedAt: '2026-10-14T02:45:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/git-workflow',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['git', 'commit', 'review', 'pr'],
      desc: {
        zh: '提供 Git 状态与变更摘要工具、Conventional Commits 提交模板与 PR 代码审查 Skill。',
        en: 'Git status and diff summary tool, Conventional Commits template, and PR review skill.'
      },
      previewCode: '$ mio --plugin-install git-workflow\nmio> /skill:git-review\n-> branch: main · 3 files changed, +142 -18 lines',
      capabilities: {
        tools: ['plugin__git_workflow__git_status_summary'],
        skills: ['git-review (skills/git-review/SKILL.md)'],
        prompts: ['commit (prompts/commit.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'git-workflow',
        version: '0.1.0',
        description: 'Git status/diff summary tool, commit message template, and PR review skill',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['git', 'commit', 'review', 'pr'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md']
      }
    },
    {
      name: 'rust-expert',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 2,
      publishedAt: '2026-10-14T02:48:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/rust-expert',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['rust', 'cargo', 'clippy', 'audit'],
      desc: {
        zh: '内置 Cargo check/clippy 严格诊断工具、unsafe 块扫描器、Rust 所有权与并发安全审计 Skill。',
        en: 'Cargo check/clippy diagnostic tool, unsafe block scanner, and Rust ownership/concurrency audit skill.'
      },
      previewCode: '$ mio --plugin-install rust-expert\nmio> /skill:rust-audit\n-> 0 warnings · 0 undocumented unsafe blocks',
      capabilities: {
        tools: ['plugin__rust_expert__cargo_diagnostics', 'plugin__rust_expert__cargo_unsafe_scan'],
        skills: ['rust-audit (skills/rust-audit/SKILL.md)'],
        prompts: ['bench (prompts/bench.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'rust-expert',
        version: '0.1.0',
        description: 'Cargo check/clippy diagnostic tool, Rust unsafe & ownership audit skill',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['rust', 'cargo', 'clippy', 'audit'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md']
      }
    },
    {
      name: 'security-audit',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 3,
      publishedAt: '2026-10-14T02:50:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/security-audit',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['security', 'audit', 'owasp', 'secrets'],
      desc: {
        zh: '扫描仓库中意外提交的 API Key 与私钥、执行 OWASP Top 10 边界安全审查并生成威胁模型。',
        en: 'Workspace secret scanner, OWASP Top 10 security review skill, and STRIDE threat model prompt.'
      },
      previewCode: '$ mio --plugin-install security-audit\nmio> /skill:security-review\n-> No hardcoded secret patterns matched in tracked files.',
      capabilities: {
        tools: ['plugin__security_audit__secret_scan'],
        skills: ['security-review (skills/security-review/SKILL.md)'],
        prompts: ['threat-model (prompts/threat-model.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'security-audit',
        version: '0.1.0',
        description: 'Workspace secret scanner tool, OWASP code review skill, and threat model prompt',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['security', 'audit', 'owasp', 'secrets']
      }
    },
    {
      name: 'mcp-github',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 4,
      publishedAt: '2026-10-14T03:04:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/mcp-github',
      types: ['mcp', 'tool', 'skill', 'prompt', 'context'],
      keywords: ['github', 'mcp', 'pr', 'issues', 'ci'],
      desc: {
        zh: '接入 GitHub 官方 MCP 服务与 gh PR 状态检查工具，提供 Issue 与 CI 故障分诊 Skill。',
        en: 'GitHub MCP server integration, gh PR status tool, and issue/CI triage skill.'
      },
      previewCode: '$ mio --plugin-install mcp-github\nmio> /skill:github-triage\n-> Connected MCP server: github (stdio JSON-RPC 2.0)',
      capabilities: {
        tools: ['plugin__mcp_github__gh_pr_status'],
        skills: ['github-triage (skills/github-triage/SKILL.md)'],
        prompts: ['pr (prompts/pr.md)'],
        mcp: ['mcp.json (@modelcontextprotocol/server-github)'],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'mcp-github',
        version: '0.1.0',
        description: 'GitHub CLI & MCP bridge for issues, pull requests, and workflow runs',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        mcp: 'mcp.json'
      }
    },
    {
      name: 'test-runner',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 5,
      publishedAt: '2026-10-14T03:05:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/test-runner',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['testing', 'tdd', 'cargo-test', 'pytest', 'coverage'],
      desc: {
        zh: '自动识别工作区测试框架（Cargo、Pytest、NPM、Go）并执行测试，附带 TDD 技能与回归测试模板。',
        en: 'Auto-detects and runs Cargo, Pytest, NPM, or Go test suites with a TDD skill.'
      },
      previewCode: '$ mio --plugin-install test-runner\nmio> /skill:tdd-workflow\n-> Running cargo test --locked ... 72 passed; 0 failed',
      capabilities: {
        tools: ['plugin__test_runner__detect_and_test'],
        skills: ['tdd-workflow (skills/tdd-workflow/SKILL.md)'],
        prompts: ['regression (prompts/regression.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'test-runner',
        version: '0.1.0',
        description: 'Polyglot test suite detector & runner, TDD skill, and regression test prompt',
        author: 'Ravomi Community',
        license: 'MIT'
      }
    },
    {
      name: 'perf-profiler',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 6,
      publishedAt: '2026-10-14T03:06:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/perf-profiler',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['performance', 'benchmark', 'profiling', 'memory', 'latency'],
      desc: {
        zh: '工作区体积与产物分析工具、冷启动延迟与峰值常驻内存（RSS）审计 Skill。',
        en: 'Workspace footprint analyzer, cold-start latency and peak RSS memory audit skill.'
      },
      previewCode: '$ mio --plugin-install perf-profiler\nmio> /skill:perf-audit\n-> Startup median: 2.5 ms · Peak RSS: 5.6 MB',
      capabilities: {
        tools: ['plugin__perf_profiler__workspace_footprint'],
        skills: ['perf-audit (skills/perf-audit/SKILL.md)'],
        prompts: ['hotspot (prompts/hotspot.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'perf-profiler',
        version: '0.1.0',
        description: 'Binary size, startup latency, RSS memory footprint analyzer',
        author: 'Ravomi Community',
        license: 'MIT'
      }
    },
    {
      name: 'docker-ops',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 7,
      publishedAt: '2026-10-14T03:07:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/docker-ops',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['docker', 'containers', 'devops', 'compose', 'ci'],
      desc: {
        zh: 'Docker / Podman 容器状态巡检工具、多阶段 Dockerfile 缓存优化与非 Root 安全加固 Skill。',
        en: 'Docker/Podman container status tool and multi-stage Dockerfile hardening skill.'
      },
      previewCode: '$ mio --plugin-install docker-ops\nmio> /skill:dockerfile-hardening\n-> Multi-stage build · non-root USER · layer cache verified',
      capabilities: {
        tools: ['plugin__docker_ops__container_ps'],
        skills: ['dockerfile-hardening (skills/dockerfile-hardening/SKILL.md)'],
        prompts: ['dockerfile (prompts/dockerfile.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'docker-ops',
        version: '0.1.0',
        description: 'Container status inspection tool and multi-stage Dockerfile hardening skill',
        author: 'Ravomi Community',
        license: 'MIT'
      }
    },
    {
      name: 'docs-writer',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 8,
      publishedAt: '2026-10-14T02:51:00Z',
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/docs-writer',
      types: ['skill', 'prompt', 'context'],
      keywords: ['docs', 'adr', 'architecture', 'markdown'],
      desc: {
        zh: '对照源码核验 CLI 参数与 API 签名的技术文档架构师 Skill 及 ADR 生成模板。',
        en: 'Source-verified technical documentation architect skill and ADR prompt template.'
      },
      previewCode: '$ mio --plugin-install docs-writer\nmio> /skill:docs-architect\n-> Verified CLI flags against src/bin/mio.rs · ADR ready',
      capabilities: {
        tools: [],
        skills: ['docs-architect (skills/docs-architect/SKILL.md)'],
        prompts: ['adr (prompts/adr.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'docs-writer',
        version: '0.1.0',
        description: 'Technical documentation architect skill and ADR prompt template',
        author: 'Ravomi Community',
        license: 'MIT'
      }
    }
  ];

  var catalogPlugins = BUILTIN_PLUGINS.slice();

  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function copyText(text, onDone) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(onDone).catch(function () {
        if (onDone) onDone();
      });
    } else if (onDone) {
      onDone();
    }
  }

  function getFilteredPlugins() {
    var searchEl = document.getElementById('packageSearchInput');
    var typeEl = document.getElementById('packageTypeSelect');
    var sortEl = document.getElementById('packageSortSelect');

    var q = ((searchEl && searchEl.value) || '').trim().toLowerCase();
    var selectedType = (typeEl && typeEl.value) || '';
    var sortBy = (sortEl && sortEl.value) || 'featured';

    var filtered = catalogPlugins.filter(function (item) {
      if (selectedType && item.types.indexOf(selectedType) === -1) return false;
      if (activeTopic) {
        var inKw = item.keywords.indexOf(activeTopic) !== -1;
        var inTp = item.types.indexOf(activeTopic) !== -1;
        var inNm = item.name.indexOf(activeTopic) !== -1;
        if (!inKw && !inTp && !inNm) return false;
      }
      if (!q) return true;
      var hay = [
        item.name,
        item.author,
        item.desc.zh,
        item.desc.en,
        item.keywords.join(' '),
        item.types.join(' ')
      ].join(' ').toLowerCase();
      return hay.indexOf(q) !== -1;
    });

    filtered.sort(function (a, b) {
      if (sortBy === 'name') return a.name.localeCompare(b.name);
      if (sortBy === 'recent') return String(b.publishedAt).localeCompare(String(a.publishedAt));
      return (a.featuredOrder || 99) - (b.featuredOrder || 99);
    });

    return filtered;
  }

  function renderPackagesGrid() {
    var grid = document.getElementById('packagesGrid');
    var countBadge = document.getElementById('packagesCountBadge');
    var emptyState = document.getElementById('packagesEmptyState');
    if (!grid) return;

    var items = getFilteredPlugins();
    if (countBadge) {
      countBadge.textContent = items.length + ' / ' + catalogPlugins.length;
    }

    if (!items.length) {
      grid.innerHTML = '';
      if (emptyState) emptyState.hidden = false;
      return;
    }
    if (emptyState) emptyState.hidden = true;

    grid.innerHTML = items.map(function (item) {
      var installCmd = 'mio --plugin-install ' + item.name;
      var descText = currentLang === 'en' ? item.desc.en : item.desc.zh;
      var copyLabel = currentLang === 'en' ? 'Copy install' : '复制安装命令';
      var detailLabel = currentLang === 'en' ? 'Inspect manifest' : '查看清单';

      return (
        '<article class="package-entry">' +
          '<div>' +
            '<div class="package-entry-top">' +
              '<button type="button" class="package-entry-name" data-open-plugin="' + escapeHtml(item.name) + '">' +
                escapeHtml(item.name) +
              '</button>' +
              '<span class="package-entry-meta">v' + escapeHtml(item.version) + ' · ' + escapeHtml(item.types.join(' / ')) + '</span>' +
            '</div>' +
            '<p class="muted" style="margin-top: 0.65rem;">' + escapeHtml(descText) + '</p>' +
          '</div>' +
          '<div data-ui>' +
            '<pre class="ui-terminal-pre"><code>' + escapeHtml(item.previewCode) + '</code></pre>' +
            '<div class="ui-action-row">' +
              '<button type="button" class="ui-btn" data-copy-cmd="' + escapeHtml(installCmd) + '">' + copyLabel + '</button>' +
              '<button type="button" class="ui-btn-secondary" data-open-plugin="' + escapeHtml(item.name) + '">' + detailLabel + '</button>' +
            '</div>' +
          '</div>' +
        '</article>'
      );
    }).join('');
  }

  function openPluginModal(name) {
    var dialog = document.getElementById('pluginDetailDialog');
    var titleEl = document.getElementById('modalPluginTitle');
    var subEl = document.getElementById('modalPluginSubtitle');
    var bodyEl = document.getElementById('modalPluginBody');
    if (!dialog || !bodyEl) return;

    var plugin = null;
    for (var i = 0; i < catalogPlugins.length; i++) {
      if (catalogPlugins[i].name === name) {
        plugin = catalogPlugins[i];
        break;
      }
    }
    if (!plugin) return;

    titleEl.textContent = plugin.name;
    subEl.textContent = 'v' + plugin.version + ' · ' + plugin.license + ' · ' + plugin.author;
    var desc = currentLang === 'en' ? plugin.desc.en : plugin.desc.zh;
    var cliInstall = 'mio --plugin-install ' + plugin.name;

    bodyEl.innerHTML =
      '<p class="muted">' + escapeHtml(desc) + '</p>' +
      '<div data-ui>' +
        '<div class="ui-header"><span class="ui-title">mio-plugin.json</span><span class="ui-status">' + escapeHtml(cliInstall) + '</span></div>' +
        '<pre class="ui-terminal-pre" style="margin-top: 0.75rem;"><code>' + escapeHtml(JSON.stringify(plugin.manifest, null, 2)) + '</code></pre>' +
      '</div>';

    if (typeof dialog.showModal === 'function') dialog.showModal();
    else dialog.setAttribute('open', '');
  }

  document.addEventListener('DOMContentLoaded', function () {
    var form = document.getElementById('packagesFilterForm');
    var searchEl = document.getElementById('packageSearchInput');
    var typeEl = document.getElementById('packageTypeSelect');
    var sortEl = document.getElementById('packageSortSelect');
    var resetBtn = document.getElementById('packageResetBtn');
    var pills = Array.prototype.slice.call(document.querySelectorAll('[data-topic]'));
    var dialog = document.getElementById('pluginDetailDialog');
    var closeBtn = document.getElementById('closePluginModalBtn');

    try {
      var params = new URLSearchParams(window.location.search);
      if (searchEl && params.get('q')) searchEl.value = params.get('q');
    } catch (e) {}

    if (form) form.addEventListener('submit', function (e) { e.preventDefault(); renderPackagesGrid(); });
    if (searchEl) searchEl.addEventListener('input', renderPackagesGrid);
    if (typeEl) typeEl.addEventListener('change', renderPackagesGrid);
    if (sortEl) sortEl.addEventListener('change', renderPackagesGrid);

    if (resetBtn) {
      resetBtn.addEventListener('click', function () {
        if (searchEl) searchEl.value = '';
        if (typeEl) typeEl.value = '';
        if (sortEl) sortEl.value = 'featured';
        activeTopic = '';
        pills.forEach(function (p) { p.classList.toggle('is-active', p.getAttribute('data-topic') === ''); });
        renderPackagesGrid();
      });
    }

    pills.forEach(function (pill) {
      pill.addEventListener('click', function () {
        activeTopic = pill.getAttribute('data-topic') || '';
        pills.forEach(function (p) { p.classList.toggle('is-active', p === pill); });
        renderPackagesGrid();
      });
    });

    if (closeBtn && dialog) {
      closeBtn.addEventListener('click', function () {
        if (typeof dialog.close === 'function') dialog.close();
        else dialog.removeAttribute('open');
      });
    }

    document.addEventListener('click', function (e) {
      var openBtn = e.target.closest('[data-open-plugin]');
      if (openBtn) {
        e.preventDefault();
        openPluginModal(openBtn.getAttribute('data-open-plugin'));
        return;
      }
      var copyBtn = e.target.closest('[data-copy-cmd]');
      if (copyBtn) {
        var cmd = copyBtn.getAttribute('data-copy-cmd');
        copyText(cmd, function () {
          var orig = copyBtn.textContent;
          copyBtn.textContent = currentLang === 'en' ? 'Copied' : '已复制';
          setTimeout(function () { copyBtn.textContent = orig; }, 1400);
        });
      }
    });

    var themeBtn = document.getElementById('themeToggleBtn');
    if (themeBtn) {
      themeBtn.addEventListener('click', function () {
        var cur = document.documentElement.dataset.theme || 'dark';
        var next = cur === 'dark' ? 'light' : 'dark';
        document.documentElement.dataset.theme = next;
        document.documentElement.dataset.themeMode = next;
        try { localStorage.setItem('mio:theme-v2', next); } catch (e) {}
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
        renderPackagesGrid();
      });
    }

    renderPackagesGrid();
  });
})();
