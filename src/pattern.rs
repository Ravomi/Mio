//! Pattern matching (Glob, bounded Thompson NFA Regex), ToolFilter, and Unified Diff helpers.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "workspace",
    "read",
    "write",
    "edit",
    "bash",
    "shell",
    "grep",
    "find",
    "ls",
    "subagent",
];

/// Controls which built-in and MCP tools are exposed and permitted at execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFilter {
    allow_all: bool,
    allowed_patterns: Vec<String>,
    excluded_patterns: Vec<String>,
}

impl Default for ToolFilter {
    fn default() -> Self {
        Self::all()
    }
}

impl ToolFilter {
    /// Allow all tools by default.
    pub fn all() -> Self {
        Self {
            allow_all: true,
            allowed_patterns: Vec::new(),
            excluded_patterns: Vec::new(),
        }
    }

    /// Disable all tools (`--no-tools` / `-nt`).
    pub fn none() -> Self {
        Self {
            allow_all: false,
            allowed_patterns: Vec::new(),
            excluded_patterns: Vec::new(),
        }
    }

    /// Build a filter from an explicit tool whitelist (e.g., from `DelegationConstraints`).
    pub fn from_whitelist(tools: &[String]) -> Result<Self, String> {
        let mut allowed = Vec::new();
        for raw in tools {
            let name = raw.trim();
            if name.is_empty() {
                return Err("Tool name in whitelist cannot be empty".into());
            }
            validate_tool_pattern(name)?;
            allowed.push(name.to_owned());
        }
        Ok(Self {
            allow_all: false,
            allowed_patterns: allowed,
            excluded_patterns: Vec::new(),
        })
    }

    /// Build a filter from CLI options (`--tools`, `--exclude-tools`, `--no-tools`).
    pub fn from_cli(
        tools: Option<&str>,
        exclude: Option<&str>,
        no_tools: bool,
    ) -> Result<Self, String> {
        if no_tools {
            if tools.is_some() {
                return Err("--no-tools cannot be combined with --tools".into());
            }
            return Ok(Self::none());
        }

        let mut filter = Self::all();

        if let Some(spec) = tools {
            let entries: Vec<&str> = spec
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if entries.is_empty() {
                return Err("--tools requires at least one tool name or pattern".into());
            }

            let is_delta = entries
                .iter()
                .all(|e| e.starts_with('+') || e.starts_with('-'));
            if is_delta {
                for entry in entries {
                    let (add, pat) = if let Some(rest) = entry.strip_prefix('+') {
                        (true, rest.trim())
                    } else if let Some(rest) = entry.strip_prefix('-') {
                        (false, rest.trim())
                    } else {
                        unreachable!()
                    };
                    validate_tool_pattern(pat)?;
                    if add {
                        filter.excluded_patterns.retain(|x| x != pat);
                        if !filter.allowed_patterns.iter().any(|x| x == pat) {
                            filter.allowed_patterns.push(pat.to_owned());
                        }
                    } else {
                        filter.excluded_patterns.push(pat.to_owned());
                    }
                }
            } else {
                filter.allow_all = false;
                for entry in entries {
                    let pat = entry.strip_prefix('+').unwrap_or(entry).trim();
                    if pat.starts_with('-') {
                        return Err(
                            "Cannot mix allowlist entries with '-tool' without '+' prefixes".into(),
                        );
                    }
                    validate_tool_pattern(pat)?;
                    filter.allowed_patterns.push(pat.to_owned());
                }
            }
        }

        if let Some(ex_spec) = exclude {
            for entry in ex_spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                validate_tool_pattern(entry)?;
                filter.excluded_patterns.push(entry.to_owned());
            }
        }

