use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, RunTools, Services, ThinkingLevel,
    ToolFilter,
    compact::{DEFAULT_AUTO_COMPACT_BYTES, DEFAULT_KEEP_RECENT_MESSAGES, compact_history},
    context::{ContextOptions, ProjectContext, expand_at_files},
    mcp::load_dynamic_tools,
    parse_model_spec,
    rpc::{RpcCommand, list_catalog_models, resolve_provider_endpoint, rpc_response},
    run_with_tools,
    session::Session,
    subagent::SubagentManager,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const HELP: &str = "Mio — Rust coding agent

Usage: mio [OPTIONS] [-p PROMPT] [@FILE...]
Without a prompt, starts an interactive conversation in a terminal or reads stdin.

  -p, --print TEXT       Run one task and exit
  --json                Emit JSON Lines events on stdout (alias for --mode json)
  --mode MODE           Output/protocol mode: text, json, or rpc
  --cwd PATH            Working directory (default: current directory)
  --model NAME          Model name or [provider/]model[:thinking] (or MIO_MODEL)
  --provider NAME       Provider preset: custom, deepseek, antigravity, openai, anthropic,
                        google, openrouter, groq, xai, mistral, ollama (or MIO_PROVIDER)
  --thinking LEVEL      Thinking level: off, minimal, low, medium, high, xhigh, max
  --list-models [QUERY] List available models across provider presets and exit
  --import-antigravity PATH  Import existing local auth.json/settings.json into Mio
  --base-url URL        OpenAI-compatible base URL (or MIO_BASE_URL)
  -t, --tools LIST      Comma-separated tools or +tool/-tool deltas (read,write,edit,bash,grep,find,ls)
  -xt, --exclude-tools LIST  Exclude tools or glob patterns (e.g. bash,mcp__*)
  -nt, --no-tools       Disable all tools
  --no-mcp              Disable MCP server discovery (.mio/mcp.json)
  --approve             Require interactive confirmation before write/edit/bash/MCP tools
  --session SELECTOR    Load/save a session by path, short UUID prefix, or session name
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
  --subagents           Enable native child sessions (4 concurrent; memory only)
  --auto-compact        Automatically compact history when exceeding 512 KiB
  --max-turns N         Tool-loop budget, 1–256 (default: 32)
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
Sessions: .mio/sessions/*.json, saved after successful turns (unless --no-session).
Interactive commands: /help, /new, /fork, /sessions, /name, /compact, /export, /model,
/thinking, /plugins, /plugin, /subagents, /status, /clear, /exit. Ctrl-C cancels and exits.
Shell is noninteractive and runs with your user permissions, not in a sandbox.
";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OutputMode {
    #[default]
    Text,
    Json,
    Rpc,
}

#[derive(Default)]
struct Args {
    cwd: Option<PathBuf>,
    model: Option<String>,
    provider: Option<String>,
    thinking: Option<ThinkingLevel>,
    list_models: bool,
    list_models_query: Option<String>,
    import_antigravity: Option<PathBuf>,
    base_url: Option<String>,
    prompt: Option<String>,
    mode: OutputMode,
    json: bool,
    tools: Option<String>,
    exclude_tools: Option<String>,
    no_tools: bool,
    no_mcp: bool,
    approve: bool,
    session: Option<String>,
    session_name: Option<String>,
    no_session: bool,
    export: Option<PathBuf>,
    resume: bool,
    fork: bool,
    list: bool,
    skills: Vec<String>,
    template: Option<String>,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    no_context: bool,
    no_context_files: bool,
    no_skills: bool,
    no_prompt_templates: bool,
    no_shell: bool,
    subagents: bool,
    auto_compact: bool,
    max_turns: Option<usize>,
    plugin_install: Option<String>,
    plugin_remove: Option<String>,
    plugin_list: bool,
    plugin_search: bool,
    plugin_search_query: Option<String>,
    plugin_init: Option<PathBuf>,
    plugin_validate: Option<PathBuf>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
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
            "--approve" => parsed.approve = true,
            "--session" => parsed.session = Some(value()?),
            "-n" | "--name" => parsed.session_name = Some(value()?),
            "--no-session" => parsed.no_session = true,
            "--export" => parsed.export = Some(value()?.into()),
            "--skill" => parsed.skills.push(value()?),
            "--template" => parsed.template = Some(value()?),
            "--system-prompt" => parsed.system_prompt = Some(value()?),
            "--append-system-prompt" => parsed.append_system_prompt = Some(value()?),
            "--max-turns" => {
                parsed.max_turns = Some(value()?.parse().map_err(|_| "Invalid --max-turns")?)
            }
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
            "--subagents" => parsed.subagents = true,
            "--auto-compact" => parsed.auto_compact = true,
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
    Ok(parsed)
}

struct CliHost {
    json: bool,
    no_shell: bool,
    approve: bool,
    steering_queue: Mutex<VecDeque<String>>,
}

impl CliHost {
    fn push_steering(&self, msg: String) {
        self.steering_queue.lock().unwrap().push_back(msg);
    }
}

impl Host for CliHost {
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        let (command, cwd, disabled) = (command.to_owned(), cwd.to_owned(), self.no_shell);
        Box::pin(async move {
            if disabled {
                return Err("Shell disabled by --no-shell".into());
            }
            mio_coding_agent::shell::execute(&command, &cwd).await
        })
    }

    fn on_event(&self, event: AgentEvent) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&event).expect("event serialization")
            );
        } else {
            match event {
                AgentEvent::Text(text) => {
                    print!("{text}");
                    if let Err(e) = io::stdout().flush() {
                        eprintln!("Output error: {e}");
                    }
                }
                AgentEvent::ToolStarted { operation, target } => {
                    eprintln!("\n[{operation}] {target}")
                }
                AgentEvent::ToolFinished {
                    operation, result, ..
                } => {
                    if let Err(error) = result {
                        eprintln!("[{operation} failed] {error}");
                    }
                }
            }
        }
    }

    fn poll_steering(&self) -> Option<String> {
        let mut guard = self.steering_queue.lock().unwrap();
        if guard.is_empty() {
            None
        } else {
            let combined: Vec<String> = guard.drain(..).collect();
            Some(combined.join("\n"))
        }
    }

    fn approve_tool(&self, operation: &str, target: &str) -> Result<(), String> {
        if !self.approve {
            return Ok(());
        }
        if self.json || !io::stdin().is_terminal() {
            return Err(format!(
                "Tool '{operation}' ({target}) requires interactive approval (--approve)"
            ));
        }
        eprint!("\nApprove [{operation}] {target}? [y/N]: ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("Failed to read approval input: {e}"))?;
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            Ok(())
        } else {
            Err(format!("User rejected tool operation '{operation}'"))
        }
    }
}

fn session_list(
    directory: &Path,
) -> Result<Vec<mio_coding_agent::session::SessionMetadata>, String> {
    if !directory.try_exists().map_err(|e| e.to_string())? {
        return Ok(Vec::new());
    }
    let (list, warnings) = Session::list(directory)?;
    for warning in warnings {
        eprintln!("Warning: {warning}");
    }
    // Session timestamps use seconds. File timestamps distinguish saves made
    // within the same second, so a freshly forked task is selected reliably.
    let mut dated = list
        .into_iter()
        .map(|session| {
            let modified = std::fs::metadata(&session.file_path)
                .and_then(|metadata| metadata.modified())
                .map_err(|e| e.to_string())?;
            Ok((modified, session))
        })
        .collect::<Result<Vec<_>, String>>()?;
    dated.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    Ok(dated.into_iter().map(|(_, session)| session).collect())
}

fn print_sessions(directory: &Path, json: bool) -> Result<(), String> {
    for session in session_list(directory)? {
        if json {
            println!("{}", serde_json::json!({"type":"session", "data":session}));
        } else if let Some(name) = &session.name {
            println!(
                "{}  [{}]  {} messages  {}",
                session.id,
                name,
                session.message_count,
                session.file_path.display()
            );
        } else {
            println!(
                "{}  {} messages  {}",
                session.id,
                session.message_count,
                session.file_path.display()
            );
        }
    }
    Ok(())
}

fn print_models(query: Option<&str>, json: bool) {
    let entries = list_catalog_models(query);
    if json {
        for entry in entries {
            println!("{}", json!({"type": "model", "data": entry}));
        }
    } else {
        for entry in entries {
            let status = if entry.configured { "ready" } else { "unset" };
            let thinking = if entry.supports_thinking {
                "thinking"
            } else {
                "standard"
            };
            println!(
                "{}/{}  [{}] ({}, {})",
                entry.provider, entry.model, status, thinking, entry.env_var
            );
        }
    }
}

struct Runner {
    config: Config,
    options: RunOptions,
    tool_filter: ToolFilter,
    no_mcp: bool,
    no_session: bool,
    host: Arc<CliHost>,
    session: Session,
    path: PathBuf,
    subagents: Option<Arc<SubagentManager>>,
    antigravity: Option<mio_coding_agent::antigravity::Antigravity>,
}

impl Runner {
    fn save_session(&mut self) -> Result<(), String> {
        if self.no_session {
            return Ok(());
        }
        let parent = self.path.parent().ok_or("Invalid session path")?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create session directory: {e}"))?;
        self.session.save(&self.path)
    }

    async fn turn(&mut self, prompt: &str) -> Result<(), String> {
        let expanded_prompt = expand_at_files(&self.session.cwd, prompt)?;
        let extra_tools = if self.no_mcp {
            Vec::new()
        } else {
            load_dynamic_tools(&self.session.cwd, self.host.clone(), &self.tool_filter).await?
        };
        let history = tokio::select! {
            result = run_with_tools(
                self.config.clone(),
                self.session.history.clone(),
                &expanded_prompt,
                &self.session.cwd,
                self.host.clone(),
                Services { antigravity: self.antigravity.as_ref(), ..Services::default() },
                RunTools {
                    options: self.options.clone(),
                    read_only: false,
                    subagents: self.subagents.clone(),
                    tool_filter: self.tool_filter.clone(),
                    extra_tools,
                }
            ) => result?,
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|e| e.to_string())?;
                return Err("Cancelled; previous session retained. File changes are not rolled back.".into());
            }
        };
        if let Some(manager) = &self.subagents {
            let active: Vec<_> = manager
                .list()
                .into_iter()
                .filter(|run| run.status == mio_coding_agent::subagent::Status::Running)
                .collect();
            if !active.is_empty() {
                for run in active {
                    manager.kill(run.run_id).await?;
                }
                return Err("Parent ended before collecting child tasks; unfinished children cancelled. File changes are not rolled back.".into());
            }
        }
        self.session.history = history;
        self.save_session()?;
        if self.host.json {
            println!(
                "{}",
                serde_json::json!({
                    "type": "done",
                    "session": if self.no_session { serde_json::Value::Null } else { json!(self.path) },
                    "messages": self.session.history.len()
                })
            );
        } else {
            println!();
            if !self.no_session {
                eprintln!("Session: {}", self.path.display());
            }
        }
        Ok(())
    }
}

async fn run_rpc_loop(runner: &mut Runner, directory: &Path, cwd: &Path) -> Result<(), String> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut follow_up_queue: VecDeque<String> = VecDeque::new();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cmd: RpcCommand = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(err) => {
                println!(
                    "{}",
                    rpc_response("parse", None, false, json!({"error": err.to_string()}))
                );
                continue;
            }
        };
        match cmd {
            RpcCommand::Prompt { id, message } => {
                println!(
                    "{}",
                    rpc_response("prompt", id.as_deref(), true, json!({"status": "started"}))
                );
                if let Err(err) = runner.turn(&message).await {
                    println!("{}", json!({"type": "error", "error": err}));
                }
                while let Some(next_msg) = follow_up_queue.pop_front() {
                    if let Err(err) = runner.turn(&next_msg).await {
                        println!("{}", json!({"type": "error", "error": err}));
                        break;
                    }
                }
            }
            RpcCommand::Steer { id, message } => {
                runner.host.push_steering(message);
                println!(
                    "{}",
                    rpc_response("steer", id.as_deref(), true, json!({"queued": true}))
                );
            }
            RpcCommand::FollowUp { id, message } => {
                follow_up_queue.push_back(message);
                println!(
                    "{}",
                    rpc_response(
                        "follow_up",
                        id.as_deref(),
                        true,
                        json!({"queued": follow_up_queue.len()})
                    )
                );
            }
            RpcCommand::Abort { id } => {
                println!(
                    "{}",
                    rpc_response("abort", id.as_deref(), true, json!({"aborted": true}))
                );
            }
            RpcCommand::Compact { id, instructions } => {
                let (compacted, stats) = compact_history(
                    &runner.session.history,
                    DEFAULT_KEEP_RECENT_MESSAGES,
                    instructions.as_deref(),
                );
                runner.session.history = compacted;
                let _ = runner.save_session();
                println!(
                    "{}",
                    rpc_response("compact", id.as_deref(), true, json!(stats))
                );
            }
            RpcCommand::GetState { id } => {
                println!(
                    "{}",
                    rpc_response(
                        "get_state",
                        id.as_deref(),
                        true,
                        json!({
                            "sessionId": runner.session.id,
                            "sessionName": runner.session.name,
                            "session_name": runner.session.name,
                            "sessionFile": if runner.no_session { serde_json::Value::Null } else { json!(runner.path) },
                            "model": runner.config.model,
                            "provider": runner.config.provider,
                            "thinkingLevel": runner.options.thinking,
                            "messageCount": runner.session.history.len(),
                        })
                    )
                );
            }
            RpcCommand::SetModel {
                id,
                model,
                thinking,
            } => {
                let (_, clean_model, inline_thinking) = parse_model_spec(&model);
                runner.config.model = clean_model;
                if let Some(t) = thinking.or(inline_thinking) {
                    runner.options.thinking = Some(t);
                }
                println!(
                    "{}",
                    rpc_response(
                        "set_model",
                        id.as_deref(),
                        true,
                        json!({
                            "model": runner.config.model,
                            "thinkingLevel": runner.options.thinking,
                        })
                    )
                );
            }
            RpcCommand::NewSession { id, name } => {
                let mut s = Session::new(cwd)?;
                s.set_name(name);
                runner.path = directory.join(format!("{}.json", s.id));
                runner.session = s;
                println!(
                    "{}",
                    rpc_response(
                        "new_session",
                        id.as_deref(),
                        true,
                        json!({"sessionId": runner.session.id, "name": runner.session.name})
                    )
                );
            }
            RpcCommand::ForkSession { id } => {
                runner.session = runner.session.fork();
                runner.path = directory.join(format!("{}.json", runner.session.id));
                let _ = runner.save_session();
                println!(
                    "{}",
                    rpc_response(
                        "fork_session",
                        id.as_deref(),
                        true,
                        json!({"sessionId": runner.session.id})
                    )
                );
            }
            RpcCommand::SetSessionName { id, name } => {
                runner.session.set_name(Some(name));
                let _ = runner.save_session();
                println!(
                    "{}",
                    rpc_response(
                        "set_session_name",
                        id.as_deref(),
                        true,
                        json!({
                            "sessionId": runner.session.id,
                            "sessionName": runner.session.name,
                            "session_name": runner.session.name,
                        })
                    )
                );
            }
            RpcCommand::ExportHtml { id, output_path } => {
                let out_path = match output_path {
                    Some(ref p) => {
                        let pb = PathBuf::from(p);
                        if pb.is_absolute() { pb } else { cwd.join(pb) }
                    }
                    None => cwd.join(format!("mio-session-{}.html", runner.session.id)),
                };
                match runner.session.export_html(&out_path) {
                    Ok(()) => {
                        println!(
                            "{}",
                            rpc_response(
                                "export_html",
                                id.as_deref(),
                                true,
                                json!({"path": out_path})
                            )
                        );
                    }
                    Err(err) => {
                        println!(
                            "{}",
                            rpc_response(
                                "export_html",
                                id.as_deref(),
                                false,
                                json!({"error": err})
                            )
                        );
                    }
                }
            }
            RpcCommand::ApproveResponse { request_id, .. } => {
                println!(
                    "{}",
                    rpc_response("approve_response", Some(&request_id), true, json!({}))
                );
            }
        }
    }
    Ok(())
}

async fn execute(args: Args) -> Result<(), String> {
    if args.list_models {
        print_models(args.list_models_query.as_deref(), args.json);
        return Ok(());
    }
    if args.plugin_search {
        let entries =
            mio_coding_agent::plugin::search_community_plugins(args.plugin_search_query.as_deref());
        if args.json {
            for e in entries {
                println!("{}", json!({"type": "community_plugin", "data": e}));
            }
        } else {
            for e in entries {
                println!(
                    "{} v{}  {}  [{}]",
                    e.name,
                    e.version,
                    e.description,
                    e.keywords.join(", ")
                );
            }
        }
        return Ok(());
    }
    if let Some(ref init_dir) = args.plugin_init {
        let name = init_dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid --plugin-init directory name")?;
        let created = mio_coding_agent::plugin::scaffold_plugin(init_dir, name)?;
        if args.json {
            println!(
                "{}",
                json!({"type": "plugin_initialized", "name": name, "path": created})
            );
        } else {
            println!(
                "Initialized standard Mio plugin '{name}' at {}",
                created.display()
            );
        }
        return Ok(());
    }
    if let Some(ref val_path) = args.plugin_validate {
        if val_path.is_file() {
            let reg = mio_coding_agent::plugin::validate_registry_file(val_path)?;
            if args.json {
                println!(
                    "{}",
                    json!({"type": "registry_valid", "plugins": reg.plugins.len(), "path": val_path})
                );
            } else {
                println!(
                    "Valid registry.json ({} plugins) at {}",
                    reg.plugins.len(),
                    val_path.display()
                );
            }
        } else {
            let manifest = mio_coding_agent::plugin::validate_plugin_dir(val_path)?;
            if args.json {
                println!(
                    "{}",
                    json!({"type": "plugin_valid", "data": manifest, "path": val_path})
                );
            } else {
                println!(
                    "Valid Mio plugin '{} v{}' at {}",
                    manifest.name,
                    manifest.version,
                    val_path.display()
                );
            }
        }
        return Ok(());
    }
    if let Some(source) = &args.import_antigravity {
        if args.prompt.is_some() || args.resume || args.session.is_some() {
            return Err("Import authorization separately from running a task".into());
        }
        let result = mio_coding_agent::profile::import_antigravity(
            source,
            &mio_coding_agent::profile::config_directory()?,
        )?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({"type":"profile_imported","provider":"antigravity","data":result})
            );
        } else {
            println!(
                "Imported Antigravity account and model {} into Mio; source files unchanged.",
                result.model
            );
        }
        return Ok(());
    }
    let cwd = args
        .cwd
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?)
        .canonicalize()
        .map_err(|e| format!("Invalid workspace: {e}"))?;
    if !cwd.is_dir() {
        return Err("Workspace must be a directory".into());
    }
    if let Some(ref spec) = args.plugin_install {
        let installed = mio_coding_agent::plugin::install_plugin(&cwd, spec)?;
        if args.json {
            println!(
                "{}",
                json!({"type": "plugin_installed", "data": installed.manifest, "path": installed.root_dir})
            );
        } else {
            println!(
                "Installed plugin '{} v{}' into {}",
                installed.manifest.name,
                installed.manifest.version,
                installed.root_dir.display()
            );
        }
        return Ok(());
    }
    if let Some(ref name) = args.plugin_remove {
        mio_coding_agent::plugin::remove_plugin(&cwd, name)?;
        if args.json {
            println!("{}", json!({"type": "plugin_removed", "name": name}));
        } else {
            println!("Removed plugin '{name}'");
        }
        return Ok(());
    }
    if args.plugin_list {
        let installed = mio_coding_agent::plugin::discover_plugins(&cwd)?;
        if args.json {
            for p in installed {
                println!(
                    "{}",
                    json!({"type": "plugin", "data": p.manifest, "path": p.root_dir})
                );
            }
        } else if installed.is_empty() {
            println!("No plugins installed in {}", cwd.display());
        } else {
            for p in installed {
                println!(
                    "{} v{}  {}  ({})",
                    p.manifest.name,
                    p.manifest.version,
                    p.manifest.description,
                    p.root_dir.display()
                );
            }
        }
        return Ok(());
    }
    let directory = cwd.join(".mio/sessions");
    if args.list {
        return print_sessions(&directory, args.json);
    }
    let selected = if args.resume {
        Some(
            session_list(&directory)?
                .first()
                .ok_or("No saved sessions in this workspace")?
                .file_path
                .clone(),
        )
    } else if let Some(ref selector) = args.session {
        Some(Session::resolve(&directory, &cwd, selector)?)
    } else {
        None
    };
    if let Some(export_path) = &args.export {
        let target_session_path = match selected {
            Some(p) => p,
            None => session_list(&directory)?
                .first()
                .ok_or("No saved sessions to export")?
                .file_path
                .clone(),
        };
        let session = Session::load(&target_session_path)?;
        let out_path = if export_path.is_absolute() {
            export_path.clone()
        } else {
            cwd.join(export_path)
        };
        session.export_html(&out_path)?;
        if args.json {
            println!(
                "{}",
                json!({"type": "exported", "session": session.id, "path": out_path})
            );
        } else {
            println!("Exported session {} to {}", session.id, out_path.display());
        }
        return Ok(());
    }
    let mut session = match selected.as_ref() {
        Some(path) if path.try_exists().map_err(|e| e.to_string())? => Session::load(path)?,
        Some(_) if args.fork => return Err("Cannot fork a missing session".into()),
        _ => Session::new(&cwd)?,
    };
    if session.cwd != cwd {
        return Err(format!(
            "Session belongs to {}; select that --cwd",
            session.cwd.display()
        ));
    }
    if args.fork {
        session = session.fork();
    }
    if let Some(ref name) = args.session_name {
        session.set_name(Some(name.clone()));
    }
    let path = if args.fork { None } else { selected }
        .unwrap_or_else(|| directory.join(format!("{}.json", session.id)));
    let context = if args.no_context {
        None
    } else {
        Some(ProjectContext::load_with_options(
            &cwd,
            &ContextOptions {
                no_context_files: args.no_context_files,
                no_skills: args.no_skills,
                no_prompt_templates: args.no_prompt_templates,
                system_prompt: args.system_prompt.clone(),
                append_system_prompt: args.append_system_prompt.clone(),
            },
        )?)
    };
    let mut instructions = context
        .as_ref()
        .map(|ctx| ctx.preamble.clone())
        .unwrap_or_default();
    if let Some(ctx) = &context {
        for name in &args.skills {
            instructions.push_str(&format!(
                "\n--- Selected skill: {name} ---\n{}",
                ctx.select_skill(name)?
            ));
        }
    }
    if instructions.len() > 128 * 1024 {
        return Err("Project instructions and selected skills exceed 128 KiB".into());
    }
    let tool_filter = ToolFilter::from_cli(
        args.tools.as_deref(),
        args.exclude_tools.as_deref(),
        args.no_tools,
    )?;
    let raw_model_arg = args.model.or_else(|| std::env::var("MIO_MODEL").ok());
    let (shorthand_provider, parsed_model, shorthand_thinking) = match &raw_model_arg {
        Some(raw) => {
            let (prov, mdl, thk) = parse_model_spec(raw);
            (prov, Some(mdl), thk)
        }
        None => (None, None, None),
    };
    let profile_directory = mio_coding_agent::profile::config_directory()?;
    let explicit_base = args.base_url.or_else(|| std::env::var("MIO_BASE_URL").ok());
    let selected_provider = args
        .provider
        .or(shorthand_provider)
        .or_else(|| std::env::var("MIO_PROVIDER").ok());
    let profile = if selected_provider
        .as_deref()
        .is_some_and(|p| p != "antigravity")
        || (selected_provider.is_none() && explicit_base.is_some())
    {
        None
    } else {
        mio_coding_agent::profile::summary(&profile_directory)?
    };
    let provider_str = selected_provider
        .as_deref()
        .unwrap_or(if profile.is_some() {
            "antigravity"
        } else {
            "custom"
        });
    let (provider, resolved_base, resolved_key) =
        resolve_provider_endpoint(provider_str, explicit_base)?;
    let antigravity = if provider == Provider::Antigravity {
        Some(mio_coding_agent::antigravity::Antigravity::from_profile(
            profile_directory,
        )?)
    } else {
        None
    };
    let config = Config {
        provider,
        model: parsed_model
            .or_else(|| profile.map(|p| p.model))
            .ok_or("Set --model or MIO_MODEL")?,
        base_url: resolved_base,
        api_key: resolved_key,
    };
    let options = RunOptions {
        instructions,
        max_turns: args.max_turns.unwrap_or(32),
        thinking: args.thinking.or(shorthand_thinking),
        auto_compact_bytes: if args.auto_compact {
            Some(DEFAULT_AUTO_COMPACT_BYTES)
        } else {
            None
        },
    };
    let subagents = if args.subagents {
        Some(Arc::new(if let Some(account) = &antigravity {
            SubagentManager::new_with_antigravity(
                &cwd,
                config.clone(),
                options.clone(),
                account.clone(),
            )?
        } else {
            SubagentManager::new(&cwd, config.clone(), options.clone())?
        }))
    } else {
        None
    };
    let mut runner = Runner {
        config,
        options,
        tool_filter,
        no_mcp: args.no_mcp,
        no_session: args.no_session,
        subagents,
        antigravity,
        host: Arc::new(CliHost {
            json: args.json,
            no_shell: args.no_shell,
            approve: args.approve,
            steering_queue: Mutex::new(VecDeque::new()),
        }),
        session,
        path,
    };
    let render = |prompt: &str| -> Result<String, String> {
        if let Some(name) = &args.template {
            context
                .as_ref()
                .ok_or("Missing project context")?
                .template(name, prompt)
        } else if !args.no_prompt_templates
            && let Some(stripped) = prompt.trim().strip_prefix('/')
        {
            let (cmd, rest) = stripped
                .split_once(char::is_whitespace)
                .unwrap_or((stripped, ""));
            if let Some(ctx) = &context
                && let Ok(rendered) = ctx.template(cmd, rest.trim())
            {
                return Ok(rendered);
            }
            Ok(prompt.to_owned())
        } else {
            Ok(prompt.to_owned())
        }
    };
    if args.mode == OutputMode::Rpc {
        return run_rpc_loop(&mut runner, &directory, &cwd).await;
    }
    if let Some(prompt) = args.prompt {
        return runner.turn(&render(&prompt)?).await;
    }
    if !io::stdin().is_terminal() {
        let mut prompt = String::new();
        tokio::io::stdin()
            .take(65537)
            .read_to_string(&mut prompt)
            .await
            .map_err(|e| e.to_string())?;
        return runner.turn(&render(&prompt)?).await;
    }
    if args.json {
        return Err("--json requires -p, piped input, or --mode rpc".into());
    }
    eprintln!(
        "Mio · {} · model: {}{}\n/help for commands. Shell {}. Workspace: {}",
        env!("CARGO_PKG_VERSION"),
        runner.config.model,
        runner
            .options
            .thinking
            .map(|t| format!(" (thinking: {})", t.as_str()))
            .unwrap_or_default(),
        if args.no_shell { "disabled" } else { "enabled" },
        cwd.display()
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        eprint!("mio> ");
        io::stderr().flush().map_err(|e| e.to_string())?;
        let line = tokio::select! {
            line = lines.next_line() => line.map_err(|e| e.to_string())?,
            signal = tokio::signal::ctrl_c() => { signal.map_err(|e| e.to_string())?; break; }
        };
        let Some(line) = line else { break };
        let trimmed = line.trim();
        match trimmed {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => {
                println!("{HELP}");
                continue;
            }
            "/clear" => {
                print!("\x1b[2J\x1b[H");
                let _ = io::stdout().flush();
                continue;
            }
            "/sessions" => {
                print_sessions(&directory, false)?;
                continue;
            }
            "/status" => {
                eprintln!(
                    "Session: {}{}\nModel: {} ({:?}, thinking: {})\nMessages: {} ({} bytes)",
                    runner.session.id,
                    runner
                        .session
                        .name
                        .as_deref()
                        .map(|n| format!(" [{n}]"))
                        .unwrap_or_default(),
                    runner.config.model,
                    runner.config.provider,
                    runner
                        .options
                        .thinking
                        .map(|t| t.as_str())
                        .unwrap_or("default"),
                    runner.session.history.len(),
                    mio_coding_agent::compact::history_bytes(&runner.session.history)
                );
                continue;
            }
            "/subagents" => {
                if let Some(manager) = &runner.subagents {
                    let list = manager.list();
                    if list.is_empty() {
                        eprintln!("No child subagent runs in this session.");
                    } else {
                        for snap in list {
                            eprintln!(
                                "{}  [{}]  {:?}  {}",
                                snap.run_id, snap.session, snap.status, snap.goal
                            );
                        }
                    }
                } else {
                    eprintln!("Subagents are disabled (launch with --subagents).");
                }
                continue;
            }
            "/new" | "/fork" => {
                runner.session = if trimmed == "/fork" {
                    runner.session.fork()
                } else {
                    Session::new(&cwd)?
                };
                runner.path = directory.join(format!("{}.json", runner.session.id));
                eprintln!("Selected session {}", runner.session.id);
                continue;
            }
            _ if trimmed.starts_with("/name") => {
                let title = trimmed.strip_prefix("/name").unwrap_or("").trim();
                if title.is_empty() {
                    eprintln!(
                        "Session name: {}",
                        runner.session.name.as_deref().unwrap_or("(unnamed)")
                    );
                } else {
                    runner.session.set_name(Some(title.to_owned()));
                    if !runner.session.history.is_empty() {
                        let _ = runner.save_session();
                    }
                    eprintln!("Session named '{title}'");
                }
                continue;
            }
            _ if trimmed.starts_with("/compact") => {
                let focus = trimmed.strip_prefix("/compact").unwrap_or("").trim();
                let (compacted, stats) = compact_history(
                    &runner.session.history,
                    DEFAULT_KEEP_RECENT_MESSAGES,
                    if focus.is_empty() { None } else { Some(focus) },
                );
                runner.session.history = compacted;
                if !runner.session.history.is_empty() {
                    let _ = runner.save_session();
                }
                eprintln!(
                    "Compacted {} -> {} messages ({} -> {} bytes)",
                    stats.original_messages,
                    stats.compacted_messages,
                    stats.original_bytes,
                    stats.compacted_bytes
                );
                continue;
            }
            _ if trimmed.starts_with("/export") => {
                let arg = trimmed.strip_prefix("/export").unwrap_or("").trim();
                let out_path = if arg.is_empty() {
                    cwd.join(format!("mio-session-{}.html", runner.session.id))
                } else {
                    cwd.join(arg)
                };
                runner.session.export_html(&out_path)?;
                eprintln!("Exported session to {}", out_path.display());
                continue;
            }
            _ if trimmed.starts_with("/model") => {
                let arg = trimmed.strip_prefix("/model").unwrap_or("").trim();
                if arg.is_empty() {
                    eprintln!("Current model: {}", runner.config.model);
                } else {
                    let (_, clean_model, inline_thinking) = parse_model_spec(arg);
                    runner.config.model = clean_model;
                    if let Some(t) = inline_thinking {
                        runner.options.thinking = Some(t);
                    }
                    eprintln!("Switched model to {}", runner.config.model);
                }
                continue;
            }
            _ if trimmed.starts_with("/thinking") => {
                let arg = trimmed.strip_prefix("/thinking").unwrap_or("").trim();
                if arg.is_empty() {
                    eprintln!(
                        "Current thinking level: {}",
                        runner
                            .options
                            .thinking
                            .map(|t| t.as_str())
                            .unwrap_or("default")
                    );
                } else {
                    let level = ThinkingLevel::from_str(arg)?;
                    runner.options.thinking = Some(level);
                    eprintln!("Thinking level set to {}", level.as_str());
                }
                continue;
            }
            "/plugins" => {
                let installed = mio_coding_agent::plugin::discover_plugins(&cwd)?;
                if installed.is_empty() {
                    eprintln!("No plugins installed in this workspace.");
                } else {
                    for p in installed {
                        eprintln!(
                            "{} v{}  {}  ({})",
                            p.manifest.name,
                            p.manifest.version,
                            p.manifest.description,
                            p.root_dir.display()
                        );
                    }
                }
                continue;
            }
            _ if trimmed.starts_with("/plugin") => {
                let rest = trimmed.strip_prefix("/plugin").unwrap_or("").trim();
                let (sub, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let arg = arg.trim();
                match sub {
                    "" | "list" => {
                        let installed = mio_coding_agent::plugin::discover_plugins(&cwd)?;
                        if installed.is_empty() {
                            eprintln!("No plugins installed in this workspace.");
                        } else {
                            for p in installed {
                                eprintln!(
                                    "{} v{}  {}  ({})",
                                    p.manifest.name,
                                    p.manifest.version,
                                    p.manifest.description,
                                    p.root_dir.display()
                                );
                            }
                        }
                    }
                    "search" => {
                        let q = if arg.is_empty() { None } else { Some(arg) };
                        for e in mio_coding_agent::plugin::search_community_plugins(q) {
                            eprintln!(
                                "{} v{}  {}  [{}]",
                                e.name,
                                e.version,
                                e.description,
                                e.keywords.join(", ")
                            );
                        }
                    }
                    "install" => {
                        let installed = mio_coding_agent::plugin::install_plugin(&cwd, arg)?;
                        eprintln!(
                            "Installed plugin '{} v{}' into {}",
                            installed.manifest.name,
                            installed.manifest.version,
                            installed.root_dir.display()
                        );
                    }
                    "remove" => {
                        mio_coding_agent::plugin::remove_plugin(&cwd, arg)?;
                        eprintln!("Removed plugin '{arg}'");
                    }
                    "init" => {
                        let dir = cwd.join(arg);
                        let name = dir
                            .file_name()
                            .and_then(|s| s.to_str())
                            .ok_or("Invalid plugin directory")?;
                        let created = mio_coding_agent::plugin::scaffold_plugin(&dir, name)?;
                        eprintln!("Initialized plugin '{name}' at {}", created.display());
                    }
                    "validate" => {
                        let target = if arg.is_empty() {
                            cwd.clone()
                        } else {
                            cwd.join(arg)
                        };
                        let m = mio_coding_agent::plugin::validate_plugin_dir(&target)?;
                        eprintln!("Valid plugin '{} v{}'", m.name, m.version);
                    }
                    _ => {
                        eprintln!(
                            "Usage: /plugin <list|search [q]|install <spec>|remove <name>|init <dir>|validate [dir]>"
                        );
                    }
                }
                continue;
            }
            _ => runner.turn(&render(&line)?).await?,
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if arguments
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("Mio {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let json = arguments
        .iter()
        .any(|arg| arg == "--json" || arg == "json" || arg == "rpc");
    let result = match parse(arguments.into_iter()) {
        Ok(args) => execute(args).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json {
                println!("{}", serde_json::json!({"type":"error", "error":error}));
            } else {
                eprintln!("Mio: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_session_uses_save_order_even_when_session_seconds_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = Session::new(dir.path()).unwrap();
        let mut fork = original.fork();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        original.save(&a).unwrap();
        fork.save(&b).unwrap();
        for (path, nanos) in [(&a, 1), (&b, 2)] {
            let times = std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos));
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(times)
                .unwrap();
        }
        let list = session_list(dir.path()).unwrap();
        assert_eq!(list[0].id, fork.id);
    }
}
