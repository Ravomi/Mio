//! OSC 7501 Program Status Reporting Specification (rev 0.4)
//!
//! Specification summary:
//! - Format: `\x1b]7501;state=<state>[:kind=<kind>][:app=<app>][:msg=<base64>]\x1b\\`
//! - States: `working`, `blocked`, `done`, `idle`, `error`
//! - In Mio, `Blocked` program states always contain a `kind`.
//! - App identity: `:app=mio` included in every status report.
//! - Message field: `:msg=<base64>` where standard Base64 encodes UTF-8 text with control characters sanitized.
//! - Length limits:
//!   - Raw UTF-8 msg: at most 2048 bytes (truncated at char boundary if longer).
//!   - Total sequence length: at most 4096 bytes.
//! - Pure encoding APIs without implicit stdout/stderr writes in library code.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

pub const APP_NAME: &str = "mio";
pub const MAX_MSG_RAW_BYTES: usize = 2048;
pub const MAX_SEQUENCE_BYTES: usize = 4096;

/// Blocked state kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

impl BlockedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }
}

/// OSC 7501 program states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramState {
    Working,
    Blocked { kind: BlockedKind },
    Done,
    Idle,
    Error,
}

impl ProgramState {
    pub fn state_name(&self) -> &'static str {
        match self {
            ProgramState::Working => "working",
            ProgramState::Blocked { .. } => "blocked",
            ProgramState::Done => "done",
            ProgramState::Idle => "idle",
            ProgramState::Error => "error",
        }
    }
}

/// Truncate a UTF-8 string to at most `max_bytes` on a valid char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Sanitize control characters in a message string.
///
/// Input is sliced to at most `MAX_MSG_RAW_BYTES` first to prevent unbounded memory allocation.
/// Control characters are replaced with a single space. Consecutive spaces are collapsed and trailing space is stripped.
pub fn sanitize_msg(raw: &str) -> String {
    let bounded = truncate_utf8(raw, MAX_MSG_RAW_BYTES);
    let mut out = String::with_capacity(bounded.len());
    let mut last_was_space = false;
    for ch in bounded.chars() {
        if ch.is_control() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = ch == ' ';
        }
    }
    if last_was_space && out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Format an OSC 7501 sequence.
///
/// Output format: `\x1b]7501;state=<state>[:kind=<kind>]:app=mio[:msg=<base64>]\x1b\\`
pub fn format_osc7501(state: &ProgramState, msg: Option<&str>) -> String {
    let mut payload = format!("state={}", state.state_name());
    if let ProgramState::Blocked { kind } = state {
        payload.push_str(":kind=");
        payload.push_str(kind.as_str());
    }
    payload.push_str(":app=");
    payload.push_str(APP_NAME);

    if let Some(raw_msg) = msg {
        let sanitized = sanitize_msg(raw_msg);
        if !sanitized.is_empty() {
            let encoded = BASE64_STANDARD.encode(sanitized);
            payload.push_str(":msg=");
            payload.push_str(&encoded);
        }
    }

    format!("\x1b]7501;{}\x1b\\", payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_basic_states() {
        assert_eq!(
            format_osc7501(&ProgramState::Working, None),
            "\x1b]7501;state=working:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(&ProgramState::Idle, None),
            "\x1b]7501;state=idle:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(&ProgramState::Done, None),
            "\x1b]7501;state=done:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(&ProgramState::Error, None),
            "\x1b]7501;state=error:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(
                &ProgramState::Blocked {
                    kind: BlockedKind::Permission
                },
                None
            ),
            "\x1b]7501;state=blocked:kind=permission:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(
                &ProgramState::Blocked {
                    kind: BlockedKind::Question
                },
                None
            ),
            "\x1b]7501;state=blocked:kind=question:app=mio\x1b\\"
        );
        assert_eq!(
            format_osc7501(
                &ProgramState::Blocked {
                    kind: BlockedKind::Auth
                },
                None
            ),
            "\x1b]7501;state=blocked:kind=auth:app=mio\x1b\\"
        );
    }

    #[test]
    fn encodes_with_msg_base64() {
        let seq = format_osc7501(&ProgramState::Working, Some("hello world"));
        let b64 = BASE64_STANDARD.encode("hello world");
        assert_eq!(
            seq,
            format!("\x1b]7501;state=working:app=mio:msg={b64}\x1b\\")
        );
    }

    #[test]
    fn sanitizes_control_characters() {
        let raw = "line1\nline2\x1b[31m\t\x00end\u{0085}after";
        let sanitized = sanitize_msg(raw);
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\t'));
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\0'));
        assert!(!sanitized.contains('\u{0085}'));
    }

    #[test]
    fn truncates_multibyte_utf8_correctly() {
        // '中' is 3 bytes (0xE4, 0xB8, 0xAD)
        let s = "中".repeat(1000); // 3000 bytes
        let truncated = truncate_utf8(&s, 2048);
        assert!(truncated.len() <= 2048);
        assert_eq!(truncated.len() % 3, 0); // Must be a whole number of 3-byte chars
        std::str::from_utf8(truncated.as_bytes()).expect("valid utf-8");
    }

    #[test]
    fn sequence_length_bound_with_huge_input() {
        let long_str = "a".repeat(100_000);
        let seq = format_osc7501(&ProgramState::Working, Some(&long_str));
        assert!(seq.len() <= MAX_SEQUENCE_BYTES);
    }
}