        Ok(filter)
    }

    pub fn is_none(&self) -> bool {
        !self.allow_all && self.allowed_patterns.is_empty()
    }

    pub fn is_disabled(&self) -> bool {
        self.is_none()
    }

    /// Check whether a tool or workspace operation is permitted.
    /// Treats `"bash"` and `"shell"` as synonyms for command execution.
    pub fn allows(&self, name: &str) -> bool {
        let single = [name];
        let candidates: &[&str] = match name {
            "bash" | "shell" => &["bash", "shell"],
            _ => &single,
        };

        if self.is_excluded(name) {
            return false;
        }

        if self.allow_all {
            return true;
        }

        candidates.iter().any(|candidate| {
            self.allowed_patterns
                .iter()
                .any(|pat| matches_glob(pat, candidate))
        })
    }

    /// Explicit exclusions also apply through the aggregate workspace tool.
    pub fn allows_workspace_operation(&self, operation: &str) -> bool {
        !self.is_excluded(operation) && (self.allows(operation) || self.allows("workspace"))
    }

    fn is_excluded(&self, name: &str) -> bool {
        let aliases: &[&str] = match name {
            "bash" | "shell" => &["bash", "shell"],
            _ => &[name],
        };
        aliases.iter().any(|name| {
            self.excluded_patterns
                .iter()
                .any(|pattern| matches_glob(pattern, name))
        })
    }

    /// Returns true if any workspace operation is permitted.
    pub fn allows_any_workspace_op(&self) -> bool {
        self.allows("workspace")
            || [
                "read", "write", "edit", "bash", "shell", "grep", "find", "ls",
            ]
            .iter()
            .any(|op| self.allows(op))
    }

    /// Returns true if all write/shell operations are disallowed by this filter.
    pub fn is_read_only_only(&self) -> bool {
        !self.allows("write") && !self.allows("edit") && !self.allows("bash")
    }
}

fn validate_tool_pattern(pat: &str) -> Result<(), String> {
    if pat.is_empty() {
        return Err("Empty tool pattern".into());
    }
    if pat.starts_with("mcp__") || has_glob_meta(pat) {
        return Ok(());
    }
    if BUILTIN_TOOL_NAMES.contains(&pat) {
        Ok(())
    } else {
        Err(format!("Unsupported tool '{pat}' in tool filter"))
    }
}

/// Returns true if `pattern` contains glob metacharacters.
pub fn has_glob_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| matches!(c, '*' | '?' | '[' | '{'))
}

/// Glob matcher supporting `*`, `**`, `?`, `[abc]`, `[a-z]`, `[!abc]`/`[^abc]`, and `{a,b}`.
pub fn matches_glob(pattern: &str, text: &str) -> bool {
    if let Some((prefix, body, suffix)) = split_brace_expansion(pattern) {
        for alt in body.split(',') {
            let expanded = format!("{prefix}{alt}{suffix}");
            if matches_glob(&expanded, text) {
                return true;
            }
        }
        return false;
    }
    let pat_chars: Vec<char> = pattern.chars().collect();
    let txt_chars: Vec<char> = text.chars().collect();
    glob_match_slice(&pat_chars, &txt_chars)
}

fn split_brace_expansion(pattern: &str) -> Option<(&str, &str, &str)> {
    let open = pattern.find('{')?;
    let rest = &pattern[open + 1..];
    let close = rest.find('}')?;
    let body = &rest[..close];
    if !body.contains(',') {
        return None;
    }
    Some((&pattern[..open], body, &rest[close + 1..]))
}

fn glob_match_slice(pat: &[char], txt: &[char]) -> bool {
    let mut p = 0;
    let mut t = 0;
    let mut star_p: Option<usize> = None;
    let mut star_t: usize = 0;

    while t < txt.len() {
        if p < pat.len() && pat[p] == '*' {
            let is_double = p + 1 < pat.len() && pat[p + 1] == '*';
            while p < pat.len() && pat[p] == '*' {
                p += 1;
            }
            if is_double {
                let next_pat = if p < pat.len() && pat[p] == '/' {
                    &pat[p + 1..]
                } else {
                    &pat[p..]
                };
                if next_pat.is_empty() {
                    return true;
                }
                for idx in t..=txt.len() {
                    if (idx == t || txt[idx - 1] == '/' || p >= pat.len() || pat[p] != '/')
                        && glob_match_slice(next_pat, &txt[idx..])
                    {
                        return true;
                    }
                }
                return false;
            }
            star_p = Some(p);
            star_t = t;
            continue;
        }

        if p < pat.len() {
            if pat[p] == '?' && txt[t] != '/' {
                p += 1;
                t += 1;
                continue;
            } else if pat[p] == '[' {
                if let Some((matched, next_p)) = match_char_class(&pat[p + 1..], txt[t]) {
                    if matched && txt[t] != '/' {
                        p += 1 + next_p;
                        t += 1;
                        continue;
                    }
                } else if pat[p] == txt[t] {
                    p += 1;
                    t += 1;
                    continue;
                }
            } else if pat[p] == txt[t] {
                p += 1;
                t += 1;
                continue;
            }
        }

        if let Some(sp) = star_p {
            if txt[star_t] == '/' {
                return false;
            }
            star_t += 1;
            t = star_t;
            p = sp;
        } else {
            return false;
        }
    }

    while p < pat.len()
        && (pat[p] == '*' || (p >= 2 && pat[p - 1] == '*' && pat[p - 2] == '*' && pat[p] == '/'))
    {
        p += 1;
    }
    p == pat.len()
}

