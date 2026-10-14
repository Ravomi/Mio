use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, RunTools, Services,
    context::ProjectContext, run_with_tools, session::Session, subagent::SubagentManager,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const HELP: &str = "Mio — Rust coding agent

Usage: mio [OPTIONS] [-p PROMPT]
Without a prompt, starts a line-based conversation in a terminal or reads stdin.

  -p, --print TEXT       Run one task and exit
  --json                Emit JSON Lines events on stdout (one task only)
  --cwd PATH            Working directory (default: current directory)
  --model NAME          Model name (or MIO_MODEL)
  --provider NAME       custom, deepseek, or antigravity (or MIO_PROVIDER)
  --import-antigravity PATH  Import existing local auth.json/settings.json into Mio
  --base-url URL        OpenAI-compatible base URL (or MIO_BASE_URL)
  --session PATH        Load/save a session file, relative to --cwd
  --resume              Continue the most recently saved session in this workspace
  --fork                Fork the selected --session or --resume into a new session
  --list-sessions       List local sessions without contacting a model
  --skill NAME          Include a project skill (repeatable)
  --template NAME       Use .mio/prompts/NAME.md; prompt replaces $ARGUMENTS
  --no-context          Do not load project instructions or skills
  --no-shell            Disable shell execution (file tools remain available)
  --subagents           Enable native child sessions (4 concurrent; memory only)
  --max-turns N         Tool-loop budget, 1–256 (default: 32)
  -h, --help            Show help
  -V, --version         Show version

