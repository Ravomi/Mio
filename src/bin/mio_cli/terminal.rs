//! 终端交互：行编辑、Tab 补全、上下键历史与 REPL 命令。
//!
//! 刻意保持极简：不进入 alternate screen，不清屏（除显式 `/clear`），
//! 只输出 `mio> ` 提示符与流式文本。

use mio_coding_agent::{
    ProgramState, ThinkingLevel, compact::DEFAULT_KEEP_RECENT_MESSAGES, rpc::list_catalog_models,
    session::Session,
};
use std::{
    io::{self, Write},
    path::Path,
    str::FromStr,
    sync::atomic::Ordering,
};

use super::runner::{Runner, print_sessions};

fn edit_pinned_context(
    current: &mio_coding_agent::context_memory::PinnedContext,
    command: &str,
) -> Result<mio_coding_agent::context_memory::PinnedContext, String> {
    use mio_coding_agent::context_memory::{PinnedContextEntry, PinnedContextKind};
    let mut candidate = current.clone();
    if let Some(id) = command.strip_prefix("/unpin ") {
        let id = id.trim();
        candidate.remove(id).ok_or("Pinned context ID not found")?;
        return Ok(candidate);
    }
    let mut rest = command
        .strip_prefix("/pin ")
        .ok_or("Expected /pin or /unpin")?
        .trim_start();
    let replace = rest.starts_with("replace ");
    if replace {
        rest = rest["replace ".len()..].trim_start();
    }
    let (kind, rest) = rest
        .split_once(char::is_whitespace)
        .ok_or("Usage: /pin [replace] constraint|goal ID TEXT")?;
    let kind = match kind {
        "constraint" => PinnedContextKind::Constraint,
        "goal" => PinnedContextKind::Goal,
        _ => return Err("Pinned kind must be constraint or goal".into()),
    };
    let (id, text) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .ok_or("Usage: /pin [replace] constraint|goal ID TEXT")?;
    let entry = PinnedContextEntry::new(id, kind, text.trim_start())?;
    if replace {
        candidate.replace(entry)?;
    } else {
        candidate.register(entry)?;
    }
    Ok(candidate)
}

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
    (
        "/pin ",
        "登记约束或目标：/pin [replace] constraint|goal ID TEXT",
    ),
    ("/unpin ", "解除已登记条目：/unpin ID"),
    ("/pins", "查看宿主显式登记的约束与目标"),
    ("/subagents", "查看当前会话中的子代理运行状态"),
    (
        "/checkpoints ",
        "列出文件检查点，或 /checkpoints on|off 开关 pre-edit 快照",
    ),
    (
        "/rewind ",
        "将某个检查点覆盖的文件回退到修改前（拒绝覆盖后续手工修改）",
    ),
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

/// 终端显示宽度：宽字符按 2 列计算，零宽组合字符不占列。
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

fn workspace_path_candidates(cwd: &Path, partial: &str) -> Vec<String> {
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
                dirs.push(format!("{dir_rel}{name}/"));
            } else {
                files.push(format!("{dir_rel}{name}"));
            }
        }
    }
    dirs.sort();
    files.sort();
    dirs.extend(files);
    dirs
}

