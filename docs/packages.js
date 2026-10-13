/**
 * Mio Official Plugin Catalog & Community Hub (packages.js)
 * Reference design: https://pi.dev/packages
 * Registry source: https://github.com/Ravomi/mio-plugins (registry.json)
 */
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
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/git-workflow',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['git', 'commit', 'review', 'pr'],
      desc: {
        zh: '提供工作区 Git 分支状态与变更统计工具、Conventional Commits 提交信息生成模板，以及结构化 PR 代码审查 Skill。',
        en: 'Git branch status & diff summary tool, Conventional Commits message template, and structured PR code review skill.'
      },
      previewTag: 'plugin__git_workflow__git_status_summary',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install git-workflow</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:git-review  <span class="hl-out"># 调用 git_status_summary + PR 审查清单</span>\n' +
        '<span class="hl-out">→ branch: main · 3 files changed, +142 -18 lines</span>',
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
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'git_status_summary',
            description: 'Inspect git branch, status, and diff stat in the current workspace',
            command: ['sh', '-c', 'git status -sb && git diff --stat']
          }
        ]
      }
    },
    {
      name: 'rust-expert',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 2,
      publishedAt: '2026-10-14T02:48:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/rust-expert',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['rust', 'cargo', 'clippy', 'audit'],
      desc: {
        zh: '内置 Cargo check/clippy 严格诊断工具、unsafe 块与 FFI 扫描器、Rust 所有权与异步并发安全审计 Skill 及微基准测试模板。',
        en: 'Cargo check/clippy diagnostic tool, unsafe & FFI block scanner, Rust ownership/async soundness audit skill, and benchmark prompt.'
      },
      previewTag: 'plugin__rust_expert__cargo_diagnostics',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install rust-expert</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:rust-audit  <span class="hl-out"># 扫描 unsafe 块并执行 clippy -D warnings</span>\n' +
        '<span class="hl-out">→ 0 warnings · 0 undocumented unsafe blocks</span>',
      capabilities: {
        tools: [
          'plugin__rust_expert__cargo_diagnostics',
          'plugin__rust_expert__cargo_unsafe_scan'
        ],
        skills: ['rust-audit (skills/rust-audit/SKILL.md)'],
        prompts: ['bench (prompts/bench.md)'],
        mcp: [],
        context: ['CONTEXT.md']
      },
      manifest: {
        schema_version: 1,
        name: 'rust-expert',
        version: '0.1.0',
        description: 'Cargo check/clippy/test diagnostic tool, Rust unsafe & ownership audit skill, and benchmark template',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['rust', 'cargo', 'clippy', 'audit'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'cargo_diagnostics',
            description: 'Run cargo check and clippy summary on the current Rust workspace',
            command: ['cargo', 'clippy', '--all-targets', '--message-format=short', '--', '-D', 'warnings']
          },
          {
            name: 'cargo_unsafe_scan',
            description: 'Scan Rust source files for unsafe blocks, impls, and extern FFI declarations',
            command: ['sh', '-c', "grep -rnE 'unsafe\\s+(fn|impl|trait|\\{)' src/ 2>/dev/null || echo 'No unsafe blocks found in src/'"]
          }
        ]
      }
    },
    {
      name: 'security-audit',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 3,
      publishedAt: '2026-10-14T02:50:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/security-audit',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['security', 'audit', 'owasp', 'secrets'],
      desc: {
        zh: '扫描仓库中意外提交的 API Key 与私钥、执行 OWASP Top 10 边界安全审查（命令注入/路径穿越/SSRF），并生成 STRIDE 威胁模型。',
        en: 'Workspace secret & private-key scanner, OWASP Top 10 security review skill, and STRIDE threat modeling prompt template.'
      },
      previewTag: 'plugin__security_audit__secret_scan',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install security-audit</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:security-review\n' +
        '<span class="hl-out">→ No hardcoded secret patterns matched in tracked files.</span>',
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
        description: 'Workspace secret & supply-chain scanner tool, OWASP code review skill, and threat model prompt',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['security', 'audit', 'owasp', 'secrets'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'secret_scan',
            description: 'Scan tracked repository files for potential hardcoded API keys, private keys, or credentials',
            command: ['sh', '-c', "git grep -nEI '(AKIA[0-9A-Z]{16}|BEGIN (RSA|OPENSSH|EC|PGP) PRIVATE KEY|sk-[A-Za-z0-9]{20,}|ghp_[A-Za-z0-9]{30,})' 2>/dev/null || echo 'No hardcoded secret patterns matched in tracked files.'"]
          }
        ]
      }
    },
    {
      name: 'mcp-github',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 4,
      publishedAt: '2026-10-14T03:04:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/mcp-github',
      types: ['mcp', 'tool', 'skill', 'prompt', 'context'],
      keywords: ['github', 'mcp', 'pr', 'issues', 'ci'],
      desc: {
        zh: '一键接入 GitHub 官方 MCP 服务 (@modelcontextprotocol/server-github) 与 gh PR 状态检查工具，提供 Issue/CI 故障分诊 Skill。',
        en: 'GitHub MCP server integration (@modelcontextprotocol/server-github), gh PR status tool, issue/CI triage skill, and PR prompt.'
      },
      previewTag: 'mcp__mcp_github_github__* + gh_pr_status',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install mcp-github</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:github-triage  <span class="hl-out"># 自动挂载 mcp.json + gh_pr_status</span>\n' +
        '<span class="hl-out">→ Connected MCP server: github (stdio JSON-RPC 2.0)</span>',
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
        description: 'GitHub CLI & MCP bridge for issues, pull requests, workflow runs, and repository search',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['github', 'mcp', 'pr', 'issues', 'ci'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        mcp: 'mcp.json',
        tools: [
          {
            name: 'gh_pr_status',
            description: 'Check GitHub Pull Request status and CI checks for the current branch using gh CLI',
            command: ['sh', '-c', "gh pr status 2>&1 || echo 'gh CLI not authenticated or not a GitHub repo'"]
          }
        ]
      }
    },
    {
      name: 'test-runner',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 5,
      publishedAt: '2026-10-14T03:05:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/test-runner',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['testing', 'tdd', 'cargo-test', 'pytest', 'coverage'],
      desc: {
        zh: '自动识别工作区测试框架（Cargo、Pytest、NPM、Go）并运行测试套件，附带严格 Red-Green-Refactor TDD 技能与边界回归测试生成模板。',
        en: 'Auto-detects and runs Cargo, Pytest, NPM, or Go test suites, with a Red-Green-Refactor TDD skill and edge-case test prompt.'
      },
      previewTag: 'plugin__test_runner__detect_and_test',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install test-runner</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:tdd-workflow\n' +
        '<span class="hl-out">→ Running cargo test --locked ... 72 passed; 0 failed</span>',
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
        description: 'Polyglot test suite detector & runner (Cargo, Pytest, Vitest/Jest, Go), TDD skill, and regression test prompt',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['testing', 'tdd', 'cargo-test', 'pytest', 'coverage'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'detect_and_test',
            description: 'Auto-detect project test framework and run unit tests',
            command: ['sh', '-c', 'if [ -f Cargo.toml ]; then cargo test --locked; elif [ -f pyproject.toml ]; then pytest -q; elif [ -f package.json ]; then npm test --silent; elif [ -f go.mod ]; then go test ./...; fi']
          }
        ]
      }
    },
    {
      name: 'perf-profiler',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 6,
      publishedAt: '2026-10-14T03:06:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/perf-profiler',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['performance', 'benchmark', 'profiling', 'memory', 'latency'],
      desc: {
        zh: '工作区体积与产物分布分析工具、冷启动延迟与峰值常驻内存（RSS）审计 Skill，以及热点分配消除与零拷贝重构提示词模板。',
        en: 'Workspace footprint analyzer, cold-start latency & peak RSS memory audit skill, and hot-path zero-copy optimization prompt.'
      },
      previewTag: 'plugin__perf_profiler__workspace_footprint',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install perf-profiler</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:perf-audit\n' +
        '<span class="hl-out">→ Startup median: 2.5 ms · Peak RSS: 5.6 MB</span>',
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
        description: 'Binary size, startup latency, RSS memory footprint analyzer, and hot-path optimization skill',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['performance', 'benchmark', 'profiling', 'memory', 'latency'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'workspace_footprint',
            description: 'Report largest source files, build artifacts, and line count distribution in the workspace',
            command: ['sh', '-c', 'du -sh . target node_modules 2>/dev/null || du -sh .']
          }
        ]
      }
    },
    {
      name: 'docker-ops',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 7,
      publishedAt: '2026-10-14T03:07:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/docker-ops',
      types: ['tool', 'skill', 'prompt', 'context'],
      keywords: ['docker', 'containers', 'devops', 'compose', 'ci'],
      desc: {
        zh: 'Docker / Podman 容器状态巡检工具、多阶段 Dockerfile 缓存优化与非 Root 安全加固 Skill，以及生产级容器化生成模板。',
        en: 'Docker/Podman container status tool, multi-stage Dockerfile caching & non-root hardening skill, and deployment prompt.'
      },
      previewTag: 'plugin__docker_ops__container_ps',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install docker-ops</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:dockerfile-hardening\n' +
        '<span class="hl-out">→ Multi-stage build · non-root USER · layer cache verified</span>',
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
        description: 'Container & Compose status inspection tool, multi-stage Dockerfile hardening skill, and deployment prompt',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['docker', 'containers', 'devops', 'compose', 'ci'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: [
          {
            name: 'container_ps',
            description: 'List running Docker or Podman containers and Compose services',
            command: ['sh', '-c', "docker ps --format 'table {{.Names}}\\t{{.Image}}\\t{{.Status}}\\t{{.Ports}}' 2>/dev/null || podman ps 2>/dev/null || echo 'No container runtime active'"]
          }
        ]
      }
    },
    {
      name: 'docs-writer',
      version: '0.1.0',
      author: 'Ravomi Community',
      license: 'MIT',
      featuredOrder: 8,
      publishedAt: '2026-10-14T02:51:00Z',
      publishedLabel: { zh: '官方精选 · 刚刚更新', en: 'Official · Just now' },
      repository: 'https://github.com/Ravomi/mio-plugins',
      path: 'plugins/docs-writer',
      types: ['skill', 'prompt', 'context'],
      keywords: ['docs', 'adr', 'architecture', 'markdown'],
      desc: {
        zh: '强制对照真实源码核验 CLI 参数与 API 签名的技术文档架构师 Skill，以及标准 ADR（架构决策记录）生成提示词模板。',
        en: 'Source-verified technical documentation architect skill and Architecture Decision Record (ADR) prompt template.'
      },
      previewTag: '/skill:docs-architect · /prompt:adr',
      previewCode:
        '<span class="hl-cmd">$ mio --plugin-install docs-writer</span>\n' +
        '<span class="hl-flag">mio&gt;</span> /skill:docs-architect\n' +
        '<span class="hl-out">→ Verified CLI flags against src/bin/mio.rs · ADR ready</span>',
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
        description: 'Technical documentation architect skill and Architecture Decision Record (ADR) prompt template',
        author: 'Ravomi Community',
        license: 'MIT',
        repository: 'https://github.com/Ravomi/mio-plugins.git',
        keywords: ['docs', 'adr', 'architecture', 'markdown'],
        skills: ['skills'],
        prompts: ['prompts'],
        context_files: ['CONTEXT.md'],
        tools: []
      }
    }
  ];

  var catalogPlugins = BUILTIN_PLUGINS.slice();

  function announce(msg) {
    var live = document.getElementById('copy-status-live');
    if (!live) return;
    live.textContent = '';
    setTimeout(function () {
      live.textContent = msg;
    }, 40);
  }

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
    try {
      document.execCommand('copy');
    } catch (e) {}
    document.body.removeChild(ta);
    if (onDone) onDone();
  }

  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  // =========================================================================
  // 1. Render Package Cards Grid
  // =========================================================================
  function getFilteredPlugins() {
    var searchEl = document.getElementById('packageSearchInput');
    var typeEl = document.getElementById('packageTypeSelect');
    var sortEl = document.getElementById('packageSortSelect');

    var q = ((searchEl && searchEl.value) || '').trim().toLowerCase();
    var selectedType = (typeEl && typeEl.value) || '';
    var sortBy = (sortEl && sortEl.value) || 'featured';

    var filtered = catalogPlugins.filter(function (item) {
      if (selectedType && item.types.indexOf(selectedType) === -1) {
        return false;
      }
      if (activeTopic) {
        var inKeywords = item.keywords.indexOf(activeTopic) !== -1;
        var inTypes = item.types.indexOf(activeTopic) !== -1;
        var inName = item.name.indexOf(activeTopic) !== -1;
        if (!inKeywords && !inTypes && !inName) return false;
      }
      if (!q) return true;
      var haystack = [
        item.name,
        item.author,
        item.desc.zh,
        item.desc.en,
        item.keywords.join(' '),
        item.types.join(' '),
        (item.capabilities.tools || []).join(' '),
        (item.capabilities.skills || []).join(' '),
        (item.capabilities.prompts || []).join(' ')
      ]
        .join(' ')
        .toLowerCase();
      return haystack.indexOf(q) !== -1;
    });

    filtered.sort(function (a, b) {
      if (sortBy === 'name') {
        return a.name.localeCompare(b.name);
      }
      if (sortBy === 'recent') {
        return String(b.publishedAt).localeCompare(String(a.publishedAt));
      }
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
    var total = catalogPlugins.length;

    if (countBadge) {
      countBadge.textContent = items.length ? '1-' + items.length + ' / ' + total : '0 / ' + total;
    }

    if (!items.length) {
      grid.innerHTML = '';
      if (emptyState) emptyState.hidden = false;
      return;
    }
    if (emptyState) emptyState.hidden = true;

    var repoSvg =
      '<svg xmlns="http://www.w3.org/2000/svg" fill="currentColor" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z"></path></svg>';
    var flagSvg =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"></path><line x1="4" y1="22" x2="4" y2="15"></line></svg>';
    var codeSvg =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="16 18 22 12 16 6"></polyline><polyline points="8 6 2 12 8 18"></polyline></svg>';

    grid.innerHTML = items
      .map(function (item) {
        var repoUrl = item.path
          ? 'https://github.com/Ravomi/mio-plugins/tree/main/' + item.path
          : item.repository;
        var manifestUrl = item.path
          ? 'https://github.com/Ravomi/mio-plugins/blob/main/' + item.path + '/mio-plugin.json'
          : item.repository;
        var reportUrl =
          'https://github.com/Ravomi/mio-plugins/issues/new?title=' +
          encodeURIComponent('[Plugin Report] ' + item.name + ' v' + item.version);

        var installCmd = 'mio --plugin-install ' + item.name;
        var descText = currentLang === 'en' ? item.desc.en : item.desc.zh;
        var pubText = currentLang === 'en' ? item.publishedLabel.en : item.publishedLabel.zh;
        var copyLabel = currentLang === 'en' ? 'Copy' : '复制';
        var inspectLabel = currentLang === 'en' ? 'inspect' : '查看清单';

        var typeBadges = item.types
          .map(function (t) {
            return (
              '<span class="packages-badge" data-type="' +
              escapeHtml(t) +
              '">' +
              escapeHtml(t) +
              '</span>'
            );
          })
          .join('');

        var keywordBadges = item.keywords
          .filter(function (k) {
            return item.types.indexOf(k) === -1;
          })
          .slice(0, 4)
          .map(function (k) {
            return '<span class="packages-badge">#' + escapeHtml(k) + '</span>';
          })
          .join('');

        return (
          '<article class="package-card" id="plugin-' +
          escapeHtml(item.name) +
          '" data-package-name="' +
          escapeHtml(item.name) +
          '">' +
          '<div class="packages-card-preview">' +
          '<div class="packages-card-preview-frame">' +
          '<div class="packages-preview-dots"><span></span><span></span><span></span></div>' +
          '<span class="packages-preview-tag">' +
          escapeHtml(item.previewTag) +
          '</span>' +
          '</div>' +
          '<pre class="packages-preview-code">' +
          item.previewCode +
          '</pre>' +
          '</div>' +
          '<div class="packages-card-body">' +
          '<div class="packages-card-title-row">' +
          '<h3 class="packages-name">' +
          '<button type="button" data-open-plugin="' +
          escapeHtml(item.name) +
          '">' +
          escapeHtml(item.name) +
          '</button>' +
          '</h3>' +
          '<span class="packages-version-chip">v' +
          escapeHtml(item.version) +
          '</span>' +
          '</div>' +
          '<p class="packages-desc">' +
          escapeHtml(descText) +
          '</p>' +
          '<div class="packages-meta">' +
          '<span>👤 ' +
          escapeHtml(item.author) +
          '</span>' +
          '<span>⚖ ' +
          escapeHtml(item.license) +
          '</span>' +
          '<span>' +
          escapeHtml(pubText) +
          '</span>' +
          '</div>' +
          '<div class="packages-badges">' +
          typeBadges +
          keywordBadges +
          '</div>' +
          '<div class="packages-links">' +
          '<button type="button" data-open-plugin="' +
          escapeHtml(item.name) +
          '">' +
          codeSvg +
          ' <span>' +
          inspectLabel +
          '</span></button>' +
          '<a href="' +
          escapeHtml(repoUrl) +
          '" target="_blank" rel="noopener noreferrer">' +
          repoSvg +
          ' <span>repo</span></a>' +
          '<a href="' +
          escapeHtml(manifestUrl) +
          '" target="_blank" rel="noopener noreferrer"><span>mio-plugin.json</span></a>' +
          '<a href="' +
          escapeHtml(reportUrl) +
          '" target="_blank" rel="noopener noreferrer">' +
          flagSvg +
          ' <span>report</span></a>' +
          '</div>' +
          '<div class="packages-install">' +
          '<code><span class="prefix">$</span>' +
          escapeHtml(installCmd) +
          '</code>' +
          '<button type="button" class="packages-copy-btn" data-copy-cmd="' +
          escapeHtml(installCmd) +
          '">' +
          copyLabel +
          '</button>' +
          '</div>' +
          '</div>' +
          '</article>'
        );
      })
      .join('');
  }

  // =========================================================================
  // 2. Interactive Plugin Inspector Modal (<dialog>)
  // =========================================================================
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

    var repoUrl = plugin.path
      ? 'https://github.com/Ravomi/mio-plugins/tree/main/' + plugin.path
      : plugin.repository;
    var cliInstall = 'mio --plugin-install ' + plugin.name;
    var replInstall = '/plugin install ' + plugin.name;
    var ghInstall = plugin.path
      ? 'mio --plugin-install github:Ravomi/mio-plugins#' + plugin.path
      : 'mio --plugin-install ' + plugin.repository;

    var desc = currentLang === 'en' ? plugin.desc.en : plugin.desc.zh;
    var caps = plugin.capabilities || {};

    function renderCapList(label, arr) {
      if (!arr || !arr.length) return '';
      return (
        '<div style="margin-bottom:8px;">' +
        '<strong style="font-size:12.5px;color:var(--muted);display:block;margin-bottom:4px;">' +
        escapeHtml(label) +
        '</strong>' +
        '<div class="packages-badges">' +
        arr
          .map(function (c) {
            return '<span class="packages-badge" style="color:var(--ink);">' + escapeHtml(c) + '</span>';
          })
          .join('') +
        '</div>' +
        '</div>'
      );
    }

    var manifestJson = JSON.stringify(plugin.manifest, null, 2);

    bodyEl.innerHTML =
      '<p style="font-size:15px;color:var(--ink-secondary);">' +
      escapeHtml(desc) +
      '</p>' +
      '<div style="display:grid;gap:10px;">' +
      '<div class="packages-install">' +
      '<code><span class="prefix">$</span>' +
      escapeHtml(cliInstall) +
      '</code>' +
      '<button type="button" class="packages-copy-btn" data-copy-cmd="' +
      escapeHtml(cliInstall) +
      '">CLI</button>' +
      '</div>' +
      '<div class="packages-install">' +
      '<code><span class="prefix">mio&gt;</span>' +
      escapeHtml(replInstall) +
      '</code>' +
      '<button type="button" class="packages-copy-btn" data-copy-cmd="' +
      escapeHtml(replInstall) +
      '">REPL / Alacritty-Mio</button>' +
      '</div>' +
      '<div class="packages-install">' +
      '<code><span class="prefix">$</span>' +
      escapeHtml(ghInstall) +
      '</code>' +
      '<button type="button" class="packages-copy-btn" data-copy-cmd="' +
      escapeHtml(ghInstall) +
      '">GitHub Spec</button>' +
      '</div>' +
      '</div>' +
      '<div>' +
      renderCapList('Declarative Tools (Tools)', caps.tools) +
      renderCapList('Agent Skills (SKILL.md)', caps.skills) +
      renderCapList('Prompt Templates (Prompts)', caps.prompts) +
      renderCapList('MCP Servers (mcp.json)', caps.mcp) +
      renderCapList('Context Files', caps.context) +
      '</div>' +
      '<div>' +
      '<div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;">' +
      '<strong style="font-family:var(--font-mono);font-size:13px;">mio-plugin.json</strong>' +
      '<a href="' +
      escapeHtml(repoUrl) +
      '" target="_blank" rel="noopener noreferrer" style="font-size:13px;color:var(--purple);font-weight:700;">在 GitHub 查看完整源码 ↗</a>' +
      '</div>' +
      '<pre class="plugin-modal-pre">' +
      escapeHtml(manifestJson) +
      '</pre>' +
      '</div>';

    if (typeof dialog.showModal === 'function') {
      dialog.showModal();
    } else {
      dialog.setAttribute('open', '');
    }
  }

  function setupModalAndDelegatedClicks() {
    var dialog = document.getElementById('pluginDetailDialog');
    var closeBtn = document.getElementById('closePluginModalBtn');

    if (closeBtn && dialog) {
      closeBtn.addEventListener('click', function () {
        if (typeof dialog.close === 'function') dialog.close();
        else dialog.removeAttribute('open');
      });
      dialog.addEventListener('click', function (e) {
        if (e.target === dialog) {
          if (typeof dialog.close === 'function') dialog.close();
          else dialog.removeAttribute('open');
        }
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
          copyBtn.textContent = currentLang === 'en' ? 'Copied ✓' : '已复制 ✓';
          announce('Copied ' + cmd);
          setTimeout(function () {
            copyBtn.textContent = orig;
          }, 1600);
        });
        return;
      }

      var targetCopyBtn = e.target.closest('[data-copy-target]');
      if (targetCopyBtn) {
        var targetId = targetCopyBtn.getAttribute('data-copy-target');
        var el = document.getElementById(targetId);
        if (!el) return;
        var raw = (el.innerText || el.textContent || '').trim();
        copyText(raw, function () {
          var orig = targetCopyBtn.textContent;
          targetCopyBtn.textContent = currentLang === 'en' ? 'Copied ✓' : '已复制 ✓';
          setTimeout(function () {
            targetCopyBtn.textContent = orig;
          }, 1600);
        });
      }
    });
  }

  // =========================================================================
  // 3. Search, Filter, Sort & Live GitHub Registry Sync
  // =========================================================================
  function setupFilters() {
    var form = document.getElementById('packagesFilterForm');
    var searchEl = document.getElementById('packageSearchInput');
    var typeEl = document.getElementById('packageTypeSelect');
    var sortEl = document.getElementById('packageSortSelect');
    var resetBtn = document.getElementById('packageResetBtn');
    var pills = Array.prototype.slice.call(document.querySelectorAll('[data-topic]'));

    // Read initial query params
    try {
      var params = new URLSearchParams(window.location.search);
      if (searchEl && params.get('q')) searchEl.value = params.get('q');
      if (typeEl && params.get('type')) typeEl.value = params.get('type');
      if (sortEl && params.get('sort')) sortEl.value = params.get('sort');
    } catch (e) {}

    if (form) {
      form.addEventListener('submit', function (e) {
        e.preventDefault();
        renderPackagesGrid();
      });
    }
    if (searchEl) {
      searchEl.addEventListener('input', renderPackagesGrid);
    }
    if (typeEl) {
      typeEl.addEventListener('change', renderPackagesGrid);
    }
    if (sortEl) {
      sortEl.addEventListener('change', renderPackagesGrid);
    }
    if (resetBtn) {
      resetBtn.addEventListener('click', function () {
        if (searchEl) searchEl.value = '';
        if (typeEl) typeEl.value = '';
        if (sortEl) sortEl.value = 'featured';
        activeTopic = '';
        pills.forEach(function (p) {
          p.classList.toggle('is-active', p.getAttribute('data-topic') === '');
        });
        renderPackagesGrid();
      });
    }

    pills.forEach(function (pill) {
      pill.addEventListener('click', function () {
        activeTopic = pill.getAttribute('data-topic') || '';
        pills.forEach(function (p) {
          p.classList.toggle('is-active', p === pill);
        });
        renderPackagesGrid();
      });
    });
  }

  function syncLiveGitHubRegistry() {
    var syncText = document.getElementById('liveSyncText');
    fetch('https://raw.githubusercontent.com/Ravomi/mio-plugins/main/registry.json')
      .then(function (res) {
        if (!res.ok) throw new Error('HTTP ' + res.status);
        return res.json();
      })
      .then(function (data) {
        if (!data || !Array.isArray(data.plugins)) return;
        var existingNames = {};
        catalogPlugins.forEach(function (p) {
          existingNames[p.name] = true;
        });

        data.plugins.forEach(function (remote, idx) {
          if (!remote || !remote.name || existingNames[remote.name]) return;
          catalogPlugins.push({
            name: remote.name,
            version: remote.version || '0.1.0',
            author: remote.author || 'Community',
            license: remote.license || 'MIT',
            featuredOrder: 20 + idx,
            publishedAt: new Date().toISOString(),
            publishedLabel: { zh: '社区新增', en: 'Community' },
            repository: remote.repository || 'https://github.com/Ravomi/mio-plugins',
            path: remote.path || '',
            types: ['tool', 'skill'],
            keywords: Array.isArray(remote.keywords) ? remote.keywords : [],
            desc: {
              zh: remote.description || '',
              en: remote.description || ''
            },
            previewTag: 'mio --plugin-install ' + remote.name,
            previewCode:
              '<span class="hl-cmd">$ mio --plugin-install ' +
              escapeHtml(remote.name) +
              '</span>\n<span class="hl-out">→ Installed from Ravomi/mio-plugins registry</span>',
            capabilities: {
              tools: [],
              skills: [],
              prompts: [],
              mcp: [],
              context: []
            },
            manifest: remote
          });
        });

        if (syncText) {
          var count = catalogPlugins.length;
          syncText.dataset.zh = '已实时同步 Ravomi/mio-plugins (' + count + ' 个插件)';
          syncText.dataset.en = 'Live synced with Ravomi/mio-plugins (' + count + ' plugins)';
          syncText.textContent = currentLang === 'en' ? syncText.dataset.en : syncText.dataset.zh;
        }
        renderPackagesGrid();
      })
      .catch(function () {});
  }

  // =========================================================================
  // 4. Theme & Language Toggles
  // =========================================================================
  function setupThemeAndLang() {
    var themeBtn = document.getElementById('themeToggleBtn');
    var themeIcon = document.getElementById('themeIcon');
    var themeLabel = document.getElementById('themeLabel');
    var langBtn = document.getElementById('langToggleBtn');
    var searchEl = document.getElementById('packageSearchInput');

    if (themeBtn) {
      themeBtn.addEventListener('click', function () {
        var cur = document.documentElement.dataset.themeMode || 'light';
        var next = cur === 'light' ? 'dark' : cur === 'dark' ? 'auto' : 'light';
        var prefersDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
        var resolved = next === 'auto' ? (prefersDark ? 'dark' : 'light') : next;
        document.documentElement.dataset.theme = resolved;
        document.documentElement.dataset.themeMode = next;
        try {
          localStorage.setItem('mio:theme', next);
        } catch (e) {}
        if (themeIcon) {
          themeIcon.textContent = next === 'dark' ? '☾' : next === 'auto' ? '◐' : '☀';
        }
        if (themeLabel) {
          var map = {
            light: { zh: '浅色', en: 'Light' },
            dark: { zh: '深色', en: 'Dark' },
            auto: { zh: '自动', en: 'Auto' }
          };
          var item = map[next] || map.light;
          themeLabel.dataset.zh = item.zh;
          themeLabel.dataset.en = item.en;
          themeLabel.textContent = currentLang === 'en' ? item.en : item.zh;
        }
      });
    }

    if (langBtn) {
      langBtn.addEventListener('click', function () {
        currentLang = currentLang === 'zh' ? 'en' : 'zh';
        document.documentElement.lang = currentLang === 'en' ? 'en' : 'zh-CN';
        langBtn.textContent = currentLang === 'en' ? '中文' : 'EN';

        document.querySelectorAll('[data-zh][data-en]').forEach(function (el) {
          el.textContent = currentLang === 'en' ? el.dataset.en : el.dataset.zh;
        });
        document.querySelectorAll('[data-zh-html][data-en-html]').forEach(function (el) {
          el.innerHTML = currentLang === 'en' ? el.dataset.enHtml : el.dataset.zhHtml;
        });
        if (searchEl) {
          searchEl.placeholder =
            currentLang === 'en'
              ? searchEl.getAttribute('data-placeholder-en')
              : searchEl.getAttribute('data-placeholder-zh');
        }
        renderPackagesGrid();
      });
    }
  }

  document.addEventListener('DOMContentLoaded', function () {
    setupThemeAndLang();
    setupFilters();
    setupModalAndDelegatedClicks();
    renderPackagesGrid();
    syncLiveGitHubRegistry();
  });
})();