fn match_char_class(pat: &[char], ch: char) -> Option<(bool, usize)> {
    let mut idx = 0;
    let negated = if idx < pat.len() && (pat[idx] == '!' || pat[idx] == '^') {
        idx += 1;
        true
    } else {
        false
    };
    let mut matched = false;
    let mut closed = false;
    while idx < pat.len() {
        if pat[idx] == ']' && idx > usize::from(negated) {
            closed = true;
            idx += 1;
            break;
        }
        if idx + 2 < pat.len() && pat[idx + 1] == '-' && pat[idx + 2] != ']' {
            let start = pat[idx];
            let end = pat[idx + 2];
            if ch >= start && ch <= end {
                matched = true;
            }
            idx += 3;
        } else {
            if pat[idx] == ch {
                matched = true;
            }
            idx += 1;
        }
    }
    if !closed {
        return None;
    }
    Some((if negated { !matched } else { matched }, idx))
}

const MAX_REGEX_LEN: usize = 2048;
const MAX_NFA_STATES: usize = 512;

/// Line matcher for `grep` supporting literal or bounded Thompson NFA regex matching.
pub enum LineMatcher {
    Literal {
        needle: String,
        case_insensitive: bool,
    },
    Regex(NfaRegex),
}

impl LineMatcher {
    pub fn new(pattern: &str, regex: bool, case_insensitive: bool) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("搜索内容 pattern 不能为空".into());
        }
        if regex {
            Ok(Self::Regex(NfaRegex::compile(pattern, case_insensitive)?))
        } else {
            let needle = if case_insensitive {
                pattern.to_lowercase()
            } else {
                pattern.to_owned()
            };
            Ok(Self::Literal {
                needle,
                case_insensitive,
            })
        }
    }

    pub fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Literal {
                needle,
                case_insensitive,
            } => {
                if *case_insensitive {
                    line.to_lowercase().contains(needle)
                } else {
                    line.contains(needle)
                }
            }
            Self::Regex(nfa) => nfa.is_match(line),
        }
    }
}

#[derive(Clone, Debug)]
enum Atom {
    Any,
    Char(char),
    Digit(bool),
    Word(bool),
    Space(bool),
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    AnchorStart,
    AnchorEnd,
    WordBoundary(bool),
}

impl Atom {
    fn matches_char(&self, ch: char, case_insensitive: bool) -> bool {
        let c = if case_insensitive {
            ch.to_ascii_lowercase()
        } else {
            ch
        };
        match self {
            Self::Any => ch != '\n' && ch != '\r',
            Self::Char(expected) => {
                let exp = if case_insensitive {
                    expected.to_ascii_lowercase()
                } else {
                    *expected
                };
                c == exp
            }
            Self::Digit(pos) => ch.is_ascii_digit() == *pos,
            Self::Word(pos) => (ch.is_ascii_alphanumeric() || ch == '_') == *pos,
            Self::Space(pos) => ch.is_whitespace() == *pos,
            Self::Class { negated, ranges } => {
                let found = ranges.iter().any(|&(lo, hi)| {
                    if case_insensitive {
                        c >= lo.to_ascii_lowercase() && c <= hi.to_ascii_lowercase()
                    } else {
                        ch >= lo && ch <= hi
                    }
                });
                if *negated { !found } else { found }
            }
            Self::AnchorStart | Self::AnchorEnd | Self::WordBoundary(_) => false,
        }
    }
}

#[derive(Clone, Debug)]
enum State {
    Consume { atom: Atom, out: usize },
    Split(usize, usize),
    Match,
}

#[derive(Clone, Debug)]
pub struct NfaRegex {
    states: Vec<State>,
    start: usize,
    case_insensitive: bool,
}