fn completion_candidates(prefix_line: &str, cwd: &Path) -> (usize, Vec<String>) {
    // 1. Slash command argument completion
    if let Some(rest) = prefix_line.strip_prefix("/thinking ")
        && !rest.contains(' ')
    {
        let start = "/thinking ".len();
        let list = INTERACTIVE_THINKING_LEVELS
            .iter()
            .filter(|(lvl, _)| lvl.starts_with(rest))
            .map(|(lvl, _)| (*lvl).to_owned())
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
                if entry.configured {
                    ready_matches.push(full);
                } else {
                    other_matches.push(full);
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
                .map(|(sub, _)| (*sub).to_owned())
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
            let mut list: Vec<String> = mio_coding_agent::plugin::search_community_plugins(None)
                .into_iter()
                .filter(|p| p.name.starts_with(arg))
                .map(|p| p.name)
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
                .map(|p| p.manifest.name)
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
            .map(|(cmd, _)| (*cmd).to_owned())
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
        // 候选文本已是含 `@` 的完整 token，因此从 `@` 本身开始替换，
        // 否则会在缓冲区留下重复的 `@@`。
        let start = token_start;
        return (
            start,
            workspace_path_candidates(cwd, partial)
                .into_iter()
                .map(|item| format!("@{item}"))
                .collect(),
        );
    }
    if token.starts_with("./") || token.starts_with("../") || token.contains('/') {
        return (token_start, workspace_path_candidates(cwd, token));
    }

    (prefix_line.len(), Vec::new())
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
    candidates: Vec<String>,
    index: usize,
}

struct InteractiveEditor {
    buf: Vec<u8>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    saved_line: String,
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
            let chosen = cycle.candidates[cycle.index].clone();
            let start = cycle.replace_start.min(self.buf.len());
            self.buf.truncate(start);
            self.buf.extend_from_slice(chosen.as_bytes());
            self.cursor = self.buf.len();
            return true;
        }
        let Some(s) = self.as_str().map(str::to_owned) else {
            return false;
        };
        let prefix = &s[..self.cursor.min(s.len())];
        let (replace_start, candidates) = completion_candidates(prefix, cwd);
        if !candidates.is_empty() {
            let idx = if forward { 0 } else { candidates.len() - 1 };
            let chosen = candidates[idx].clone();
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
            }
            return true;
        }
        false
    }

    /// 单行重绘：`\r` + 清当前行 + `mio> ` + 缓冲区，光标按显示宽度回退。
    ///
    /// 不画多行补全菜单、不输出颜色、不进入 alternate screen，也不写滚动历史。
    fn render(&mut self, _cwd: &Path) -> io::Result<()> {
        let Some(s) = self.as_str().map(str::to_owned) else {
            return Ok(());
        };
        let mut stderr = io::stderr().lock();
        stderr.write_all(b"\r\x1b[2Kmio> ")?;
        stderr.write_all(s.as_bytes())?;
        let back = str_display_width(&s[self.cursor.min(s.len())..]);
        if back > 0 {
            write!(stderr, "\x1b[{back}D")?;
        }
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
                        // Ctrl-C: echo the line and exit the interactive session
                        let current = String::from_utf8_lossy(&self.buf);
                        eprint!("\r\x1b[2Kmio> {current}^C\r\n");
                        let _ = io::stderr().flush();
                        return Ok(ReadOutcome::Interrupted);
                    }
                    4 => {
                        // Ctrl-D: exit when empty, otherwise delete char under cursor
                        if self.buf.is_empty() {
                            eprint!("\r\x1b[2K\r\n");
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
                        // Ctrl-E: move to end of line
                        self.cursor = self.buf.len();
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    9 => {
                        // Tab: complete command, argument, or @file path (cycles on repeated Tab)
                        self.handle_tab(cwd);
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    11 if self.cursor < self.buf.len() => {
                        // Ctrl-K: delete from cursor to end of line
                        self.buf.truncate(self.cursor);
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    12 => {
                        // Ctrl-L: clear screen
                        eprint!("\x1b[2J\x1b[H");
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    21 if self.cursor > 0 => {
                        // Ctrl-U: delete from cursor to start of line
                        self.buf.drain(..self.cursor);
                        self.cursor = 0;
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    23 if self.cursor > 0 => {
                        // Ctrl-W: delete previous word
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
                    8 | 127 if self.cursor > 0 => {
                        let prev = self.prev_char_boundary();
                        self.buf.drain(prev..self.cursor);
                        self.cursor = prev;
                        self.render(cwd).map_err(|e| e.to_string())?;
                    }
                    8 | 127 => {}
                    b'\r' | b'\n' => {
                        let mut submitted = String::from_utf8_lossy(&self.buf).into_owned();
                        let trimmed_sub = submitted.trim();
                        if trimmed_sub.starts_with('/')
                            && !trimmed_sub.contains(char::is_whitespace)
                        {
                            let (_, cands) = completion_candidates(trimmed_sub, cwd);
                            if cands.len() == 1 && !cands[0].ends_with(' ') {
                                submitted = cands[0].clone();
                            }
                        }
                        eprint!("\r\x1b[2Kmio> {submitted}\r\n");
                        let _ = io::stderr().flush();
                        self.push_history(&submitted);
                        return Ok(ReadOutcome::Line(submitted));
                    }
                    b if b >= 0x20 => {
                        self.buf.insert(self.cursor, b);
                        self.cursor += 1;
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

/// 交互式 REPL：`mio> ` 提示符、Tab 补全、上下键历史与 slash 命令。
///
/// 不进入 alternate screen，也不清屏；只有显式 `/clear` 才发送清屏序列。
pub(super) async fn run_interactive<F>(
    runner: &mut Runner,
    directory: &Path,
    cwd: &Path,
    render: F,
) -> Result<(), String>
where
    F: Fn(&str) -> Result<String, String>,
{
    let reporter = runner.host.reporter.clone();
    reporter.report(ProgramState::Idle, None);
    let mut editor = InteractiveEditor::new();
    loop {
        let line = match editor.read_line(cwd)? {
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
            "/pins" => {
                for entry in runner.session.pinned_context.entries() {
                    eprintln!("{} {:?}: {:?}", entry.kind, entry.id, entry.text);
                }
                continue;
            }
            _ if trimmed == "/pin"
                || trimmed == "/unpin"
                || trimmed.starts_with("/pin ")
                || trimmed.starts_with("/unpin ") =>
            {
                match edit_pinned_context(&runner.session.pinned_context, trimmed) {
                    Ok(pinned) => {
                        let mut candidate = runner.session.clone();
                        candidate.pinned_context = pinned;
                        match runner.commit_session(candidate, runner.path.clone()) {
                            Ok(()) => eprintln!("Pinned context updated."),
                            Err(error) => eprintln!("Mio: {error}"),
                        }
                    }
                    Err(error) => eprintln!("Mio: {error}"),
                }
                continue;
            }
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => {
                super::args::print_help();
                continue;
            }
            "/clear" => {
                print!("\x1b[2J\x1b[H");
                let _ = io::stdout().flush();
                continue;
            }
            "/sessions" => {
                print_sessions(directory, false)?;
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
            _ if trimmed.starts_with("/checkpoints") || trimmed == "/checkpoint" => {
                let arg = trimmed
                    .strip_prefix("/checkpoints")
                    .or_else(|| trimmed.strip_prefix("/checkpoint"))
                    .unwrap_or("")
                    .trim();
                match arg {
                    "on" | "enable" => match runner.set_checkpoints(true) {
                        Ok(()) => eprintln!(
                            "File checkpoints enabled: write/edit will snapshot before each change."
                        ),
                        Err(error) => eprintln!("Mio: {error}"),
                    },
                    "off" | "disable" => match runner.set_checkpoints(false) {
                        Ok(()) => eprintln!("File checkpoints disabled."),
                        Err(error) => eprintln!("Mio: {error}"),
                    },
                    "" => match runner.checkpoints.list() {
                        Err(error) => eprintln!("Mio: {error}"),
                        Ok(entries) if entries.is_empty() => eprintln!(
                            "No file checkpoints in this workspace (enable with /checkpoints on)."
                        ),
                        Ok(entries) => {
                            eprintln!(
                                "File checkpoints ({} total, snapshots {}):",
                                entries.len(),
                                if runner.checkpoints_enabled() {
                                    "on"
                                } else {
                                    "off"
                                }
                            );
                            for entry in entries {
                                eprintln!(
                                    "{}  {:?}  {}  {}  ({} -> {} bytes)",
                                    entry.id,
                                    entry.state,
                                    entry.operation,
                                    entry.target,
                                    entry.before_len.unwrap_or(0),
                                    entry.after_len
                                );
                            }
                        }
                    },
                    other => eprintln!("Usage: /checkpoints [on|off] (unknown argument '{other}')"),
                }
                continue;
            }
            _ if trimmed.starts_with("/rewind") => {
                let arg = trimmed.strip_prefix("/rewind").unwrap_or("").trim();
                if arg.is_empty() {
                    eprintln!("Usage: /rewind <checkpoint-id> (see /checkpoints)");
                    continue;
                }
                match runner.rewind_checkpoint(arg).await {
                    Ok(outcome) => eprintln!(
                        "Rewind {}: {:?} {}",
                        outcome.id, outcome.action, outcome.target
                    ),
                    Err(error) => eprintln!("Mio: {error}"),
                }
                continue;
            }
            "/new" | "/fork" => {
                let candidate = if trimmed == "/fork" {
                    runner.session.fork()
                } else {
                    match Session::new(cwd) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Mio: {e}");
                            continue;
                        }
                    }
                };
                let target_path = directory.join(format!("{}.json", candidate.id));
                if let Err(e) = runner.commit_session(candidate, target_path) {
                    eprintln!("Mio: {e}");
                    continue;
                }
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
                    let mut candidate = runner.session.clone();
                    candidate.set_name(Some(title.to_owned()));
                    if let Err(e) = runner.commit_session(candidate, runner.path.clone()) {
                        eprintln!("Mio: {e}");
                        continue;
                    }
                    eprintln!("Session named '{title}'");
                }
                continue;
            }
            _ if trimmed.starts_with("/compact") => {
                let focus = trimmed.strip_prefix("/compact").unwrap_or("").trim();
                let mut candidate = runner.session.clone();
                let stats = candidate.compact(
                    DEFAULT_KEEP_RECENT_MESSAGES,
                    if focus.is_empty() { None } else { Some(focus) },
                );
                if let Err(e) = runner.commit_session(candidate, runner.path.clone()) {
                    eprintln!("Mio: {e}");
                    continue;
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
                    eprintln!(
                        "Current model: {}/{}",
                        runner.provider_name, runner.config.model
                    );
                    let mut ready: Vec<String> = Vec::new();
                    let mut others = 0usize;
                    for entry in list_catalog_models(None) {
                        if entry.configured {
                            ready.push(format!("{}/{}", entry.provider, entry.model));
                        } else {
                            others += 1;
                        }
                    }
                    if ready.is_empty() {
                        eprintln!("No configured providers; use /model provider/model to switch.");
                    } else {
                        eprintln!("Available models:");
                        for line in ready.iter().take(20) {
                            eprintln!("  {line}");
                        }
                        if ready.len() > 20 {
                            eprintln!("  ... {} more configured", ready.len() - 20);
                        }
                        if others > 0 {
                            eprintln!(
                                "  ({others} unconfigured catalog models; type /model <provider>/<model>)"
                            );
                        }
                    }
                } else {
                    match runner.set_model(arg, None) {
                        Ok(()) => eprintln!(
                            "Switched model to {}/{}",
                            runner.provider_name, runner.config.model
                        ),
                        Err(error) => eprintln!("Cannot switch model: {error}"),
                    }
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
                    let levels: Vec<&str> = ThinkingLevel::ALL.iter().map(|l| l.as_str()).collect();
                    eprintln!("Available levels: {}", levels.join(" "));
                    eprintln!("Usage: /thinking <level>");
                } else {
                    let model = runner.config.model.clone();
                    match ThinkingLevel::from_str(arg).and_then(|level| {
                        runner.set_model(&model, Some(level))?;
                        Ok(level)
                    }) {
                        Ok(level) => eprintln!("Thinking level set to {}", level.as_str()),
                        Err(error) => eprintln!("Cannot set thinking level: {error}"),
                    }
                }
                continue;
            }
            "/plugins" => {
                let installed = mio_coding_agent::plugin::discover_plugins(cwd)?;
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
                        let installed = mio_coding_agent::plugin::discover_plugins(cwd)?;
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
                        let installed = mio_coding_agent::plugin::install_plugin(cwd, arg)?;
                        eprintln!(
                            "Installed plugin '{} v{}' into {}",
                            installed.manifest.name,
                            installed.manifest.version,
                            installed.root_dir.display()
                        );
                    }
                    "remove" => {
                        mio_coding_agent::plugin::remove_plugin(cwd, arg)?;
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
                            cwd.to_path_buf()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_pin_edits_preserve_existing_context_on_rejected_updates() {
        use mio_coding_agent::context_memory::PinnedContext;
        let empty = PinnedContext::default();
        let pinned = edit_pinned_context(&empty, "/pin constraint api 保留公共 API").unwrap();
        assert_eq!(pinned.get("api").unwrap().text, "保留公共 API");
        assert!(edit_pinned_context(&pinned, "/pin goal api other").is_err());
        assert_eq!(pinned.get("api").unwrap().text, "保留公共 API");
        let replaced = edit_pinned_context(&pinned, "/pin replace goal api 完成迁移").unwrap();
        assert_eq!(replaced.get("api").unwrap().text, "完成迁移");
        assert!(edit_pinned_context(&replaced, "/unpin missing").is_err());
        assert!(
            edit_pinned_context(&replaced, "/unpin api")
                .unwrap()
                .is_empty()
        );
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
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/pin "));
        editor.handle_tab(dir.path());
        assert_eq!(editor.as_str(), Some("/pins"));

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
