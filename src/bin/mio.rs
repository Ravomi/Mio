use mio_coding_agent::{
    AgentEvent, BlockedKind, Config, Host, HostFuture, ProgramState, Provider, RunOptions,
    RunTools, Services, ThinkingLevel, ToolFilter,
    compact::{DEFAULT_AUTO_COMPACT_BYTES, DEFAULT_KEEP_RECENT_MESSAGES, compact_history},
    context::{ContextOptions, ProjectContext, expand_at_files},
    format_osc7501,
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
/thinking, /plugins, /plugin, /subagents, /status, /clear, /exit.
Tab completes commands, models, plugins, and @file paths; Up/Down browses history; Ctrl-C exits.
Shell is noninteractive and runs with your user permissions, not in a sandbox.
";

const INTERACTIVE_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "显示帮助与命令说明"),
    ("/status", "查看当前会话、模型与消息统计"),
    (
        "/model ",
        "查看或切换模型 (如 /model antigravity/claude-sonnet-4-5)",
    ),
    (
        "/thinking ",
        "设置思考深度 (off, minimal, low, medium, high, xhigh, max)",
    ),
    ("/new", "创建新会话"),
    ("/fork", "从当前会话分叉新分支"),
    ("/sessions", "列出当前工作区已保存的会话"),
    ("/name ", "设置或查看当前会话名称"),
    ("/compact", "归纳压缩早期对话历史"),
    ("/export ", "导出当前会话为独立 HTML 报告"),
    ("/plugins", "列出当前工作区已安装的插件"),
    (
        "/plugin ",
        "管理插件 (list, search, install, remove, init, validate)",
    ),
    ("/subagents", "查看当前会话中的子代理运行状态"),
    ("/clear", "清空终端屏幕"),
    ("/exit", "退出 Mio"),
    ("/quit", "退出 Mio"),
];

const INTERACTIVE_THINKING_LEVELS: &[(&str, &str)] = &[
    ("off", "关闭扩展思考"),
    ("minimal", "极简思考预算"),
    ("low", "低思考深度"),
    ("medium", "中等思考深度"),
    ("high", "高思考深度"),
    ("xhigh", "超高思考深度"),
    ("max", "最大思考深度"),
];

const INTERACTIVE_PLUGIN_SUBCOMMANDS: &[(&str, &str)] = &[
    ("list", "列出当前工作区已安装插件"),
    ("search ", "搜索官方社区插件注册表"),
    ("install ", "安装社区插件、GitHub 仓库或本地插件"),
    ("remove ", "卸载当前工作区中的已安装插件"),
    ("init ", "在指定目录初始化新插件脚手架"),
    ("validate ", "校验插件目录或注册表格式"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionItem {
    replacement: String,
    display: String,
    description: String,
}

fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20
        || (0x7f..0xa0).contains(&u)
        || (0x0300..=0x036f).contains(&u)
        || (0x1ab0..=0x1aff).contains(&u)
        || (0x1dc0..=0x1dff).contains(&u)
        || (0x20d0..=0x20ff).contains(&u)
        || (0xfe20..=0xfe2f).contains(&u)
    {
        0
    } else if (0x1100..=0x115f).contains(&u)
        || u == 0x2329
        || u == 0x232a
        || (0x2e80..=0x303e).contains(&u)
        || (0x3040..=0xa4cf).contains(&u)
        || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe10..=0xfe19).contains(&u)
        || (0xfe30..=0xfe6f).contains(&u)
        || (0xff00..=0xff60).contains(&u)
        || (0xffe0..=0xffe6).contains(&u)
        || (0x1f300..=0x1faff).contains(&u)
        || (0x20000..=0x3fffd).contains(&u)
    {
        2
    } else {
        1
    }
}

fn str_display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut out = String::new();
    for ch in s.chars() {
        let w = char_display_width(ch);
        if width + w > max_width {
            break;
        }
        width += w;
        out.push(ch);
    }
    out
}

#[cfg(unix)]
fn terminal_columns() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 20 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

#[cfg(not(unix))]
fn terminal_columns() -> usize {
    80
}

