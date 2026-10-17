//! CLI 参数定义、校验与帮助文本。
//!
//! 只依赖 `mio_coding_agent` 的公共类型，不引用终端、Runner 或 RPC 状态。

use mio_coding_agent::ThinkingLevel;
use std::{io::IsTerminal, path::PathBuf, str::FromStr};

pub(super) const HELP: &str = "Mio — Rust coding agent

Usage: mio [OPTIONS] [-p PROMPT] [@FILE...]
Without a prompt, starts an interactive conversation in a terminal or reads stdin.

  -p, --print TEXT       Run one task and exit
  --json                Emit JSON Lines events on stdout (alias for --mode json)
  --mode MODE           Output/protocol mode: text, json, or rpc
  --cwd PATH            Working directory (default: current directory)
  --model NAME          Model name or [provider/]model[:thinking] (or MIO_MODEL)
  --provider NAME       Provider preset: custom, custom-responses, deepseek, antigravity, openai, anthropic,
                        google, openrouter, groq, xai, mistral, ollama (or MIO_PROVIDER)
  --thinking LEVEL      Thinking level: off, minimal, low, medium, high, xhigh, max
  --max-model-concurrency N  Concurrent model requests in this process (default: 2)
  --max-provider-concurrency N  Concurrent requests per provider (default: 2)
  --max-heavy-tasks N    Concurrent shell/external tool calls (default: 2)
  --tool-concurrency N  Concurrent tool handlers per run (1–32; default: 1)
  --max-subagents N      Concurrent admitted child tasks (default: 4)
  --max-model-calls N    Shared model-call budget for this process
  --max-total-tokens N   Shared conservative token budget for this process
  --shared-limits-dir PATH  Shared cross-process quota directory
  --no-shared-limits    Use only in-process admission limits
  --list-models [QUERY] List available models across provider presets and exit
  --import-antigravity PATH  Import existing local auth.json/settings.json into Mio
  --base-url URL        OpenAI-compatible base URL (or MIO_BASE_URL)
  -t, --tools LIST      Comma-separated tools or +tool/-tool deltas (read,write,edit,bash,grep,find,ls)
  -xt, --exclude-tools LIST  Exclude tools or glob patterns (e.g. bash,mcp__*)
  -nt, --no-tools       Disable all tools
  --no-mcp              Disable MCP server discovery (.mio/mcp.json)
  --trust-project-mcp   Authorize starting project MCP servers without per-start approval
  --recovery            Print persisted execution evidence for the selected session and exit
  --list-unresolved-effects      List exact durable unknown-side-effect records and snapshot digests (read-only)
  --ack-unresolved-effect JSON   Acknowledge exactly one durable record by exact key+snapshot (maintenance)
  --approve             Require interactive confirmation before write/edit/bash/MCP tools
  --session SELECTOR    Load/save a session by path, short UUID prefix, or session name
  --checkpoints         Save pre-edit snapshots for write/edit
  --list-checkpoints    List this workspace file checkpoints and exit
  --rewind ID           Revert one file checkpoint if the file still matches
  --output-cleanup-plan PATH     Offline dry run: write a fresh output cleanup plan (never deletes)
  --output-cleanup-confirm PATH  Apply one plan written by --output-cleanup-plan (explicit only)
  --dedup-key KEY       One-shot task key: repeat submissions reuse the first task id and never re-run
  -c, --continue, -r, --resume  Continue the most recently saved session in this workspace
  --fork [SELECTOR]     Fork the selected --session or --resume into a new session
  -n, --name TITLE      Set a human-readable display name for the session
  --no-session          Run ephemerally in memory without saving a session file
  --export PATH         Export the selected or latest session to a standalone HTML file
  --list-sessions       List local sessions without contacting a model
  --skill NAME          Include a project skill (repeatable)
  --template NAME       Use .mio/prompts/NAME.md; prompt replaces $ARGUMENTS
  --system-prompt TEXT  Custom system prompt string or file path
  --append-system-prompt TEXT  Append text or file path to system prompt
  --no-context          Do not load project instructions or skills
  --no-context-files    Do not load AGENTS.md / CLAUDE.md / SYSTEM.md
  --no-skills           Do not scan or advertise project skills
  --no-prompt-templates Disable prompt template expansion
  --no-shell            Disable shell execution (file tools remain available)
  --sandbox BACKEND     Subprocess backend: unrestricted (default) or bubblewrap (Linux)
  --sandbox-read-only PATH  Add an absolute read-only mount (repeatable; never /)
  --sandbox-status      Show the selected subprocess backend and exit without a model
  --sandbox-cpu-seconds N  Per-process CPU ceiling inside bubblewrap
  --sandbox-memory-mib N  Per-process address-space ceiling inside bubblewrap
  --sandbox-open-files N  Per-process file-descriptor ceiling inside bubblewrap
  --subagents           Enable native child sessions (4 concurrent; memory only)
  --no-auto-compact     Disable automatic session compaction (on by default: 3/4 of
                        the input budget, at most 512 KiB)
  --context-window N    Override deployment context capacity (tokens; >= 1024)
  --max-output-tokens N Output token cap and reserve within the context window
  --temperature N       Fixed sampling temperature sent with every request, 0–2 (default: provider default)
  --max-turns N         Tool-loop budget, 1–256 (default: 32)
  --output-schema PATH  Require the final answer to match this JSON Schema file (off by default)
  --plugin-install SPEC Install plugin from community registry, github:owner/repo, or path
  --plugin-remove NAME  Remove an installed plugin from .mio/plugins/NAME
  --plugin-list         List installed plugins in this workspace
  --plugin-search [Q]   Search official community plugins (Ravomi/mio-plugins)
  --plugin-init DIR     Scaffold a new standard-compliant Mio plugin (mio-plugin.json)
  --plugin-validate P   Validate a plugin directory or registry.json file
  -h, --help            Show help
  -V, --version         Show version

