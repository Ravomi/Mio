use mio_coding_agent::{
    Config, ProgramState, Provider, RunOptions, ToolFilter,
    compact::DEFAULT_AUTO_COMPACT_BYTES,
    context::{ContextOptions, ProjectContext},
    dedup::TaskKeyLedger,
    parse_model_spec,
    rpc::resolve_provider_endpoint,
    session::Session,
    subagent::SubagentManager,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, IsTerminal},
    path::Path,
    process::ExitCode,
    sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering},
};
use tokio::io::AsyncReadExt;

use journal::{Journal, RecoveryReport};
use mio_coding_agent::journal;

#[path = "mio_cli/approval.rs"]
mod approval;
#[path = "mio_cli/args.rs"]
mod args;
#[path = "mio_cli/json_output.rs"]
mod json_output;
#[path = "mio_cli/output_cleanup.rs"]
mod output_cleanup;
#[path = "mio_cli/rpc.rs"]
mod rpc;
#[path = "mio_cli/runner.rs"]
mod runner;
#[path = "mio_cli/terminal.rs"]
mod terminal;
#[path = "mio_cli/tui/mod.rs"]
mod tui;

use approval::{RpcApprovals, StatusReporter};
use args::{Args, OutputMode, parse, print_help};
use json_output::{JsonOutput, emit_or_print};
use rpc::{run_one_shot, run_rpc_loop};
use runner::{
    CliHost, Runner, open_checkpoints, open_journal, print_models, print_recovered_calls,
    print_recovered_ops, print_sessions, print_sessions_json, session_list,
};
use terminal::run_interactive;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserCliSettings {
    default_model: Option<String>,
    default_provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

fn read_user_cli_settings(directory: &Path) -> Result<UserCliSettings, String> {
    let path = directory.join("settings.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|error| format!("Invalid Mio settings at {}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UserCliSettings::default()),
        Err(error) => Err(format!(
            "Cannot read Mio settings at {}: {error}",
            path.display()
        )),
    }
}

/// DES-09 维护通道：列出或精确确认工作区未决副作用账本。
///
/// 它不是模型可自动调用的工具：必须由操作者在命令行显式给出精确
/// `(scope, call_index, snapshot)`。它只确认现场、绝不删除证据，也不改写 journal。
fn run_unresolved_effects_maintenance(
    args: &Args,
    cwd: &Path,
    output: &Option<JsonOutput>,
) -> Result<(), String> {
    use mio_coding_agent::unresolved_effect::UnresolvedEffectLedger;
    if let Some(raw) = args.ack_unresolved_effect.as_deref() {
        let request: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| format!("--ack-unresolved-effect requires a JSON object: {error}"))?;
        let scope = request
            .get("scope")
            .and_then(|value| value.as_str())
            .ok_or("--ack-unresolved-effect requires a string \"scope\"")?;
        let call_index = request
            .get("call_index")
            .and_then(|value| value.as_u64())
            .ok_or("--ack-unresolved-effect requires a positive integer \"call_index\"")?;
        let snapshot = request
            .get("snapshot")
            .and_then(|value| value.as_str())
            .ok_or("--ack-unresolved-effect requires a sha256 \"snapshot\"")?;
        let note = request.get("note").and_then(|value| value.as_str());
        let ledger = UnresolvedEffectLedger::persistent(cwd);
        let observation =
            runner::acknowledge_unresolved_exact(&ledger, scope, call_index, snapshot, note)?;
        let value = json!({
            "scope": observation.record.scope,
            "callIndex": observation.record.call_index,
            "state": observation.record.state.as_str(),
            "snapshot": observation.snapshot_key.as_str(),
            "note": observation.record.acknowledgement.as_ref().map(|ack| ack.note.clone()),
        });
        if args.json {
            emit_or_print(
                output,
                json!({"type": "unresolved_effect_acknowledged", "data": value}),
                false,
            )?;
        } else {
            println!(
                "Acknowledged {}#{} ({})",
                scope,
                call_index,
                observation.record.state.as_str()
            );
        }
        return Ok(());
    }
    let ledger = UnresolvedEffectLedger::read_only(cwd);
    let view = runner::unresolved_effects_view(&ledger);
    if args.json {
        emit_or_print(
            output,
            json!({"type": "unresolved_effects", "data": view}),
            false,
        )?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_else(|_| view.to_string())
        );
    }
    Ok(())
}