#[derive(Clone)]
struct Frag {
    start: usize,
    dangling: Vec<(usize, bool)>, // (state_idx, is_second_branch)
}

impl NfaRegex {
    pub fn compile(pattern: &str, case_insensitive: bool) -> Result<Self, String> {
        if pattern.len() > MAX_REGEX_LEN {
            return Err("正则表达式超过最大长度限制".into());
        }
        let chars: Vec<char> = pattern.chars().collect();
        let mut compiler = Compiler {
            chars: &chars,
            pos: 0,
            states: Vec::new(),
        };
        let frag = compiler.parse_alternation()?;
        if compiler.pos < compiler.chars.len() {
            return Err(format!(
                "无效的正则表达式：位置 {} 处存在未匹配的 '{}'",
                compiler.pos, compiler.chars[compiler.pos]
            ));
        }
        let match_state = compiler.push_state(State::Match)?;
        compiler.patch(&frag.dangling, match_state);
        Ok(Self {
            states: compiler.states,
            start: frag.start,
            case_insensitive,
        })
    }

    pub fn is_match(&self, line: &str) -> bool {
        let chars: Vec<char> = line.chars().collect();
        for start_idx in 0..=chars.len() {
            let mut current = BTreeSet::new();
            self.add_state(&mut current, self.start, &chars, start_idx);
            if current
                .iter()
                .any(|&s| matches!(self.states[s], State::Match))
            {
                return true;
            }
            for idx in start_idx..chars.len() {
                let ch = chars[idx];
                let mut next = BTreeSet::new();
                for &s in &current {
                    if let State::Consume { ref atom, out } = self.states[s]
                        && atom.matches_char(ch, self.case_insensitive)
                    {
                        self.add_state(&mut next, out, &chars, idx + 1);
                    }
                }
                if next.iter().any(|&s| matches!(self.states[s], State::Match)) {
                    return true;
                }
                if next.is_empty() {
                    break;
                }
                current = next;
            }
        }
        false
    }

    fn add_state(&self, set: &mut BTreeSet<usize>, state_idx: usize, chars: &[char], pos: usize) {
        if !set.insert(state_idx) {
            return;
        }
        match &self.states[state_idx] {
            State::Split(out1, out2) => {
                self.add_state(set, *out1, chars, pos);
                self.add_state(set, *out2, chars, pos);
            }
            State::Consume { atom, out } => match atom {
                Atom::AnchorStart => {
                    if pos == 0 {
                        self.add_state(set, *out, chars, pos);
                    }
                }
                Atom::AnchorEnd => {
                    if pos == chars.len() {
                        self.add_state(set, *out, chars, pos);
                    }
                }
                Atom::WordBoundary(expected) => {
                    let prev_word = pos > 0 && is_word_char(chars[pos - 1]);
                    let next_word = pos < chars.len() && is_word_char(chars[pos]);
                    if (prev_word != next_word) == *expected {
                        self.add_state(set, *out, chars, pos);
                    }
                }
                _ => {}
            },
            State::Match => {}
        }
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

struct Compiler<'a> {
    chars: &'a [char],
    pos: usize,
    states: Vec<State>,
}

impl<'a> Compiler<'a> {
    fn push_state(&mut self, state: State) -> Result<usize, String> {
        if self.states.len() >= MAX_NFA_STATES {
            return Err("正则表达式复杂度超过状态上限".into());
        }
        let idx = self.states.len();
        self.states.push(state);
        Ok(idx)
    }

    fn patch(&mut self, dangling: &[(usize, bool)], target: usize) {
        for &(idx, second) in dangling {
            match &mut self.states[idx] {
                State::Consume { out, .. } => *out = target,
                State::Split(out1, out2) => {
                    if second {
                        *out2 = target;
                    } else {
                        *out1 = target;
                    }
                }
                State::Match => {}
            }
        }
    }

    fn parse_alternation(&mut self) -> Result<Frag, String> {
        let mut left = self.parse_concat()?;
        while self.pos < self.chars.len() && self.chars[self.pos] == '|' {
            self.pos += 1;
            let right = self.parse_concat()?;
            let split = self.push_state(State::Split(left.start, right.start))?;
            let mut dangling = left.dangling;
            dangling.extend(right.dangling);
            left = Frag {
                start: split,
                dangling,
            };
        }
        Ok(left)
    }