fn workspace_path_candidates(cwd: &Path, partial: &str) -> Vec<CompletionItem> {
    let (dir_rel, file_prefix) = match partial.rfind('/') {
        Some(idx) => (&partial[..=idx], &partial[idx + 1..]),
        None => ("", partial),
    };
    let search_dir = if dir_rel.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(dir_rel)
    };
    let Ok(entries) = std::fs::read_dir(search_dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }
        if matches!(name.as_str(), "target" | "node_modules" | ".git")
            && !file_prefix.starts_with(&name)
        {
            continue;
        }
        if name.starts_with(file_prefix) {
            let is_dir = entry.file_type().ok().is_some_and(|ft| ft.is_dir());
            if is_dir {
                let rel = format!("{dir_rel}{name}/");
                dirs.push(CompletionItem {
                    display: rel.clone(),
                    replacement: rel,
                    description: "目录".to_owned(),
                });
            } else {
                let rel = format!("{dir_rel}{name}");
                files.push(CompletionItem {
                    display: rel.clone(),
                    replacement: rel,
                    description: "文件".to_owned(),
                });
            }
        }
    }
    dirs.sort_by(|a, b| a.replacement.cmp(&b.replacement));
    files.sort_by(|a, b| a.replacement.cmp(&b.replacement));
    dirs.extend(files);
    dirs
}

fn completion_candidates(prefix_line: &str, cwd: &Path) -> (usize, Vec<CompletionItem>) {
    // 1. Slash command argument completion
    if let Some(rest) = prefix_line.strip_prefix("/thinking ")
        && !rest.contains(' ')
    {
        let start = "/thinking ".len();
        let list = INTERACTIVE_THINKING_LEVELS
            .iter()
            .filter(|(lvl, _)| lvl.starts_with(rest))
            .map(|(lvl, desc)| CompletionItem {
                replacement: (*lvl).to_owned(),
                display: (*lvl).to_owned(),
                description: (*desc).to_owned(),
            })
            .collect();
        return (start, list);
    }
    if let Some(rest) = prefix_line.strip_prefix("/model ")
        && !rest.contains(' ')
    {
        let start = "/model ".len();
        let catalog = list_catalog_models(None);
        let mut ready_matches = Vec::new();
        let mut other_matches = Vec::new();
        for entry in catalog {
            let full = format!("{}/{}", entry.provider, entry.model);
            let matched = full.starts_with(rest) || entry.model.starts_with(rest);
            if matched {
                let item = CompletionItem {
                    replacement: full.clone(),
                    display: full,
                    description: format!(
                        "[{}] {} · {}",
                        if entry.configured { "ready" } else { "unset" },
                        if entry.supports_thinking {
                            "thinking"
                        } else {
                            "standard"
                        },
                        entry.env_var
                    ),
                };
                if entry.configured {
                    ready_matches.push(item);
                } else {
                    other_matches.push(item);
                }
            }
        }
        ready_matches.extend(other_matches);
        return (start, ready_matches);
    }
    if let Some(rest) = prefix_line.strip_prefix("/plugin ") {
        if !rest.contains(' ') {
            let start = "/plugin ".len();
            let list = INTERACTIVE_PLUGIN_SUBCOMMANDS
                .iter()
                .filter(|(sub, _)| sub.starts_with(rest))
                .map(|(sub, desc)| CompletionItem {
                    replacement: (*sub).to_owned(),
                    display: sub.trim().to_owned(),
                    description: (*desc).to_owned(),
                })
                .collect();
            return (start, list);
        }
        if let Some(arg) = rest.strip_prefix("install ")
            && !arg.contains(' ')
        {
            let start = "/plugin install ".len();
            if arg.starts_with('.') || arg.starts_with('/') {
                return (start, workspace_path_candidates(cwd, arg));
            }
            let mut list: Vec<CompletionItem> =
                mio_coding_agent::plugin::search_community_plugins(None)
                    .into_iter()
                    .filter(|p| p.name.starts_with(arg))
                    .map(|p| CompletionItem {
                        replacement: p.name.clone(),
                        display: p.name,
                        description: format!("v{} · {}", p.version, p.description),
                    })
                    .collect();
            list.extend(workspace_path_candidates(cwd, arg));
            return (start, list);
        }
        if let Some(arg) = rest.strip_prefix("remove ")
            && !arg.contains(' ')
        {
            let start = "/plugin remove ".len();
            let list = mio_coding_agent::plugin::discover_plugins(cwd)
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.manifest.name.starts_with(arg))
                .map(|p| CompletionItem {
                    replacement: p.manifest.name.clone(),
                    display: p.manifest.name,
                    description: format!("v{} · {}", p.manifest.version, p.manifest.description),
                })
                .collect();
            return (start, list);
        }
        for sub in ["init ", "validate "] {
            if let Some(arg) = rest.strip_prefix(sub)
                && !arg.contains(' ')
            {
                let start = "/plugin ".len() + sub.len();
                return (start, workspace_path_candidates(cwd, arg));
            }
        }
    }
    if let Some(rest) = prefix_line.strip_prefix("/export ")
        && !rest.contains(' ')
    {
        let start = "/export ".len();
        return (start, workspace_path_candidates(cwd, rest));
    }

    // 2. Slash command name completion
    if prefix_line.starts_with('/') && !prefix_line.contains(char::is_whitespace) {
        let list = INTERACTIVE_SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| {
                cmd.starts_with(prefix_line) || cmd.trim_end().starts_with(prefix_line)
            })
            .map(|(cmd, desc)| CompletionItem {
                replacement: (*cmd).to_owned(),
                display: cmd.trim_end().to_owned(),
                description: (*desc).to_owned(),
            })
            .collect();
        return (0, list);
    }

    // 3. @file workspace path completion on current token
    let token_start = prefix_line
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &prefix_line[token_start..];
    if let Some(partial) = token.strip_prefix('@')
        && !partial.contains(':')
    {
        let start = token_start + 1;
        return (
            start,
            workspace_path_candidates(cwd, partial)
                .into_iter()
                .map(|mut item| {
                    item.display = format!("@{}", item.display);
                    item
                })
                .collect(),
        );
    }
    if token.starts_with("./") || token.starts_with("../") || token.contains('/') {
        return (token_start, workspace_path_candidates(cwd, token));
    }

    (prefix_line.len(), Vec::new())
}

