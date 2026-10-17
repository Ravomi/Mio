//! Content-free observations of the `read` tool's actual returned range.
//!
//! `tool_started` only carries the operation and the requested target, so a
//! consumer cannot tell which line or byte range a read really returned. This
//! module defines the bounded, structured observation the engine emits on a
//! real, successful `read`: the normalized workspace-relative path plus the
//! *actual* returned line range (`offset`/`limit`, both counted from the lines
//! the tool really produced) or byte range (`byteOffset`/`byteLimit`, both
//! counted from the bytes the tool really returned), the file total and
//! whether the result was truncated.
//!
//! What never crosses this channel: file contents, absolute paths, request
//! parameters that were not actually returned, tool arguments or any host path
//! outside the workspace. Identity fields are copied from the engine's real
//! tool dispatch when it supplied one; an absent identity stays absent rather
//! than being fabricated. The observation is only built after a read succeeded:
//! a rejected or failed read emits nothing.
//!
//! The field names match the accepted spellings of the diagnostics collector
//! (`scripts/eval_diagnostics.py`): `offset`/`limit` for a line range and
//! `byteOffset`/`byteLimit` for a byte range, with the exact same meaning. A
//! zero-length result (an empty file or an empty page) is reported as an
//! explicit zero-length unknown range rather than a fabricated one-line range.

use crate::tool_identity::ToolIdentity;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use uuid::Uuid;

/// Upper bound on the emitted workspace-relative path, in Unicode scalar
/// values. It matches the diagnostics collector's default field bound so a
/// normalized path the engine emits is never rejected as too long.
pub const MAX_READ_PATH_CHARS: usize = 256;

/// The family of an observed read range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRangeKind {
    /// A line range reported through `offset`/`limit`.
    Line,
    /// A byte range reported through `byteOffset`/`byteLimit`.
    Byte,
    /// No representable range (an empty file or a zero-length page).
    Unknown,
}

/// The actual range a successful read returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRange {
    /// A line range: the 1-based `offset` of the first returned line and
    /// `limit` equal to the number of lines actually returned.
    Lines { offset: u64, limit: u64 },
    /// A byte range: the 0-based `offset` of the first returned byte and
    /// `limit` equal to the number of bytes actually returned.
    Bytes { offset: u64, limit: u64 },
    /// No representable range: an empty file or a zero-length page. `total`
    /// and `truncated` still describe the real result.
    Unknown,
}

/// One content-free observation of a successful `read` call.
///
/// Fields the engine cannot truthfully supply stay `None`; they are never
/// fabricated. `subagent_id`/`parent_run_id` are added by a delegated child's
/// host so the child's own real run UUID is preserved while the parent link is
/// marked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadObservation {
    /// Real engine run UUID, when the engine supplied a dispatch identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    /// Delegated run identity; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    /// The run that started this delegated run; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    /// One-based engine model-call index the call belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// The model's own tool-call id, when the engine supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The dispatcher's minted effect id, when the engine supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<u64>,
    /// Normalized workspace-relative path. Never absolute.
    pub path: String,
    /// Which range family this observation reports.
    pub kind: ReadRangeKind,
    /// 1-based first returned line; present only for a real line range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Number of lines actually returned; present only for a real line range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// 0-based first returned byte; present only for a real byte range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
    /// Number of bytes actually returned; present only for a real byte range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_limit: Option<u64>,
    /// Total lines for a line read or total bytes for a byte read; `None` when
    /// the total itself is unknown. An empty file reports `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Whether the returned range stopped before the end of the file.
    pub truncated: bool,
}