Credentials: MIO_API_KEY (fallback OPENAI_API_KEY). Never saved as configuration.
CLI supports OpenAI-compatible APIs and imported Antigravity OAuth accounts.
Sessions: .mio/sessions/*.json, saved after successful turns. No automatic compaction.
Interactive commands: /help, /new, /fork, /sessions, /exit. Ctrl-C cancels and exits.
Shell is noninteractive and runs with your user permissions, not in a sandbox.
";

#[derive(Default)]
struct Args {
    cwd: Option<PathBuf>,
    model: Option<String>,
    provider: Option<String>,
    import_antigravity: Option<PathBuf>,
    base_url: Option<String>,
    prompt: Option<String>,
    json: bool,
    session: Option<PathBuf>,
    resume: bool,
    fork: bool,
    list: bool,
    skills: Vec<String>,
    template: Option<String>,
    no_context: bool,
    no_shell: bool,
    subagents: bool,
    max_turns: Option<usize>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--cwd" => parsed.cwd = Some(value()?.into()),
            "--model" => parsed.model = Some(value()?),
            "--provider" => parsed.provider = Some(value()?),
            "--import-antigravity" => parsed.import_antigravity = Some(value()?.into()),
            "--base-url" => parsed.base_url = Some(value()?),
            "-p" | "--print" => parsed.prompt = Some(value()?),
            "--session" => parsed.session = Some(value()?.into()),
            "--skill" => parsed.skills.push(value()?),
            "--template" => parsed.template = Some(value()?),
            "--max-turns" => {
                parsed.max_turns = Some(value()?.parse().map_err(|_| "Invalid --max-turns")?)
            }
            "--json" => parsed.json = true,
            "--resume" => parsed.resume = true,
            "--fork" => parsed.fork = true,
            "--list-sessions" => parsed.list = true,
            "--no-context" => parsed.no_context = true,
            "--no-shell" => parsed.no_shell = true,
            "--subagents" => parsed.subagents = true,
            "--" => {
                if parsed.prompt.is_some() {
                    return Err("Prompt already provided".into());
                }
                parsed.prompt = Some(args.collect::<Vec<_>>().join(" "));
                break;
            }
            _ => return Err(format!("Unknown option {arg}; use --help")),
        }
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
    if !(1..=256).contains(&parsed.max_turns.unwrap_or(32)) {
        return Err("--max-turns must be 1–256".into());
    }
    Ok(parsed)
}

struct CliHost {
    json: bool,
    no_shell: bool,
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

struct Runner {
    config: Config,
    options: RunOptions,
    host: Arc<CliHost>,
    session: Session,
    path: PathBuf,
    subagents: Option<Arc<SubagentManager>>,
    antigravity: Option<mio_coding_agent::antigravity::Antigravity>,
}

impl Runner {
    async fn turn(&mut self, prompt: &str) -> Result<(), String> {
        let history = tokio::select! {
            result = run_with_tools(self.config.clone(), self.session.history.clone(), prompt,
                &self.session.cwd, self.host.clone(), Services { antigravity: self.antigravity.as_ref(), ..Services::default() }, RunTools {
                    options: self.options.clone(), read_only: false, subagents: self.subagents.clone(),
                }) => result?,
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
        let parent = self.path.parent().ok_or("Invalid session path")?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create session directory: {e}"))?;
        self.session.save(&self.path)?;
        if self.host.json {
            println!(
                "{}",
                serde_json::json!({"type":"done","session":self.path,"messages":self.session.history.len()})
            );
        } else {
            println!();
            eprintln!("Session: {}", self.path.display());
        }
        Ok(())
    }
}

async fn execute(args: Args) -> Result<(), String> {
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
    } else {
        args.session.map(|path| cwd.join(path))
    };
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
    let path = if args.fork { None } else { selected }
        .unwrap_or_else(|| directory.join(format!("{}.json", session.id)));
    let context = if args.no_context {
        None
    } else {
        Some(ProjectContext::load(&cwd)?)
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
    let profile_directory = mio_coding_agent::profile::config_directory()?;
    let explicit_base = args.base_url.or_else(|| std::env::var("MIO_BASE_URL").ok());
    let selected_provider = args.provider.or_else(|| std::env::var("MIO_PROVIDER").ok());
    let profile = if selected_provider
        .as_deref()
        .is_some_and(|p| p != "antigravity")
        || (selected_provider.is_none() && explicit_base.is_some())
    {
        None
    } else {
        mio_coding_agent::profile::summary(&profile_directory)?
    };
    let provider = match selected_provider
        .as_deref()
        .unwrap_or(if profile.is_some() {
            "antigravity"
        } else {
            "custom"
        }) {
        "custom" => Provider::Custom,
        "deepseek" => Provider::Deepseek,
        "antigravity" => Provider::Antigravity,
        _ => return Err("CLI provider must be custom, deepseek, or antigravity".into()),
    };
    if provider == Provider::Antigravity && explicit_base.is_some() {
        return Err("Antigravity uses its account endpoint; remove --base-url/MIO_BASE_URL".into());
    }
    let antigravity = if provider == Provider::Antigravity {
        Some(mio_coding_agent::antigravity::Antigravity::from_profile(
            profile_directory,
        )?)
    } else {
        None
    };
    let config = Config {
        provider,
        model: args
            .model
            .or_else(|| std::env::var("MIO_MODEL").ok())
            .or_else(|| profile.map(|p| p.model))
            .ok_or("Set --model or MIO_MODEL")?,
        base_url: explicit_base.unwrap_or_else(|| "https://api.openai.com/v1".into()),
        api_key: std::env::var("MIO_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default(),
    };
    let options = RunOptions {
        instructions,
        max_turns: args.max_turns.unwrap_or(32),
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
        subagents,
        antigravity,
        host: Arc::new(CliHost {
            json: args.json,
            no_shell: args.no_shell,
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
        } else {
            Ok(prompt.to_owned())
        }
    };
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
        return Err("--json requires -p or piped input".into());
    }
    eprintln!(
        "Mio · {}\n/help for commands. Shell {}. Workspace: {}",
        env!("CARGO_PKG_VERSION"),
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
        match line.trim() {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => {
                println!("{HELP}");
                continue;
            }
            "/sessions" => {
                print_sessions(&directory, false)?;
                continue;
            }
            "/new" | "/fork" => {
                runner.session = if line.trim() == "/fork" {
                    runner.session.fork()
                } else {
                    Session::new(&cwd)?
                };
                runner.path = directory.join(format!("{}.json", runner.session.id));
                eprintln!("Selected session {}", runner.session.id);
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
    let json = arguments.iter().any(|arg| arg == "--json");
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