fn compute_ghost_suggestion(
    buf: &str,
    cursor: usize,
    history: &[String],
    cwd: &Path,
) -> Option<String> {
    if cursor != buf.len() || buf.is_empty() {
        return None;
    }
    let (start, candidates) = completion_candidates(buf, cwd);
    let current_part = &buf[start..];
    if !current_part.is_empty() {
        for cand in candidates {
            if cand.replacement.starts_with(current_part)
                && cand.replacement.len() > current_part.len()
            {
                return Some(cand.replacement[current_part.len()..].to_owned());
            }
        }
    }
    for item in history.iter().rev() {
        if item.starts_with(buf) && item.len() > buf.len() {
            return Some(item[buf.len()..].to_owned());
        }
    }
    None
}

enum ReadOutcome {
    Line(String),
    Interrupted,
    Exit,
}

#[derive(Default)]
enum EscState {
    #[default]
    None,
    Esc,
    Csi(Vec<u8>),
}

struct TabCycleState {
    replace_start: usize,
    candidates: Vec<CompletionItem>,
    index: usize,
}

struct InteractiveEditor {
    buf: Vec<u8>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    saved_line: String,
    ghost_dismissed: bool,
    had_ghost: bool,
    esc: EscState,
    tab_cycle: Option<TabCycleState>,
}

#[cfg(unix)]
struct RawTerminalGuard {
    fd: libc::c_int,
    original: Option<libc::termios>,
}

#[cfg(unix)]
impl RawTerminalGuard {
    fn acquire() -> Self {
        let fd = libc::STDIN_FILENO;
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return Self { fd, original: None };
            }
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSADRAIN, &raw) != 0 {
                return Self { fd, original: None };
            }
            Self {
                fd,
                original: Some(orig),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.original.take() {
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSADRAIN, &orig);
            }
        }
    }
}