Credentials: MIO_API_KEY (or provider-specific OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY,
DEEPSEEK_API_KEY, OPENROUTER_API_KEY, GROQ_API_KEY, XAI_API_KEY, MISTRAL_API_KEY).
User defaults: ~/.config/mio/settings.json (defaultProvider, defaultModel, baseUrl, apiKey).
Sessions: .mio/sessions/*.json, saved after successful turns (unless --no-session).
File checkpoints (off by default): --checkpoints, MIO_CHECKPOINTS=1 or /checkpoints on snapshots write/edit
before each change; /checkpoints lists them and /rewind ID reverts one file. Rewind refuses to
overwrite later manual edits, never touches Git history and never rolls back shell effects.
Interactive commands: /help, /new, /fork, /sessions, /name, /compact, /export, /model,
/thinking, /plugins, /plugin, /subagents, /checkpoints [on|off], /rewind ID, /status,
/clear, /exit.
Tab completes commands, models, plugins, and @file paths; Up/Down browses history;
Ctrl-C exits, and Ctrl-D exits on an empty prompt.
Shell is noninteractive. Default backend is unrestricted; --sandbox bubblewrap opts into Linux isolation.
";

pub(super) fn print_help() {
    if !std::io::stdout().is_terminal()
        || std::env::var_os("NO_COLOR").is_some()
        || std::env::var("TERM").as_deref() == Ok("dumb")
    {
        print!("{HELP}");
        return;
    }

    for line in HELP.lines() {
        if line == "Mio — Rust coding agent" {
            println!("\x1b[1;36m{line}\x1b[0m");
        } else if let Some(rest) = line.strip_prefix("Usage: ") {
            println!("\x1b[1;33mUsage:\x1b[0m \x1b[1;32m{rest}\x1b[0m");
        } else if let Some(option) = line.strip_prefix("  -") {
            let option = format!("-{option}");
            let end = option.find("  ").unwrap_or(option.len());
            println!("  \x1b[1;36m{}\x1b[0m{}", &option[..end], &option[end..]);
        } else if let Some((label, rest)) = line.split_once(':') {
            if matches!(
                label,
                "Credentials"
                    | "User defaults"
                    | "Sessions"
                    | "File checkpoints"
                    | "Interactive commands"
            ) {
                println!("\x1b[1;33m{label}:\x1b[0m{rest}");
            } else {
                println!("{line}");
            }
        } else {
            println!("{line}");
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum OutputMode {
    #[default]
    Text,
    Json,
    Rpc,
}

#[derive(Default)]
pub(super) struct Args {
    pub(super) cwd: Option<PathBuf>,
    pub(super) model: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) thinking: Option<ThinkingLevel>,
    pub(super) list_models: bool,
    pub(super) list_models_query: Option<String>,
    pub(super) import_antigravity: Option<PathBuf>,
    pub(super) base_url: Option<String>,
    pub(super) prompt: Option<String>,
    pub(super) mode: OutputMode,
    pub(super) json: bool,
    pub(super) tools: Option<String>,
    pub(super) exclude_tools: Option<String>,
    pub(super) no_tools: bool,
    pub(super) no_mcp: bool,
    pub(super) trust_project_mcp: bool,
    pub(super) recovery: bool,
    /// 精确列出工作区持久未决副作用账本（只读维护命令）。
    pub(super) list_unresolved_effects: bool,
    /// 按精确 `(scope, call_index, snapshot)` 确认一条未决副作用账本记录（维护命令）。
    pub(super) ack_unresolved_effect: Option<String>,
    pub(super) approve: bool,
    pub(super) session: Option<String>,
    pub(super) session_name: Option<String>,
    pub(super) no_session: bool,
    pub(super) checkpoints: bool,
    pub(super) list_checkpoints: bool,
    pub(super) rewind: Option<String>,
    pub(super) output_cleanup_plan: Option<PathBuf>,
    pub(super) output_cleanup_confirm: Option<PathBuf>,
    pub(super) dedup_key: Option<String>,
    pub(super) export: Option<PathBuf>,
    pub(super) resume: bool,
    pub(super) fork: bool,
    pub(super) list: bool,
    pub(super) skills: Vec<String>,
    pub(super) template: Option<String>,
    pub(super) system_prompt: Option<String>,
    pub(super) append_system_prompt: Option<String>,
    pub(super) no_context: bool,
    pub(super) no_context_files: bool,
    pub(super) no_skills: bool,
    pub(super) no_prompt_templates: bool,
    pub(super) no_shell: bool,
    pub(super) sandbox: Option<String>,
    pub(super) sandbox_status: bool,
    pub(super) sandbox_read_only: Vec<PathBuf>,
    pub(super) sandbox_cpu_seconds: Option<u64>,
    pub(super) sandbox_memory_mib: Option<u64>,
    pub(super) sandbox_open_files: Option<u64>,
    pub(super) subagents: bool,
    pub(super) no_auto_compact: bool,
    pub(super) context_window: Option<u32>,
    pub(super) max_output_tokens: Option<u32>,
    pub(super) temperature: Option<f64>,
    pub(super) max_turns: Option<usize>,
    /// `--output-schema PATH`：最终答复 JSON Schema 的文件路径。默认关闭；
    /// 相对路径按 `--cwd`（或当前工作区）解析，与 `--export` / `--shared-limits-dir`
    /// 一致。文件读取、解析与 32 KiB 序列化上限都在任何模型请求之前完成。
    pub(super) output_schema: Option<PathBuf>,
    pub(super) tool_concurrency: Option<usize>,
    pub(super) max_model_concurrency: Option<usize>,
    pub(super) max_provider_concurrency: Option<usize>,
    pub(super) max_heavy_tasks: Option<usize>,
    pub(super) max_subagents: Option<usize>,
    pub(super) max_model_calls: Option<u64>,
    pub(super) max_total_tokens: Option<u64>,
    pub(super) shared_limits_dir: Option<PathBuf>,
    pub(super) no_shared_limits: bool,
    pub(super) plugin_install: Option<String>,
    pub(super) plugin_remove: Option<String>,
    pub(super) plugin_list: bool,
    pub(super) plugin_search: bool,
    pub(super) plugin_search_query: Option<String>,
    pub(super) plugin_init: Option<PathBuf>,
    pub(super) plugin_validate: Option<PathBuf>,
}

pub(super) fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.peekable();
    let mut extra_parts: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--cwd" => parsed.cwd = Some(value()?.into()),
            "--model" => parsed.model = Some(value()?),
            "--provider" => parsed.provider = Some(value()?),
            "--thinking" => parsed.thinking = Some(ThinkingLevel::from_str(&value()?)?),
            "--context-window" => {
                parsed.context_window =
                    Some(value()?.parse().map_err(|_| "Invalid --context-window")?)
            }
            "--max-output-tokens" => {
                parsed.max_output_tokens = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid --max-output-tokens")?,
                )
            }
            "--temperature" => {
                let temperature: f64 = value()?.parse().map_err(|_| "Invalid --temperature")?;
                if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
                    return Err("--temperature must be a finite number between 0 and 2".into());
                }
                parsed.temperature = Some(temperature);
            }
            "--list-models" => {
                parsed.list_models = true;
                if let Some(next) = args.peek()
                    && !next.starts_with('-')
                {
                    parsed.list_models_query = args.next();
                }
            }
            "--import-antigravity" => parsed.import_antigravity = Some(value()?.into()),
            "--base-url" => parsed.base_url = Some(value()?),
            "-p" | "--print" => parsed.prompt = Some(value()?),
            "--mode" => {
                let m = value()?;
                parsed.mode = match m.as_str() {
                    "text" => OutputMode::Text,
                    "json" => {
                        parsed.json = true;
                        OutputMode::Json
                    }
                    "rpc" => {
                        parsed.json = true;
                        OutputMode::Rpc
                    }
                    _ => return Err(format!("Invalid --mode '{m}'; expected text, json, or rpc")),
                };
            }
            "-t" | "--tools" => parsed.tools = Some(value()?),
            "-xt" | "--exclude-tools" => parsed.exclude_tools = Some(value()?),
            "-nt" | "--no-tools" => parsed.no_tools = true,
            "--no-mcp" => parsed.no_mcp = true,
            "--trust-project-mcp" => parsed.trust_project_mcp = true,
            "--recovery" => parsed.recovery = true,
            "--list-unresolved-effects" => parsed.list_unresolved_effects = true,
            "--ack-unresolved-effect" => parsed.ack_unresolved_effect = Some(value()?),
            "--approve" => parsed.approve = true,
            "--session" => parsed.session = Some(value()?),
            "-n" | "--name" => parsed.session_name = Some(value()?),
            "--no-session" => parsed.no_session = true,
            "--checkpoints" => parsed.checkpoints = true,
            "--list-checkpoints" => parsed.list_checkpoints = true,
            "--rewind" => parsed.rewind = Some(value()?),
            "--output-cleanup-plan" => parsed.output_cleanup_plan = Some(value()?.into()),
            "--output-cleanup-confirm" => parsed.output_cleanup_confirm = Some(value()?.into()),
            "--dedup-key" => parsed.dedup_key = Some(value()?),
            "--export" => parsed.export = Some(value()?.into()),
            "--skill" => parsed.skills.push(value()?),
            "--template" => parsed.template = Some(value()?),
            "--system-prompt" => parsed.system_prompt = Some(value()?),
            "--append-system-prompt" => parsed.append_system_prompt = Some(value()?),
            "--tool-concurrency" => {
                parsed.tool_concurrency =
                    Some(value()?.parse().map_err(|_| "Invalid --tool-concurrency")?);
            }
            "--max-turns" => {
                parsed.max_turns = Some(value()?.parse().map_err(|_| "Invalid --max-turns")?)
            }
            "--output-schema" => parsed.output_schema = Some(value()?.into()),
            "--max-model-concurrency" => {
                parsed.max_model_concurrency = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid --max-model-concurrency")?,
                )
            }
            "--max-provider-concurrency" => {
                parsed.max_provider_concurrency = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid --max-provider-concurrency")?,
                )
            }
            "--max-heavy-tasks" => {
                parsed.max_heavy_tasks =
                    Some(value()?.parse().map_err(|_| "Invalid --max-heavy-tasks")?)
            }
            "--max-subagents" => {
                parsed.max_subagents =
                    Some(value()?.parse().map_err(|_| "Invalid --max-subagents")?)
            }
            "--max-model-calls" => {
                parsed.max_model_calls =
                    Some(value()?.parse().map_err(|_| "Invalid --max-model-calls")?)
            }
            "--max-total-tokens" => {
                parsed.max_total_tokens =
                    Some(value()?.parse().map_err(|_| "Invalid --max-total-tokens")?)
            }
            "--shared-limits-dir" => parsed.shared_limits_dir = Some(value()?.into()),
            "--no-shared-limits" => parsed.no_shared_limits = true,
            "--json" => {
                parsed.json = true;
                if parsed.mode == OutputMode::Text {
                    parsed.mode = OutputMode::Json;
                }
            }
            "-c" | "--continue" => parsed.resume = true,
            "-r" | "--resume" => {
                if let Some(next) = args.peek()
                    && !next.starts_with('-')
                    && parsed.session.is_none()
                {
                    parsed.session = args.next();
                } else {
                    parsed.resume = true;
                }
            }
            "--fork" => {
                parsed.fork = true;
                if let Some(next) = args.peek()
                    && !next.starts_with('-')
                    && parsed.session.is_none()
                    && !parsed.resume
                {
                    parsed.session = args.next();
                }
            }
            "--list-sessions" => parsed.list = true,
            "--no-context" => parsed.no_context = true,
            "--no-context-files" => parsed.no_context_files = true,
            "--no-skills" => parsed.no_skills = true,
            "--no-prompt-templates" => parsed.no_prompt_templates = true,
            "--no-shell" => parsed.no_shell = true,
            "--sandbox" => parsed.sandbox = Some(value()?),
            "--sandbox-status" => parsed.sandbox_status = true,
            "--sandbox-read-only" => {
                if parsed.sandbox_read_only.len() >= 32 {
                    return Err("At most 32 additional sandbox read-only mounts are allowed".into());
                }
                parsed.sandbox_read_only.push(value()?.into());
            }
            "--sandbox-cpu-seconds" => {
                parsed.sandbox_cpu_seconds = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid sandbox CPU ceiling")?,
                );
            }
            "--sandbox-memory-mib" => {
                parsed.sandbox_memory_mib = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid sandbox memory ceiling")?,
                );
            }
            "--sandbox-open-files" => {
                parsed.sandbox_open_files = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Invalid sandbox file-descriptor ceiling")?,
                );
            }
            "--subagents" => parsed.subagents = true,
            // Accepted for compatibility; automatic compaction is the default.
            "--auto-compact" => parsed.no_auto_compact = false,
            "--no-auto-compact" => parsed.no_auto_compact = true,
            "--plugin-install" => parsed.plugin_install = Some(value()?),
            "--plugin-remove" => parsed.plugin_remove = Some(value()?),
            "--plugin-list" | "--list-plugins" => parsed.plugin_list = true,
            "--plugin-search" => {
                parsed.plugin_search = true;
                if let Some(next) = args.peek()
                    && !next.starts_with('-')
                {
                    parsed.plugin_search_query = args.next();
                }
            }
            "--plugin-init" => parsed.plugin_init = Some(value()?.into()),
            "--plugin-validate" => parsed.plugin_validate = Some(value()?.into()),
            "--" => {
                if parsed.prompt.is_some() {
                    return Err("Prompt already provided".into());
                }
                parsed.prompt = Some(args.collect::<Vec<_>>().join(" "));
                break;
            }
            other if other.starts_with('@') => {
                extra_parts.push(other.to_owned());
            }
            _ => return Err(format!("Unknown option {arg}; use --help")),
        }
    }
    if !extra_parts.is_empty() {
        let joined = extra_parts.join(" ");
        parsed.prompt = Some(match parsed.prompt.take() {
            Some(existing) => format!("{joined} {existing}"),
            None => joined,
        });
    }
    if parsed.resume && parsed.session.is_some() {
        return Err("Use either --resume or --session".into());
    }
    if parsed.fork && !parsed.resume && parsed.session.is_none() {
        return Err("--fork requires --resume or --session".into());
    }
    if parsed.no_context && (!parsed.skills.is_empty() || parsed.template.is_some()) {
        return Err("--no-context cannot be combined with --skill or --template".into());
    }
    if parsed.no_skills && !parsed.skills.is_empty() {
        return Err("--no-skills cannot be combined with --skill".into());
    }
    if parsed.no_prompt_templates && parsed.template.is_some() {
        return Err("--no-prompt-templates cannot be combined with --template".into());
    }
    if !(1..=256).contains(&parsed.max_turns.unwrap_or(32)) {
        return Err("--max-turns must be 1–256".into());
    }
    if !(1..=32).contains(&parsed.tool_concurrency.unwrap_or(1)) {
        return Err("--tool-concurrency must be 1–32".into());
    }
    for (flag, value) in [
        ("--max-model-concurrency", parsed.max_model_concurrency),
        (
            "--max-provider-concurrency",
            parsed.max_provider_concurrency,
        ),
        ("--max-heavy-tasks", parsed.max_heavy_tasks),
    ] {
        if value.is_some_and(|value| !(1..=64).contains(&value)) {
            return Err(format!("{flag} must be 1–64"));
        }
    }
    if parsed
        .max_subagents
        .is_some_and(|value| !(1..=16).contains(&value))
    {
        return Err("--max-subagents must be 1–16".into());
    }
    if parsed.max_model_calls == Some(0) || parsed.max_total_tokens == Some(0) {
        return Err("Model-call and token budgets must be positive".into());
    }
    if parsed.no_shared_limits && parsed.shared_limits_dir.is_some() {
        return Err("--no-shared-limits conflicts with --shared-limits-dir".into());
    }
    if let Some(key) = parsed.dedup_key.as_deref() {
        mio_coding_agent::dedup::validate_key(key)?;
        if parsed.mode == OutputMode::Rpc {
            return Err(
                "--dedup-key is for one-shot tasks; use the RPC prompt/follow_up dedup_key field"
                    .into(),
            );
        }
        if parsed.no_session {
            return Err(
                "--dedup-key requires cross-run persistence and cannot be combined with --no-session"
                    .into(),
            );
        }
    }
    // 文件检查点默认关闭；MIO_CHECKPOINTS 与交互式 /checkpoints on 都可开启，
    // 两者都是开关而不是新 CLI 参数，因此站点 CLI 参数表无需变更。
    if std::env::var("MIO_CHECKPOINTS").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    }) {
        parsed.checkpoints = true;
    }
    if std::env::var("MIO_TRUST_PROJECT_MCP").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    }) {
        parsed.trust_project_mcp = true;
    }
    if parsed.no_session
        && (parsed.checkpoints || parsed.list_checkpoints || parsed.rewind.is_some())
    {
        return Err(
            "file checkpoints require persistence; cannot combine with --no-session".into(),
        );
    }
    if parsed.list_checkpoints && parsed.rewind.is_some() {
        return Err("--list-checkpoints conflicts with --rewind".into());
    }
    if (parsed.list_checkpoints || parsed.rewind.is_some())
        && (parsed.prompt.is_some() || parsed.mode == OutputMode::Rpc)
    {
        return Err("checkpoint maintenance requires a standalone CLI command".into());
    }
    parsed.execution_policy()?;
    if parsed.sandbox_status && (parsed.prompt.is_some() || parsed.mode == OutputMode::Rpc) {
        return Err("--sandbox-status requires a standalone CLI command".into());
    }
    // 离线输出清理（DES-17）是显式独立动作：两个动作互斥，且不与 prompt、RPC 或其它
    // 独立执行动作串联。它不解析 Provider/模型，因此允许在任何无凭据环境下运行。
    if parsed.output_cleanup_plan.is_some() && parsed.output_cleanup_confirm.is_some() {
        return Err("--output-cleanup-plan conflicts with --output-cleanup-confirm".into());
    }
    if parsed.output_cleanup_plan.is_some() || parsed.output_cleanup_confirm.is_some() {
        if parsed.prompt.is_some() {
            return Err(
                "output cleanup is a standalone command and cannot be combined with a prompt"
                    .into(),
            );
        }
        if parsed.mode == OutputMode::Rpc {
            return Err(
                "output cleanup is a standalone command and cannot be combined with --mode rpc"
                    .into(),
            );
        }
        let conflict = [
            ("--list-models", parsed.list_models),
            ("--import-antigravity", parsed.import_antigravity.is_some()),
            ("--recovery", parsed.recovery),
            ("--resume", parsed.resume),
            ("--session", parsed.session.is_some()),
            ("--fork", parsed.fork),
            ("--list-sessions", parsed.list),
            ("--export", parsed.export.is_some()),
            ("--list-checkpoints", parsed.list_checkpoints),
            ("--rewind", parsed.rewind.is_some()),
            ("--sandbox-status", parsed.sandbox_status),
            ("--plugin-install", parsed.plugin_install.is_some()),
            ("--plugin-remove", parsed.plugin_remove.is_some()),
            ("--plugin-list", parsed.plugin_list),
            ("--plugin-search", parsed.plugin_search),
            ("--plugin-init", parsed.plugin_init.is_some()),
            ("--plugin-validate", parsed.plugin_validate.is_some()),
        ]
        .into_iter()
        .find(|(_, set)| *set)
        .map(|(flag, _)| flag);
        if let Some(flag) = conflict {
            return Err(format!(
                "output cleanup is a standalone command and cannot be combined with {flag}"
            ));
        }
    }
    if parsed.list_unresolved_effects || parsed.ack_unresolved_effect.is_some() {
        if parsed.list_unresolved_effects && parsed.ack_unresolved_effect.is_some() {
            return Err(
                "--list-unresolved-effects and --ack-unresolved-effect are mutually exclusive"
                    .into(),
            );
        }
        if parsed.no_session {
            return Err(
                "--list-unresolved-effects / --ack-unresolved-effect require the persistent ledger; --no-session disables it"
                    .into(),
            );
        }
        if parsed.mode == OutputMode::Rpc {
            return Err(
                "unresolved-effect maintenance cannot run in --mode rpc; use the RPC ledger view instead"
                    .into(),
            );
        }
        let conflict = [
            ("--list-models", parsed.list_models),
            ("--recovery", parsed.recovery),
            ("--resume", parsed.resume),
            ("--session", parsed.session.is_some()),
            ("--fork", parsed.fork),
            ("--list-sessions", parsed.list),
            ("--export", parsed.export.is_some()),
            ("--list-checkpoints", parsed.list_checkpoints),
            ("--rewind", parsed.rewind.is_some()),
            ("--sandbox-status", parsed.sandbox_status),
            ("--plugin-install", parsed.plugin_install.is_some()),
            ("--plugin-remove", parsed.plugin_remove.is_some()),
            ("--plugin-list", parsed.plugin_list),
            ("--plugin-search", parsed.plugin_search),
            ("--plugin-init", parsed.plugin_init.is_some()),
            ("--plugin-validate", parsed.plugin_validate.is_some()),
            ("--prompt", parsed.prompt.is_some()),
            (
                "--output-cleanup-plan",
                parsed.output_cleanup_plan.is_some(),
            ),
            (
                "--output-cleanup-confirm",
                parsed.output_cleanup_confirm.is_some(),
            ),
        ]
        .into_iter()
        .find(|(_, set)| *set)
        .map(|(flag, _)| flag);
        if let Some(flag) = conflict {
            return Err(format!(
                "unresolved-effect maintenance is a standalone command and cannot be combined with {flag}"
            ));
        }
    }
    // `--output-schema` 只对真实模型运行生效。它不能与不接触模型的独立命令组合，
    // 否则开关会被静默忽略，让人误以为本次输出已受 schema 约束。
    if let Some(path) = &parsed.output_schema {
        if path.as_os_str().is_empty() {
            return Err("--output-schema requires a schema file path".into());
        }
        let standalone = [
            ("--list-models", parsed.list_models),
            ("--import-antigravity", parsed.import_antigravity.is_some()),
            ("--sandbox-status", parsed.sandbox_status),
            ("--recovery", parsed.recovery),
            ("--list-sessions", parsed.list),
            ("--export", parsed.export.is_some()),
            ("--list-checkpoints", parsed.list_checkpoints),
            ("--rewind", parsed.rewind.is_some()),
            ("--plugin-install", parsed.plugin_install.is_some()),
            ("--plugin-remove", parsed.plugin_remove.is_some()),
            ("--plugin-list", parsed.plugin_list),
            ("--plugin-search", parsed.plugin_search),
            ("--plugin-init", parsed.plugin_init.is_some()),
            ("--plugin-validate", parsed.plugin_validate.is_some()),
            (
                "--output-cleanup-plan",
                parsed.output_cleanup_plan.is_some(),
            ),
            (
                "--output-cleanup-confirm",
                parsed.output_cleanup_confirm.is_some(),
            ),
            ("--list-unresolved-effects", parsed.list_unresolved_effects),
            (
                "--ack-unresolved-effect",
                parsed.ack_unresolved_effect.is_some(),
            ),
        ]
        .into_iter()
        .find(|(_, selected)| *selected)
        .map(|(flag, _)| flag);
        if let Some(flag) = standalone {
            return Err(format!(
                "--output-schema applies to a model run and cannot be combined with {flag}"
            ));
        }
    }
    Ok(parsed)
}

impl Args {
    pub(super) fn execution_policy(
        &self,
    ) -> Result<mio_coding_agent::execution_backend::ExecutionPolicy, String> {
        use mio_coding_agent::execution_backend::{
            BubblewrapPolicy, ExecutionPolicy, FilesystemPolicy, ResourceLimits,
            linux_runtime_mounts,
        };
        let limits = ResourceLimits {
            cpu_seconds: self.sandbox_cpu_seconds,
            address_space_bytes: self
                .sandbox_memory_mib
                .map(|value| {
                    value
                        .checked_mul(1024 * 1024)
                        .ok_or("Sandbox memory ceiling overflows bytes")
                })
                .transpose()?,
            open_files: self.sandbox_open_files,
        };
        if limits.cpu_seconds == Some(0)
            || limits.address_space_bytes == Some(0)
            || limits.open_files.is_some_and(|value| value < 3)
        {
            return Err("Sandbox limits must be positive; open files must be at least 3".into());
        }
        match self.sandbox.as_deref().unwrap_or("unrestricted") {
            "unrestricted" if limits.is_unset() && self.sandbox_read_only.is_empty() => {
                Ok(ExecutionPolicy::Unrestricted)
            }
            "unrestricted" => Err("Sandbox limits and mounts require --sandbox bubblewrap".into()),
            "bubblewrap" => {
                let mut read_only = linux_runtime_mounts();
                for path in &self.sandbox_read_only {
                    if !path.is_absolute() {
                        return Err("--sandbox-read-only requires an absolute path".into());
                    }
                    let canonical = path.canonicalize().map_err(|error| {
                        format!("Cannot resolve sandbox read-only mount: {error}")
                    })?;
                    if canonical.parent().is_none() {
                        return Err("The host root cannot be a sandbox read-only mount".into());
                    }
                    if !read_only.contains(path) {
                        read_only.push(path.clone());
                    }
                }
                Ok(ExecutionPolicy::Bubblewrap(BubblewrapPolicy {
                    filesystem: FilesystemPolicy::ExplicitMounts { read_only },
                    limits,
                    ..BubblewrapPolicy::default()
                }))
            }
            _ => Err("--sandbox must be unrestricted or bubblewrap".into()),
        }
    }
}