    fn parse_concat(&mut self) -> Result<Frag, String> {
        let mut frags: Vec<Frag> = Vec::new();
        while self.pos < self.chars.len() && !matches!(self.chars[self.pos], '|' | ')') {
            frags.push(self.parse_quantified()?);
        }
        if frags.is_empty() {
            let nop = self.push_state(State::Split(0, 0))?;
            return Ok(Frag {
                start: nop,
                dangling: vec![(nop, false)],
            });
        }
        let mut iter = frags.into_iter();
        let mut acc = iter.next().unwrap();
        for next in iter {
            self.patch(&acc.dangling, next.start);
            acc.dangling = next.dangling;
        }
        Ok(acc)
    }

    fn parse_quantified(&mut self) -> Result<Frag, String> {
        let base = self.parse_atom()?;
        if self.pos >= self.chars.len() {
            return Ok(base);
        }
        match self.chars[self.pos] {
            '*' => {
                self.pos += 1;
                let split = self.push_state(State::Split(base.start, 0))?;
                self.patch(&base.dangling, split);
                Ok(Frag {
                    start: split,
                    dangling: vec![(split, true)],
                })
            }
            '+' => {
                self.pos += 1;
                let split = self.push_state(State::Split(base.start, 0))?;
                self.patch(&base.dangling, split);
                Ok(Frag {
                    start: base.start,
                    dangling: vec![(split, true)],
                })
            }
            '?' => {
                self.pos += 1;
                let split = self.push_state(State::Split(base.start, 0))?;
                let mut dangling = base.dangling;
                dangling.push((split, true));
                Ok(Frag {
                    start: split,
                    dangling,
                })
            }
            _ => Ok(base),
        }
    }