impl InteractiveEditor {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            saved_line: String::new(),
            ghost_dismissed: false,
            had_ghost: false,
            esc: EscState::None,
            tab_cycle: None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.buf).ok()
    }

    fn set_str(&mut self, s: &str) {
        self.buf.clear();
        self.buf.extend_from_slice(s.as_bytes());
        self.cursor = self.buf.len();
    }

    fn reset_line(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_index = None;
        self.ghost_dismissed = false;
        self.had_ghost = false;
        self.esc = EscState::None;
        self.tab_cycle = None;
    }

    fn push_history(&mut self, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().is_none_or(|last| last != trimmed) {
            self.history.push(trimmed.to_owned());
        }
    }

    fn prev_char_boundary(&self) -> usize {
        let mut idx = self.cursor.min(self.buf.len());
        while idx > 0 {
            idx -= 1;
            if (self.buf[idx] & 0xc0) != 0x80 {
                break;
            }
        }
        idx
    }

    fn next_char_boundary(&self) -> usize {
        let mut idx = (self.cursor + 1).min(self.buf.len());
        while idx < self.buf.len() && (self.buf[idx] & 0xc0) == 0x80 {
            idx += 1;
        }
        idx
    }

    fn current_ghost(&self, cwd: &Path) -> Option<String> {
        if self.ghost_dismissed {
            None
        } else {
            let s = self.as_str()?;
            compute_ghost_suggestion(s, self.cursor, &self.history, cwd)
        }
    }

    fn handle_tab(&mut self, cwd: &Path) {
        self.step_completion(cwd, true);
    }

    fn step_completion(&mut self, cwd: &Path, forward: bool) -> bool {
        if let Some(cycle) = &mut self.tab_cycle {
            if forward {
                cycle.index = (cycle.index + 1) % cycle.candidates.len();
            } else {
                cycle.index = (cycle.index + cycle.candidates.len() - 1) % cycle.candidates.len();
            }
            let chosen = cycle.candidates[cycle.index].replacement.clone();
            let start = cycle.replace_start.min(self.buf.len());
            self.buf.truncate(start);
            self.buf.extend_from_slice(chosen.as_bytes());
            self.cursor = self.buf.len();
            self.ghost_dismissed = true;
            return true;
        }
        let Some(s) = self.as_str().map(str::to_owned) else {
            return false;
        };
        let prefix = &s[..self.cursor.min(s.len())];
        let (replace_start, candidates) = completion_candidates(prefix, cwd);
        if !candidates.is_empty() {
            let idx = if forward { 0 } else { candidates.len() - 1 };
            let chosen = candidates[idx].replacement.clone();
            let tail = &s[self.cursor.min(s.len())..];
            let mut next = String::with_capacity(replace_start + chosen.len() + tail.len());
            next.push_str(&s[..replace_start]);
            next.push_str(&chosen);
            let new_cursor = next.len();
            next.push_str(tail);
            self.buf = next.into_bytes();
            self.cursor = new_cursor;
            if candidates.len() > 1 && tail.is_empty() {
                self.tab_cycle = Some(TabCycleState {
                    replace_start,
                    candidates,
                    index: idx,
                });
                self.ghost_dismissed = true;
            } else {
                self.ghost_dismissed = false;
            }
            return true;
        }
        if let Some(ghost) = self.current_ghost(cwd) {
            self.buf.extend_from_slice(ghost.as_bytes());
            self.cursor = self.buf.len();
            return true;
        }
        false
    }

    fn render(&mut self, cwd: &Path) -> io::Result<()> {
        let Some(s) = self.as_str().map(str::to_owned) else {
            return Ok(());
        };
        let ghost = self.current_ghost(cwd);
        let mut out = String::from("\r\x1b[2K\x1b[Jmio> ");
        out.push_str(&s);
        if let Some(ref g) = ghost {
            out.push_str("\x1b[38;2;130;180;220m");
            out.push_str(g);
            out.push_str("\x1b[0m");
            self.had_ghost = true;
        } else {
            self.had_ghost = false;
        }

        let live_candidates = if self.tab_cycle.is_none() && !self.ghost_dismissed && !s.is_empty()
        {
            let prefix = &s[..self.cursor.min(s.len())];
            let (_, list) = completion_candidates(prefix, cwd);
            list
        } else {
            Vec::new()
        };

        let menu_view: Option<(&[CompletionItem], usize)> = if let Some(cycle) = &self.tab_cycle {
            Some((&cycle.candidates, cycle.index))
        } else if !live_candidates.is_empty() {
            Some((&live_candidates, 0))
        } else {
            None
        };

        let mut menu_lines = 0usize;
        if let Some((items, selected_idx)) = menu_view {
            let cols = terminal_columns();
            let max_line_width = cols.saturating_sub(2);
            let total = items.len();
            let win_len = total.min(6);
            let win_start = if selected_idx >= win_len {
                selected_idx + 1 - win_len
            } else {
                0
            };
            let pad_col = items[win_start..win_start + win_len]
                .iter()
                .map(|it| str_display_width(&it.display))
                .max()
                .unwrap_or(10)
                .clamp(12, 34);

            for (offset, item) in items[win_start..win_start + win_len].iter().enumerate() {
                let idx = win_start + offset;
                let disp_w = str_display_width(&item.display);
                let pad_spaces = " ".repeat(pad_col.saturating_sub(disp_w) + 2);
                let prefix_w = 4 + disp_w + pad_spaces.len();
                let desc_budget = max_line_width.saturating_sub(prefix_w);
                let desc = truncate_to_width(&item.description, desc_budget);

                out.push_str("\r\n");
                if idx == selected_idx {
                    out.push_str("\x1b[1;38;2;149;205;247m  ▸ ");
                    out.push_str(&item.display);
                    out.push_str(&pad_spaces);
                    out.push_str("\x1b[0m\x1b[38;2;195;210;225m");
                    out.push_str(&desc);
                    out.push_str("\x1b[0m");
                } else {
                    out.push_str("\x1b[38;2;155;170;185m    ");
                    out.push_str(&item.display);
                    out.push_str(&pad_spaces);
                    out.push_str("\x1b[0m\x1b[38;2;115;130;145m");
                    out.push_str(&desc);
                    out.push_str("\x1b[0m");
                }
                menu_lines += 1;
            }
            if total > win_len {
                let footer = format!("    … ({}/{total} · Tab/↑↓ 切换补全)", selected_idx + 1);
                let footer_clipped = truncate_to_width(&footer, max_line_width);
                out.push_str("\r\n\x1b[38;2;115;130;145m");
                out.push_str(&footer_clipped);
                out.push_str("\x1b[0m");
                menu_lines += 1;
            }
            self.had_ghost = true;
        }

        if menu_lines > 0 {
            out.push_str(&format!("\x1b[{menu_lines}A"));
        }
        let cursor_col = 5 + str_display_width(&s[..self.cursor.min(s.len())]);
        out.push_str(&format!("\r\x1b[{cursor_col}C"));

        let mut stderr = io::stderr().lock();
        stderr.write_all(out.as_bytes())?;
        stderr.flush()
    }

    #[cfg(unix)]
    fn read_line(&mut self, cwd: &Path) -> Result<ReadOutcome, String> {
        self.reset_line();
        let guard = RawTerminalGuard::acquire();
        if guard.original.is_none() {
            drop(guard);
            eprint!("mio> ");
            io::stderr().flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            let n = io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(ReadOutcome::Exit);
            }
            return Ok(ReadOutcome::Line(line));
        }
        self.render(cwd).map_err(|e| e.to_string())?;
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    eprint!("^C\r\n");
                    let _ = io::stderr().flush();
                    return Ok(ReadOutcome::Exit);
                }
                return Err(err.to_string());
            }
            if n == 0 {
                eprint!("\r\n");
                let _ = io::stderr().flush();
                return Ok(ReadOutcome::Exit);
            }
            for &byte in &buf[..n as usize] {
                match &mut self.esc {
                    EscState::None => {
                        if byte == 0x1b {
                            self.esc = EscState::Esc;
                            continue;
                        }
                    }
                    EscState::Esc => {
                        if byte == b'[' || byte == b'O' {
                            self.esc = EscState::Csi(Vec::new());
                        } else {
                            self.esc = EscState::None;
                            self.tab_cycle = None;
                            self.ghost_dismissed = true;
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                        continue;
                    }
                    EscState::Csi(seq) => {
                        seq.push(byte);
                        if (0x40..=0x7e).contains(&byte) {
                            let params = seq.clone();
                            self.esc = EscState::None;
                            match params.as_slice() {
                                b"Z" => {
                                    // Shift+Tab: step completion backward
                                    self.step_completion(cwd, false);
                                    self.render(cwd).map_err(|e| e.to_string())?;
                                }
                                b"A" => {
                                    if self.tab_cycle.is_some() {
                                        self.step_completion(cwd, false);
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    } else if !self.history.is_empty() {
                                        self.tab_cycle = None;
                                        let next_idx = match self.history_index {
                                            None => {
                                                self.saved_line =
                                                    String::from_utf8_lossy(&self.buf).into_owned();
                                                self.history.len() - 1
                                            }
                                            Some(i) => i.saturating_sub(1),
                                        };
                                        self.history_index = Some(next_idx);
                                        let entry = self.history[next_idx].clone();
                                        self.set_str(&entry);
                                        self.ghost_dismissed = true;
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    }
                                }
                                b"B" => {
                                    if self.tab_cycle.is_some() {
                                        self.step_completion(cwd, true);
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    } else if let Some(i) = self.history_index {
                                        self.tab_cycle = None;
                                        let entry = if i + 1 < self.history.len() {
                                            self.history_index = Some(i + 1);
                                            self.history[i + 1].clone()
                                        } else {
                                            self.history_index = None;
                                            self.saved_line.clone()
                                        };
                                        self.set_str(&entry);
                                        self.ghost_dismissed = true;
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    } else if self.buf.starts_with(b"/") {
                                        self.step_completion(cwd, true);
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    }
                                }
                                b"C" => {
                                    self.tab_cycle = None;
                                    if self.cursor < self.buf.len() {
                                        self.cursor = self.next_char_boundary();
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    } else if let Some(ghost) = self.current_ghost(cwd) {
                                        self.buf.extend_from_slice(ghost.as_bytes());
                                        self.cursor = self.buf.len();
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    }
                                }
                                b"D" => {
                                    self.tab_cycle = None;
                                    if self.cursor > 0 {
                                        self.cursor = self.prev_char_boundary();
                                        self.render(cwd).map_err(|e| e.to_string())?;
                                    }
                                }
                                b"H" | b"1~" => {
                                    self.tab_cycle = None;
                                    self.cursor = 0;
                                    self.render(cwd).map_err(|e| e.to_string())?;
                                }
                                b"F" | b"4~" => {
                                    self.tab_cycle = None;
                                    self.cursor = self.buf.len();
                                    self.render(cwd).map_err(|e| e.to_string())?;
                                }
                                b"3~" if self.cursor < self.buf.len() => {
                                    self.tab_cycle = None;
                                    let next = self.next_char_boundary();
                                    self.buf.drain(self.cursor..next);
                                    self.render(cwd).map_err(|e| e.to_string())?;
                                }
                                _ => {
                                    self.tab_cycle = None;
                                }
                            }
                        }
                        continue;
                    }
                }

                if byte != b'\t' {
                    self.tab_cycle = None;
                }

                match byte {
                    3 => {
                        // Ctrl-C: clear completion menu/ghost and immediately exit interactive session
                        let current = String::from_utf8_lossy(&self.buf);
                        eprint!("\r\x1b[2K\x1b[Jmio> {current}^C\r\n");
                        let _ = io::stderr().flush();
                        return Ok(ReadOutcome::Interrupted);
                    }
                    4 => {
                        // Ctrl-D: exit when empty, otherwise delete char under cursor
                        if self.buf.is_empty() {
                            eprint!("\r\x1b[2K\x1b[J\r\n");
                            let _ = io::stderr().flush();
                            return Ok(ReadOutcome::Exit);
                        } else if self.cursor < self.buf.len() {
                            let next = self.next_char_boundary();
                            self.buf.drain(self.cursor..next);
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    1 => {
                        // Ctrl-A: Home
                        self.cursor = 0;
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    5 => {
                        // Ctrl-E: End or accept ghost
                        if self.cursor < self.buf.len() {
                            self.cursor = self.buf.len();
                        } else if let Some(ghost) = self.current_ghost(cwd) {
                            self.buf.extend_from_slice(ghost.as_bytes());
                            self.cursor = self.buf.len();
                        }
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    9 => {
                        // Tab: complete command, argument, or @file path (cycles on repeated Tab)
                        self.handle_tab(cwd);
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    11 => {
                        // Ctrl-K: delete from cursor to end of line
                        if self.cursor < self.buf.len() {
                            self.buf.truncate(self.cursor);
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    12 => {
                        // Ctrl-L: clear screen
                        eprint!("\x1b[2J\x1b[H");
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    21 => {
                        // Ctrl-U: delete from cursor to start of line
                        if self.cursor > 0 {
                            self.buf.drain(..self.cursor);
                            self.cursor = 0;
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    23 => {
                        // Ctrl-W: delete previous word
                        if self.cursor > 0 {
                            let mut i = self.cursor;
                            while i > 0 && self.buf[i - 1].is_ascii_whitespace() {
                                i -= 1;
                            }
                            while i > 0 && !self.buf[i - 1].is_ascii_whitespace() {
                                i -= 1;
                            }
                            self.buf.drain(i..self.cursor);
                            self.cursor = i;
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    8 | 127 => {
                        if self.cursor > 0 {
                            let prev = self.prev_char_boundary();
                            self.buf.drain(prev..self.cursor);
                            self.cursor = prev;
                            self.ghost_dismissed = false;
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    b'\r' | b'\n' => {
                        let mut submitted = String::from_utf8_lossy(&self.buf).into_owned();
                        let trimmed_sub = submitted.trim();
                        if trimmed_sub.starts_with('/')
                            && !trimmed_sub.contains(char::is_whitespace)
                        {
                            let (_, cands) = completion_candidates(trimmed_sub, cwd);
                            if cands.len() == 1 && !cands[0].replacement.ends_with(' ') {
                                submitted = cands[0].replacement.clone();
                            }
                        }
                        eprint!("\r\x1b[2K\x1b[Jmio> {submitted}\r\n");
                        let _ = io::stderr().flush();
                        self.push_history(&submitted);
                        return Ok(ReadOutcome::Line(submitted));
                    }
                    b if b >= 0x20 => {
                        self.buf.insert(self.cursor, b);
                        self.cursor += 1;
                        self.ghost_dismissed = false;
                        if self.as_str().is_some() {
                            self.render(cwd).map_err(|e| e.to_string())?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn read_line(&mut self, _cwd: &Path) -> Result<ReadOutcome, String> {
        eprint!("mio> ");
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        let n = io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(ReadOutcome::Exit);
        }
        Ok(ReadOutcome::Line(line))
    }
}

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

struct StatusReporter {
    enabled: bool,
    cancelled: AtomicBool,
    last_seq: Mutex<Option<String>>,
}

impl StatusReporter {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            cancelled: AtomicBool::new(false),
            last_seq: Mutex::new(None),
        }
    }

    fn report(&self, state: ProgramState, msg: Option<&str>) {
        if !self.enabled {
            return;
        }
        let seq = format_osc7501(&state, msg);
        let mut guard = self.last_seq.lock().unwrap();
        if guard.as_ref() == Some(&seq) {
            return;
        }
        *guard = Some(seq.clone());
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(seq.as_bytes());
        let _ = stderr.flush();
    }
}

struct CliHost {
    json: bool,
    no_shell: bool,
    approve: bool,
    reporter: Arc<StatusReporter>,
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
                    self.reporter
                        .report(ProgramState::Working, Some("Responding"));
                    print!("{text}");
                    if let Err(e) = io::stdout().flush() {
                        eprintln!("Output error: {e}");
                    }
                }
                AgentEvent::ToolStarted { operation, target } => {
                    let msg = format!("{operation} {target}");
                    self.reporter
                        .report(ProgramState::Working, Some(msg.trim()));
                    eprintln!("\n[{operation}] {target}")
                }
                AgentEvent::ToolFinished {
                    operation, result, ..
                } => {
                    self.reporter
                        .report(ProgramState::Working, Some("Waiting for model"));
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
        let msg = format!("{operation} {target}");
        self.reporter.report(
            ProgramState::Blocked {
                kind: BlockedKind::Permission,
            },
            Some(msg.trim()),
        );
        eprint!("\nApprove [{operation}] {target}? [y/N]: ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        let res = io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("Failed to read approval input: {e}"));
        self.reporter.report(ProgramState::Working, None);
        res?;
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
        self.host.reporter.cancelled.store(false, Ordering::SeqCst);
        self.host
            .reporter
            .report(ProgramState::Working, Some("Waiting for model"));
        let res = self.turn_inner(prompt).await;
        match res {
            Ok(cancelled) => {
                if cancelled {
                    self.host.reporter.cancelled.store(true, Ordering::SeqCst);
                    self.host.reporter.report(ProgramState::Idle, None);
                    Err(
                        "Cancelled; previous session retained. File changes are not rolled back."
                            .into(),
                    )
                } else {
                    self.host.reporter.report(ProgramState::Done, None);
                    Ok(())
                }
            }
            Err(e) => {
                self.host.reporter.report(ProgramState::Error, Some(&e));
                Err(e)
            }
        }
    }

    async fn turn_inner(&mut self, prompt: &str) -> Result<bool, String> {
        let expanded_prompt = expand_at_files(&self.session.cwd, prompt)?;
        let extra_tools = if self.no_mcp {
            Vec::new()
        } else {
            load_dynamic_tools(&self.session.cwd, self.host.clone(), &self.tool_filter).await?
        };
        let mut cancelled = false;
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
                cancelled = true;
                Vec::new()
            }
        };
        if cancelled {
            return Ok(true);
        }
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
        Ok(false)
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

async fn execute(args: Args, reporter: Arc<StatusReporter>) -> Result<(), String> {
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
            reporter: reporter.clone(),
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
        "Mio · {} · model: {}{}\n/help for commands · Tab completes · Ctrl-C exits. Shell {}. Workspace: {}",
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
    reporter.report(ProgramState::Idle, None);
    let mut editor = InteractiveEditor::new();
    loop {
        let line = match editor.read_line(&cwd)? {
            ReadOutcome::Line(line) => line,
            ReadOutcome::Interrupted => {
                reporter.report(ProgramState::Idle, None);
                break;
            }
            ReadOutcome::Exit => break,
        };
        reporter.cancelled.store(false, Ordering::SeqCst);
        reporter.report(ProgramState::Idle, None);
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
            _ => match render(&line) {
                Ok(rendered) => {
                    if let Err(err) = runner.turn(&rendered).await {
                        eprintln!("Mio: {err}");
                    }
                }
                Err(err) => eprintln!("Mio: {err}"),
            },
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
    let result = match parse(arguments.iter().cloned()) {
        Ok(args) => {
            let json = args.json;
            let reporter_enabled = args.mode == OutputMode::Text && io::stderr().is_terminal();
            let reporter = Arc::new(StatusReporter::new(reporter_enabled));
            (execute(args, reporter.clone()).await, reporter, json)
        }
        Err(error) => {
            let json = arguments
                .iter()
                .any(|arg| arg == "--json" || arg == "json" || arg == "rpc");
            let reporter_enabled = !json && io::stderr().is_terminal();
            let reporter = Arc::new(StatusReporter::new(reporter_enabled));
            (Err(error), reporter, json)
        }
    };
    match result {
        (Ok(()), _, _) => ExitCode::SUCCESS,
        (Err(error), reporter, json) => {
            if !reporter.cancelled.load(Ordering::SeqCst) {
                reporter.report(ProgramState::Error, Some(&error));
            }
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

    #[test]
    fn interactive_tab_completes_commands_plugins_and_at_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        // 1. Slash command completion
        let mut editor = InteractiveEditor::new();
        editor.set_str("/he");
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/help"));

        // 2. Tab cycling across multiple matching slash commands (/p -> /plugins -> /plugin )
        editor.reset_line();
        editor.set_str("/p");
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/plugins"));
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/plugin "));

        // 3. /plugin install community completion
        editor.reset_line();
        editor.set_str("/plugin install rust");
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/plugin install rust-expert"));

        // 4. @file path completion
        editor.reset_line();
        editor.set_str("check @REA");
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("check @README.md"));

        editor.reset_line();
        editor.set_str("check @sr");
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("check @src/"));
    }
}