async fn execute(
    args: Args,
    reporter: Arc<StatusReporter>,
    output: Option<JsonOutput>,
) -> Result<(), String> {
    let execution_policy = args.execution_policy()?;
    if args.sandbox_status {
        let status = runner::execution_status(&execution_policy);
        if args.json {
            emit_or_print(
                &output,
                json!({"type":"backend_status", "data":status}),
                false,
            )?;
        } else {
            println!("Backend: {}", execution_policy.name());
            println!("{}", status);
        }
        return Ok(());
    }
    if args.list_models {
        print_models(args.list_models_query.as_deref(), &output);
        return Ok(());
    }
    if args.plugin_search {
        let entries =
            mio_coding_agent::plugin::search_community_plugins(args.plugin_search_query.as_deref());
        if args.json {
            for e in entries {
                emit_or_print(
                    &output,
                    json!({"type": "community_plugin", "data": e}),
                    false,
                )?;
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
            emit_or_print(
                &output,
                json!({"type": "plugin_initialized", "name": name, "path": created}),
                false,
            )?;
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
                emit_or_print(
                    &output,
                    json!({"type": "registry_valid", "plugins": reg.plugins.len(), "path": val_path}),
                    false,
                )?;
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
                emit_or_print(
                    &output,
                    json!({"type": "plugin_valid", "data": manifest, "path": val_path}),
                    false,
                )?;
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
            emit_or_print(
                &output,
                serde_json::json!({"type":"profile_imported","provider":"antigravity","data":result}),
                false,
            )?;
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
        .clone()
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?)
        .canonicalize()
        .map_err(|e| format!("Invalid workspace: {e}"))?;
    if !cwd.is_dir() {
        return Err("Workspace must be a directory".into());
    }
    // DES-17 显式离线输出清理：`--output-cleanup-plan` / `--output-cleanup-confirm`
    // 是无 Provider、无模型的独立命令，必须在任何 Config/Session/scheduler 初始化之前
    // 返回；dry-run 永不删除，只有显式 confirm 才会删除。
    if output_cleanup::selected(&args) {
        return output_cleanup::run(&args, &cwd, &output);
    }
    // DES-09 维护通道：精确列出/确认工作区未决副作用账本。它是无 Provider、无模型的
    // 独立命令（不是模型可自动调用的工具），用于当运行内闸门溢出、无法普通确认时，
    // 让操作者按精确 key+snapshot 逐一确认，不删除任何证据。
    if args.list_unresolved_effects || args.ack_unresolved_effect.is_some() {
        return run_unresolved_effects_maintenance(&args, &cwd, &output);
    }
    // DES-17：离线清理命令已在上面提前返回，因此它永远不会被自己的租约挡住。其余
    // CLI/RPC/交互生命周期（包括 idle ephemeral 主机）都持有本工作区的 shared
    // activity 租约，直到 `execute` 返回，从而在另一个进程里跑离线 cleanup 时正确地
    // 报 `Busy`，而不是删掉本次运行还可能引用的旧输出。锁文件位于项目之外。
    let _runtime_activity_lease =
        match mio_coding_agent::shell::workspace_activity::shared_for_workspace(&cwd) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                return Err(
                    "an offline output cleanup holds this workspace; retry after it finishes"
                        .into(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "workspace output activity lease is unavailable: {error}"
                ));
            }
        };
    if let Some(ref spec) = args.plugin_install {
        let installed = mio_coding_agent::plugin::install_plugin(&cwd, spec)?;
        if args.json {
            emit_or_print(
                &output,
                json!({"type": "plugin_installed", "data": installed.manifest, "path": installed.root_dir}),
                false,
            )?;
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
            emit_or_print(
                &output,
                json!({"type": "plugin_removed", "name": name}),
                false,
            )?;
        } else {
            println!("Removed plugin '{name}'");
        }
        return Ok(());
    }
    if args.plugin_list {
        let installed = mio_coding_agent::plugin::discover_plugins(&cwd)?;
        if args.json {
            for p in installed {
                emit_or_print(
                    &output,
                    json!({"type": "plugin", "data": p.manifest, "path": p.root_dir}),
                    false,
                )?;
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
    if args.list_checkpoints || args.rewind.is_some() {
        let store = Arc::new(mio_coding_agent::checkpoint::CheckpointStore::open(&cwd)?);
        if args.list_checkpoints {
            let entries = store.list()?;
            if args.json {
                emit_or_print(
                    &output,
                    json!({"type":"checkpoints", "data":entries}),
                    false,
                )?;
            } else {
                for entry in entries {
                    println!(
                        "{} {:?} {} {}",
                        entry.id, entry.state, entry.operation, entry.target
                    );
                }
            }
            return Ok(());
        }
        let host = Arc::new(CliHost {
            json: args.json,
            output: output.clone(),
            no_shell: true,
            execution_policy: execution_policy.clone(),
            approve: args.approve,
            trust_project_mcp: false,
            approved_project_mcp: Arc::new(Mutex::new(BTreeSet::new())),
            reporter: reporter.clone(),
            steering_queue: Mutex::new(VecDeque::new()),
            rpc_approvals: None,
            journal: Arc::new(Journal::disabled()),
            ledger: Arc::new(
                mio_coding_agent::unresolved_effect::UnresolvedEffectLedger::disabled(),
            ),
            workspace: cwd.clone(),
            unknown_fence: Mutex::new(mio_coding_agent::unknown_effect::UnknownEffectFence::new()),
            turn_claims: Mutex::new(Vec::new()),
            dispatched_calls: Mutex::new(BTreeMap::new()),
            run_states: Mutex::new(BTreeMap::new()),
            run_tracking_saturated: AtomicBool::new(false),
            // rewind 是离线文件恢复，不会发起任何模型请求。
            output_schema: None,
        });
        let shared = if args.no_shared_limits {
            None
        } else {
            let root = match &args.shared_limits_dir {
                Some(path) if path.is_absolute() => path.clone(),
                Some(path) => cwd.join(path),
                None => mio_coding_agent::profile::config_directory()?.join("coordination"),
            };
            Some(
                mio_coding_agent::shared_limits::SharedLimits::new(root, Default::default())
                    .map_err(|error| error.to_string())?,
            )
        };
        let filter = ToolFilter::from_cli(
            args.tools.as_deref(),
            args.exclude_tools.as_deref(),
            args.no_tools,
        )?;
        let outcome = runner::rewind_file(
            &cwd,
            store,
            host,
            filter,
            shared,
            None,
            args.rewind.as_deref().expect("rewind selected"),
        )
        .await?;
        if args.json {
            emit_or_print(&output, json!({"type":"rewound", "data":outcome}), false)?;
        } else {
            println!(
                "Rewind {}: {:?} {}",
                outcome.id, outcome.action, outcome.target
            );
        }
        return Ok(());
    }
    let directory = cwd.join(".mio/sessions");
    if args.list {
        return if args.json {
            print_sessions_json(&directory, &output)
        } else {
            print_sessions(&directory, false)
        };
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
    if args.recovery {
        if args.no_session {
            return Err(
                "--recovery is unavailable with --no-session because no evidence is persisted"
                    .into(),
            );
        }
        let mut reports: Vec<(String, Option<RecoveryReport>)> = Vec::new();
        let mut recovery_cwd = cwd.clone();
        if let Some(path) = &selected {
            let session = Session::load(path)?;
            recovery_cwd = session.cwd.clone();
            let id = session.id.to_string();
            reports.push((
                id.clone(),
                Journal::open(&session.cwd, &id).recovery_report(),
            ));
        } else {
            // 默认遍历工作区全部 journal，覆盖会话文件从未保存的中断运行。
            let journal_dir = cwd.join(journal::JOURNAL_DIR);
            if journal_dir.is_dir() {
                for entry in std::fs::read_dir(&journal_dir).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let Some(stem) = entry
                        .path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_owned)
                    else {
                        continue;
                    };
                    let report = Journal::open(&cwd, &stem).recovery_report();
                    reports.push((stem, report));
                }
                reports.sort_by(|a, b| a.0.cmp(&b.0));
            } else if let Some(latest) = session_list(&directory)?.first() {
                let session = Session::load(&latest.file_path)?;
                let id = session.id.to_string();
                reports.push((
                    id.clone(),
                    Journal::open(&session.cwd, &id).recovery_report(),
                ));
            }
            if reports.is_empty() {
                return Err("No persisted execution evidence in this workspace".into());
            }
        }
        let saved_sessions =
            if selected.is_none() && directory.try_exists().map_err(|error| error.to_string())? {
                // Recovery must correlate older custom-named files too. The
                // ordinary listing's 1000-result display cap is not an identity
                // lookup bound; the directory scan itself remains bounded.
                let (sessions, warnings) = Session::list_with_limit(&directory, usize::MAX)?;
                for warning in warnings {
                    eprintln!("Warning: {}", warning.escape_debug());
                }
                sessions
            } else {
                Vec::new()
            };
        // DES-09: a read-only view of the cross-restart unresolved-effect
        // ledger. `read_only` creates nothing: no directory, no file, no sweep
        // and no migration, so `--recovery` never mutates persisted evidence.
        let unresolved_ledger =
            mio_coding_agent::unresolved_effect::UnresolvedEffectLedger::read_only(&recovery_cwd);
        let unresolved_view = runner::ledger_summary(&unresolved_ledger);
        for (session_id, report) in reports {
            let matching_paths: Vec<_> = saved_sessions
                .iter()
                .filter(|saved| saved.id.to_string() == session_id)
                .map(|saved| saved.file_path.clone())
                .collect();
            let ambiguous = selected.is_none() && matching_paths.len() > 1;
            let session_path = selected
                .clone()
                .or_else(|| matching_paths.first().cloned())
                .unwrap_or_else(|| directory.join(format!("{session_id}.json")));
            let mut view = runner::recovery_view(
                &session_id,
                &recovery_cwd,
                if ambiguous { None } else { Some(&session_path) },
                "data",
            );
            if ambiguous {
                view["sessionError"] =
                    json!("Multiple saved files have this session ID; select one with --session");
            }
            if args.json {
                emit_or_print(
                    &output,
                    json!({"type": "recovery", "session": session_id, "data": report, "view": view, "unresolvedEffects": unresolved_view.clone()}),
                    false,
                )?;
                continue;
            }
            println!("Session: {session_id}");
            if view["sessionSaved"] == true {
                println!("Saved messages: {}", view["savedMessages"]);
            } else {
                println!("No matching saved session history");
            }
            if let Some(error) = view["sessionError"].as_str() {
                eprintln!("Session warning: {}", error.escape_debug());
            }
            match report {
                Some(report) => {
                    if let Some(error) = &report.journal_error {
                        eprintln!("Journal warning: {error}");
                    }
                    println!("Interrupted runs: {}", report.interrupted_runs);
                    for goal in &report.unfinished_goals {
                        println!(
                            "Unfinished request (turn {}, {:?}{}): {}",
                            goal.turn,
                            goal.state,
                            if goal.truncated { ", truncated" } else { "" },
                            goal.goal
                                .as_deref()
                                .unwrap_or("[not recorded]")
                                .escape_debug(),
                        );
                    }
                    println!("Executed operations: {}", report.executed_operations);
                    print_recovered_ops("Executed", &report.executed);
                    print_recovered_ops("Unknown result", &report.unknown_operations);
                    print_recovered_ops("Failed", &report.failed_operations);
                    if !report.calls.is_empty() {
                        println!("Canonical tool calls: {}", report.calls.len());
                        print_recovered_calls(&report.calls);
                    }
                    if report.has_unfinished() {
                        println!(
                            "Mio will not replay these operations; file changes are not rolled back."
                        );
                    }
                }
                None => println!("No persisted execution evidence for this session"),
            }
        }
        if !args.json {
            println!(
                "Unresolved effects: {} unknown, {} outstanding, {} resolved trimmed",
                unresolved_view["unknownCount"],
                unresolved_view["outstandingCount"],
                unresolved_view["resolvedTrimmed"],
            );
            if let Some(error) = unresolved_view["error"].as_str() {
                eprintln!("Unresolved effect ledger warning: {}", error.escape_debug());
            }
        }
        return Ok(());
    }
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
            emit_or_print(
                &output,
                json!({"type": "exported", "session": session.id, "path": out_path}),
                false,
            )?;
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
    let tool_filter = if args.no_shell {
        tool_filter.without_shell()
    } else {
        tool_filter
    };
    let profile_directory = mio_coding_agent::profile::config_directory()?;
    let settings = read_user_cli_settings(&profile_directory)?;
    let raw_model_arg = args
        .model
        .or_else(|| settings.default_model.clone())
        .or_else(|| std::env::var("MIO_MODEL").ok());
    let (shorthand_provider, parsed_model, shorthand_thinking) = match &raw_model_arg {
        Some(raw) => {
            let (prov, mdl, thk) = parse_model_spec(raw);
            (prov, Some(mdl), thk)
        }
        None => (None, None, None),
    };
    let explicit_base = args
        .base_url
        .or_else(|| settings.base_url.clone())
        .or_else(|| std::env::var("MIO_BASE_URL").ok());
    let selected_provider = args
        .provider
        .or(shorthand_provider)
        .or_else(|| settings.default_provider.clone())
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
    let resolved_key = if settings.default_provider.as_deref() == Some(provider_str)
        && settings.base_url.as_deref() == Some(resolved_base.as_str())
    {
        settings.api_key.unwrap_or(resolved_key)
    } else {
        resolved_key
    };
    let antigravity = if provider == Provider::Antigravity {
        Some(mio_coding_agent::antigravity::Antigravity::from_profile(
            profile_directory.clone(),
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
    // `--output-schema PATH` 在创建会话、调度器与任何模型请求之前一次性读取。
    // 读盘/JSON/Schema 错误都在这里以明确错误返回，绝不降级为“无 schema”。
    let output_schema = match &args.output_schema {
        Some(path) => Some(runner::load_output_schema(&cwd, path)?),
        None => None,
    };
    let shared_directory = if args.no_shared_limits {
        None
    } else {
        Some(match &args.shared_limits_dir {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => cwd.join(path),
            None => profile_directory.join("coordination"),
        })
    };
    let scheduler = Arc::new(mio_coding_agent::scheduling::Scheduler::new(
        mio_coding_agent::coordinator::Limits {
            agent: args.max_subagents.unwrap_or(4),
            model: args.max_model_concurrency.unwrap_or(2),
            provider_model: args.max_provider_concurrency.unwrap_or(2),
            heavy: args.max_heavy_tasks.unwrap_or(2),
            max_model_calls: args.max_model_calls,
            max_total_tokens: args.max_total_tokens,
            ..Default::default()
        },
        shared_directory.as_deref(),
    )?);
    // REQ-RECOVERY：工作区级可选文件检查点。默认关闭，只有 MIO_CHECKPOINTS=1 或交互式
    // /checkpoints on 才把存储接到本次运行的 workspace 上。--no-session 与 journal 一致禁用。
    let checkpoints = open_checkpoints(args.no_session, &session.cwd)?;
    let options = RunOptions {
        pinned_context: session.pinned_context.clone(),
        scheduler: Some(scheduler),
        instructions,
        checkpoints: args.checkpoints.then(|| checkpoints.clone()),
        context_window: args.context_window,
        max_output_tokens: args.max_output_tokens,
        temperature: args.temperature,
        catalog_provider: Some(provider_str.to_owned()),
        max_turns: args.max_turns.unwrap_or(32),
        tool_concurrency: args.tool_concurrency.unwrap_or(1),
        thinking: args.thinking.or(shorthand_thinking),
        auto_compact_bytes: (!args.no_auto_compact).then_some(DEFAULT_AUTO_COMPACT_BYTES),
        task: None,
    };
    mio_coding_agent::validate_model_thinking(&config, options.thinking)?;
    mio_coding_agent::context_budget::ContextBudget::resolve(&config, &options)?;
    let subagents = if args.subagents {
        let manager = if let Some(account) = &antigravity {
            SubagentManager::new_with_antigravity(
                &cwd,
                config.clone(),
                options.clone(),
                account.clone(),
            )?
        } else {
            SubagentManager::new(&cwd, config.clone(), options.clone())?
        };
        Some(Arc::new(
            manager.with_concurrency(args.max_subagents.unwrap_or(4))?,
        ))
    } else {
        None
    };
    let provider_name = if provider_str == "gemini" {
        "google"
    } else {
        provider_str
    }
    .to_owned();
    let rpc_approvals = if args.mode == OutputMode::Rpc && args.approve {
        Some(Arc::new(RpcApprovals::with_output(
            std::time::Duration::from_secs(120),
            output.clone(),
        )))
    } else {
        None
    };
    // AUD-13: no-session 模式下 journal 完全禁用，不产生任何磁盘写入。
    let journal = open_journal(args.no_session, &session.cwd, &session.id.to_string());
    // 显式任务 key 账本：--no-session 时仅进程内（零磁盘写入），否则按工作区持久化。
    let dedup = Arc::new(if args.no_session {
        TaskKeyLedger::ephemeral()
    } else {
        TaskKeyLedger::persistent(&session.cwd)
    });
    let mut runner = Runner {
        connections: BTreeMap::from([(provider_name.clone(), config.clone())]),
        provider_name,
        config,
        options,
        tool_filter,
        no_mcp: args.no_mcp,
        no_session: args.no_session,
        subagents,
        antigravity,
        host: Arc::new(CliHost {
            json: args.json,
            output: output.clone(),
            no_shell: args.no_shell,
            execution_policy,
            approve: args.approve,
            trust_project_mcp: args.trust_project_mcp,
            approved_project_mcp: Arc::new(Mutex::new(BTreeSet::new())),
            reporter: reporter.clone(),
            steering_queue: Mutex::new(VecDeque::new()),
            rpc_approvals,
            journal: journal.clone(),
            ledger: runner::open_ledger(args.no_session, &session.cwd),
            workspace: session.cwd.clone(),
            unknown_fence: Mutex::new(mio_coding_agent::unknown_effect::UnknownEffectFence::new()),
            turn_claims: Mutex::new(Vec::new()),
            dispatched_calls: Mutex::new(BTreeMap::new()),
            run_states: Mutex::new(BTreeMap::new()),
            run_tracking_saturated: AtomicBool::new(false),
            output_schema: output_schema.clone(),
        }),
        session,
        path,
        journal,
        checkpoints,
        dedup,
    };
    // 关联 ID 只在真实存在时附上：会话 ID 由运行器维护并同步到输出封套。
    if let Some(output) = &output {
        output.set_session_id(Some(runner.session.id.to_string()));
    }
    // 恢复时的未完成运行提示 / RPC 事件。不重放操作，也不声称文件已回滚。
    if let Some(report) = runner.journal.recovery_report()
        && (report.has_unfinished() || report.journal_error.is_some())
    {
        if args.json {
            emit_or_print(&output, json!({"type": "recovery", "data": report}), false)?;
        } else {
            if let Some(error) = &report.journal_error {
                eprintln!("Warning: {error}");
            }
            if report.has_unfinished() {
                eprintln!("Warning: {}", report.summary_line());
            }
        }
    }
    // 启用检查点时，索引损坏必须在启动时可见（fail-closed，后续写入会被拒绝）。
    if runner.checkpoints_enabled()
        && let Err(error) = runner.checkpoints.list()
    {
        eprintln!("Warning: {error}");
    }
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
        let rendered = render(&prompt)?;
        return run_one_shot(&mut runner, &rendered, args.dedup_key.as_deref()).await;
    }
    if !io::stdin().is_terminal() {
        let mut prompt = String::new();
        tokio::io::stdin()
            .take(65537)
            .read_to_string(&mut prompt)
            .await
            .map_err(|e| e.to_string())?;
        let rendered = render(&prompt)?;
        return run_one_shot(&mut runner, &rendered, args.dedup_key.as_deref()).await;
    }
    if args.dedup_key.is_some() {
        return Err("--dedup-key requires -p or piped input".into());
    }
    if args.json {
        return Err("--json requires -p, piped input, or --mode rpc".into());
    }
    run_interactive(&mut runner, &directory, &cwd, render).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        print_help();
        return ExitCode::SUCCESS;
    }
    if arguments
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("Mio {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let (outcome, reporter, output, json) = match parse(arguments.iter().cloned()) {
        Ok(args) => {
            let json = args.json;
            let output = json.then(JsonOutput::new);
            let reporter_enabled = args.mode == OutputMode::Text && io::stderr().is_terminal();
            let reporter = Arc::new(StatusReporter::new(reporter_enabled));
            (
                execute(args, reporter.clone(), output.clone()).await,
                reporter,
                output,
                json,
            )
        }
        Err(error) => {
            let json = arguments
                .iter()
                .any(|arg| arg == "--json" || arg == "json" || arg == "rpc");
            let output = json.then(JsonOutput::new);
            let reporter_enabled = !json && io::stderr().is_terminal();
            let reporter = Arc::new(StatusReporter::new(reporter_enabled));
            (Err(error), reporter, output, json)
        }
    };
    mio_coding_agent::mcp::shutdown_mcp_connections();
    let error = match outcome {
        Ok(()) => None,
        Err(error) => {
            if !reporter.cancelled.load(Ordering::SeqCst) {
                reporter.report(ProgramState::Error, Some(&error));
            }
            Some(error)
        }
    };
    let mut failed = error.is_some();
    if let Some(error) = &error {
        // 传输已 fail-closed 时不再写 stdout，只允许短 stderr 诊断。
        match &output {
            Some(output) if json && !output.is_failed() => {
                if let Err(transport) =
                    output.emit_critical(serde_json::json!({"type":"error", "error": error}))
                {
                    eprintln!("Mio: JSON output transport failed: {transport}");
                    failed = true;
                }
            }
            _ => eprintln!("Mio: {error}"),
        }
    }
    if let Some(output) = &output {
        let status = output.shutdown(json_output::EXIT_FLUSH);
        if let Some(failure) = status.failure {
            eprintln!(
                "Mio: stdout output failed: {failure}; dropped streaming events: {}",
                status.dropped
            );
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