    fn parse_atom(&mut self) -> Result<Frag, String> {
        if self.pos >= self.chars.len() {
            return Err("意外结束的正则表达式".into());
        }
        let ch = self.chars[self.pos];
        match ch {
            '(' => {
                self.pos += 1;
                if self.pos + 1 < self.chars.len()
                    && self.chars[self.pos] == '?'
                    && self.chars[self.pos + 1] == ':'
                {
                    self.pos += 2;
                }
                let inner = self.parse_alternation()?;
                if self.pos >= self.chars.len() || self.chars[self.pos] != ')' {
                    return Err("无效的正则表达式：缺少闭合括号 ')'".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            '[' => {
                self.pos += 1;
                let atom = self.parse_class()?;
                let s = self.push_state(State::Consume { atom, out: 0 })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
            '^' => {
                self.pos += 1;
                let s = self.push_state(State::Consume {
                    atom: Atom::AnchorStart,
                    out: 0,
                })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
            '$' => {
                self.pos += 1;
                let s = self.push_state(State::Consume {
                    atom: Atom::AnchorEnd,
                    out: 0,
                })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
            '.' => {
                self.pos += 1;
                let s = self.push_state(State::Consume {
                    atom: Atom::Any,
                    out: 0,
                })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
            '\\' => {
                self.pos += 1;
                if self.pos >= self.chars.len() {
                    return Err("无效的正则表达式：末尾孤立转义符 '\\'".into());
                }
                let esc = self.chars[self.pos];
                self.pos += 1;
                let atom = match esc {
                    'd' => Atom::Digit(true),
                    'D' => Atom::Digit(false),
                    'w' => Atom::Word(true),
                    'W' => Atom::Word(false),
                    's' => Atom::Space(true),
                    'S' => Atom::Space(false),
                    'b' => Atom::WordBoundary(true),
                    'B' => Atom::WordBoundary(false),
                    'n' => Atom::Char('\n'),
                    'r' => Atom::Char('\r'),
                    't' => Atom::Char('\t'),
                    other => Atom::Char(other),
                };
                let s = self.push_state(State::Consume { atom, out: 0 })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
            '*' | '+' | '?' => Err(format!("无效的正则表达式：量词 '{ch}' 缺少前置表达式")),
            other => {
                self.pos += 1;
                let s = self.push_state(State::Consume {
                    atom: Atom::Char(other),
                    out: 0,
                })?;
                Ok(Frag {
                    start: s,
                    dangling: vec![(s, false)],
                })
            }
        }
    }

    fn parse_class(&mut self) -> Result<Atom, String> {
        let negated = if self.pos < self.chars.len()
            && (self.chars[self.pos] == '^' || self.chars[self.pos] == '!')
        {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut ranges = Vec::new();
        let mut closed = false;
        let start_pos = self.pos;
        while self.pos < self.chars.len() {
            if self.chars[self.pos] == ']' && self.pos > start_pos {
                self.pos += 1;
                closed = true;
                break;
            }
            let c1 = self.next_class_char()?;
            if self.pos + 1 < self.chars.len()
                && self.chars[self.pos] == '-'
                && self.chars[self.pos + 1] != ']'
            {
                self.pos += 1;
                let c2 = self.next_class_char()?;
                ranges.push((c1, c2));
            } else {
                ranges.push((c1, c1));
            }
        }
        if !closed {
            return Err("无效的正则表达式：字符类缺少闭合 ']'".into());
        }
        Ok(Atom::Class { negated, ranges })
    }

    fn next_class_char(&mut self) -> Result<char, String> {
        let ch = self.chars[self.pos];
        self.pos += 1;
        if ch == '\\' {
            if self.pos >= self.chars.len() {
                return Err("无效的正则表达式：字符类中转义未完成".into());
            }
            let esc = self.chars[self.pos];
            self.pos += 1;
            Ok(match esc {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            })
        } else {
            Ok(ch)
        }
    }
}

/// Generate a concise unified diff snippet for an `edit` replacement.
pub fn unified_diff(path: &str, start_line: usize, old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut out = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{start_line},{} +{start_line},{} @@\n",
        old_lines.len().max(1),
        new_lines.len().max(1)
    );
    const MAX_DIFF_LINES: usize = 40;
    for (idx, line) in old_lines.iter().enumerate() {
        if idx >= MAX_DIFF_LINES {
            out.push_str(&format!("-... ({} more lines)\n", old_lines.len() - idx));
            break;
        }
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for (idx, line) in new_lines.iter().enumerate() {
        if idx >= MAX_DIFF_LINES {
            out.push_str(&format!("+... ({} more lines)\n", new_lines.len() - idx));
            break;
        }
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_matching() {
        assert!(matches_glob("*.rs", "main.rs"));
        assert!(!matches_glob("*.rs", "src/main.rs"));
        assert!(matches_glob("**/*.rs", "main.rs"));
        assert!(matches_glob("**/*.rs", "src/bin/mio.rs"));
        assert!(matches_glob("src/*.{rs,toml}", "src/lib.rs"));
        assert!(matches_glob("src/*.{rs,toml}", "src/Cargo.toml"));
        assert!(matches_glob("file_[0-9].txt", "file_7.txt"));
        assert!(!matches_glob("file_[!0-9].txt", "file_7.txt"));
        assert!(matches_glob("mcp__*", "mcp__github__search"));
    }

    #[test]
    fn test_regex_nfa_matcher() {
        let m = LineMatcher::new(r"^pub\s+(async\s+)?fn\s+\w+\(", true, false).unwrap();
        assert!(m.is_match("pub async fn execute(&self) {"));
        assert!(m.is_match("pub fn new() -> Self"));
        assert!(!m.is_match("fn private_helper()"));

        let ci = LineMatcher::new(r"error|warning", true, true).unwrap();
        assert!(ci.is_match("Fatal ERROR occurred"));
        assert!(ci.is_match("A minor Warning"));
        assert!(!ci.is_match("All good"));

        assert!(LineMatcher::new("(unclosed", true, false).is_err());
        assert!(LineMatcher::new("[unclosed", true, false).is_err());
    }

    #[test]
    fn test_tool_filter_cli_and_whitelist() {
        let ro = ToolFilter::from_cli(Some("read,grep,find,ls"), None, false).unwrap();
        assert!(ro.allows("read"));
        assert!(ro.allows("grep"));
        assert!(!ro.allows("write"));
        assert!(!ro.allows("bash"));
        assert!(ro.is_read_only_only());

        let delta = ToolFilter::from_cli(Some("-write,-edit"), Some("bash"), false).unwrap();
        assert!(delta.allows("read"));
        assert!(!delta.allows("write"));
        assert!(!delta.allows("shell"));
        assert!(delta.is_read_only_only());

        let none = ToolFilter::from_cli(None, None, true).unwrap();
        assert!(none.is_none());
        assert!(!none.allows("read"));

        assert!(ToolFilter::from_cli(Some("nonexistent_tool"), None, false).is_err());
    }
}