impl ReadObservation {
    /// Build one observation from the actual result of a successful read.
    ///
    /// `identity` is the engine's real dispatch identity read off the tool
    /// task scope before any blocking work; `None` is preserved as absent.
    pub fn new(
        identity: Option<&ToolIdentity>,
        path: String,
        range: ReadRange,
        total: Option<u64>,
        truncated: bool,
    ) -> Self {
        let (kind, offset, limit, byte_offset, byte_limit) = match range {
            ReadRange::Lines { offset, limit } => {
                (ReadRangeKind::Line, Some(offset), Some(limit), None, None)
            }
            ReadRange::Bytes { offset, limit } => {
                (ReadRangeKind::Byte, None, None, Some(offset), Some(limit))
            }
            ReadRange::Unknown => (ReadRangeKind::Unknown, None, None, None, None),
        };
        Self {
            run_id: identity.map(ToolIdentity::run_id),
            subagent_id: None,
            parent_run_id: None,
            turn_id: identity.and_then(ToolIdentity::turn_id),
            tool_call_id: identity.map(|identity| identity.tool_call_id().to_owned()),
            effect_id: identity.map(ToolIdentity::effect_id),
            path,
            kind,
            offset,
            limit,
            byte_offset,
            byte_limit,
            total,
            truncated,
        }
    }
}

/// Normalize a validated read target into a bounded, workspace-relative
/// posix-style path.
///
/// The read implementation only calls this after `checked_path` accepted the
/// target, so the input is already a relative in-workspace path. This helper
/// makes the emitted form canonical and refuses anything it cannot represent
/// honestly:
///
/// - a non-UTF-8 component returns `None` (no observation is emitted);
/// - absolute paths, parent traversal and platform prefixes return `None`;
/// - control characters return `None` (the diagnostics path sanitizer rejects
///   them, so emitting one would only be dropped);
/// - a path longer than [`MAX_READ_PATH_CHARS`] returns `None`.
///
/// `None` never leaks the rejected input; the caller simply emits nothing for
/// that read.
pub fn normalize_read_path(raw: &str) -> Option<String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part.is_empty() || part.chars().any(char::is_control) {
                    return None;
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    let relative = parts.join("/");
    if relative.chars().count() > MAX_READ_PATH_CHARS {
        return None;
    }
    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_use_offset_and_limit_only() {
        let observation = ReadObservation::new(
            None,
            "src/main.rs".into(),
            ReadRange::Lines {
                offset: 10,
                limit: 5,
            },
            Some(40),
            true,
        );
        let value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["path"], "src/main.rs");
        assert_eq!(value["offset"], 10);
        assert_eq!(value["limit"], 5);
        assert_eq!(value["kind"], "line");
        assert_eq!(value["total"], 40);
        assert!(value["truncated"].as_bool().unwrap());
        assert!(value.get("byteOffset").is_none());
        assert!(value.get("byteLimit").is_none());
        assert!(value.get("runId").is_none());
    }

    #[test]
    fn byte_ranges_use_byte_offsets_only() {
        let observation = ReadObservation::new(
            None,
            "out.txt".into(),
            ReadRange::Bytes {
                offset: 4,
                limit: 64,
            },
            Some(4096),
            false,
        );
        let value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["byteOffset"], 4);
        assert_eq!(value["byteLimit"], 64);
        assert_eq!(value["kind"], "byte");
        assert!(value.get("offset").is_none());
        assert!(value.get("limit").is_none());
    }

    #[test]
    fn unknown_ranges_carry_no_bounds() {
        let observation =
            ReadObservation::new(None, "empty".into(), ReadRange::Unknown, Some(0), false);
        let value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["kind"], "unknown");
        assert_eq!(value["total"], 0);
        assert!(value.get("offset").is_none());
        assert!(value.get("limit").is_none());
        assert!(value.get("byteOffset").is_none());
        assert!(value.get("byteLimit").is_none());
    }

    #[test]
    fn normalize_accepts_relative_and_rejects_escapes_and_overlong() {
        assert_eq!(
            normalize_read_path("src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            normalize_read_path("./a/./b.txt").as_deref(),
            Some("a/b.txt")
        );
        assert_eq!(normalize_read_path("a//b.txt").as_deref(), Some("a/b.txt"));
        assert!(normalize_read_path("/etc/passwd").is_none());
        assert!(normalize_read_path("../outside").is_none());
        assert!(normalize_read_path("a/../../etc/shadow").is_none());
        assert!(normalize_read_path("").is_none());
        assert!(normalize_read_path(&"x".repeat(MAX_READ_PATH_CHARS + 1)).is_none());
        assert!(normalize_read_path("bad\u{1}name").is_none());
    }
}
