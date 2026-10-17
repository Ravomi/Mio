use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use uuid::Uuid;

const CURRENT_SESSION_VERSION: u32 = 1;
const MAX_SESSION_FILE_SIZE: usize = 16 * 1024 * 1024;
const MAX_SESSION_LIST: usize = 1000;
/// Upper bound on directory entries inspected while scanning sessions.
const MAX_SESSION_SCAN: usize = 20_000;

/// Newest-first scan result: `(candidates, warnings, exceeded_entry_cap)`.
type SessionScan = (Vec<(SystemTime, PathBuf)>, Vec<String>, bool);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub created: u64,
    pub updated: u64,
    pub history: Vec<crate::Message>,
    #[serde(
        default,
        skip_serializing_if = "crate::context_memory::PinnedContext::is_empty"
    )]
    pub pinned_context: crate::context_memory::PinnedContext,
    /// Most recent explicit session compaction. Request-level audits belong to
    /// the Host observation/journal channel instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_loss: Option<crate::compact::CompactionLossReport>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMetadata {
    pub version: u32,
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub created: u64,
    pub updated: u64,
    pub message_count: usize,
    pub file_path: PathBuf,
}

impl Session {
    pub fn new(cwd: &Path) -> Result<Self, String> {
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize cwd {}: {e}", cwd.display()))?;
        if !canonical_cwd.is_dir() {
            return Err(format!(
                "cwd is not a directory: {}",
                canonical_cwd.display()
            ));
        }
        let now = now_secs();
        Ok(Self {
            version: CURRENT_SESSION_VERSION,
            id: Uuid::new_v4(),
            parent_id: None,
            name: None,
            cwd: canonical_cwd,
            created: now,
            updated: now,
            history: Vec::new(),
            pinned_context: crate::context_memory::PinnedContext::default(),
            last_compaction_loss: None,
        })
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        let trimmed = name.into().trim().to_owned();
        self.name = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        self
    }

    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    }

    pub fn fork(&self) -> Self {
        let now = now_secs();
        Self {
            version: CURRENT_SESSION_VERSION,
            id: Uuid::new_v4(),
            parent_id: Some(self.id),
            name: self.name.as_ref().map(|n| format!("{n} (fork)")),
            cwd: self.cwd.clone(),
            created: now,
            updated: now,
            history: self.history.clone(),
            pinned_context: self.pinned_context.clone(),
            last_compaction_loss: self.last_compaction_loss.clone(),
        }
    }

    /// Prepare a compacted candidate in memory; callers still save atomically
    /// before replacing their live session.
    pub fn compact(
        &mut self,
        keep_recent: usize,
        focus: Option<&str>,
    ) -> crate::compact::CompactionStats {
        let (history, mut stats) = crate::compact::compact_history_with_pinned(
            &self.history,
            keep_recent,
            focus,
            Some(&self.pinned_context),
        );
        if let Some(report) = stats.loss_report.as_mut() {
            report.pinned_items_preserved = (!self.pinned_context.is_empty()).then_some(true);
            self.last_compaction_loss = Some(report.clone());
        }
        self.history = history;
        stats
    }

    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        let mut snapshot = self.clone();
        snapshot.updated = now_secs().max(snapshot.created);
        snapshot.validate()?;
        // Hold the shared workspace activity + output leases for the whole
        // snapshot-to-disk interval. When `.mio/outputs` does not exist this is a
        // no-op that creates nothing; when an exclusive cleanup lease is active the
        // save is refused before any temp file is created, keeping the previous
        // file intact. The same lease set is used for the external source
        // registration below.
        let output_guard =
            match crate::shell::output_store::OutputStore::guard_existing(self.cwd.as_path()) {
                Ok(lease) => lease,
                Err(error) => {
                    return Err(format!(
                        "Session save refused: the workspace output lease is unavailable: {error}"
                    ));
                }
            };
        let parent_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent_dir.exists() {
            return Err(format!(
                "Directory does not exist: {}",
                parent_dir.display()
            ));
        }

        let mut temp = NamedTempFile::new_in(parent_dir)
            .map_err(|e| format!("Failed to create tempfile in {}: {e}", parent_dir.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("Failed to set 0600 mode: {e}"))?;
        }

        let payload = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| format!("Failed to serialize session: {e}"))?;
        if payload.len() > MAX_SESSION_FILE_SIZE {
            return Err(format!(
                "Session size {} exceeds limit {MAX_SESSION_FILE_SIZE}",
                payload.len()
            ));
        }

        temp.write_all(&payload)
            .map_err(|e| format!("Failed to write: {e}"))?;
        temp.as_file()
            .sync_all()
            .map_err(|e| format!("Failed to sync: {e}"))?;
        // A session saved outside `.mio/sessions` can hold output references that
        // the standard cleanup scan cannot see. Register the source *before*
        // publishing the new file, while still holding the shared activity lease:
        // a registration failure (for example a full bounded registry) must leave
        // the previous session snapshot intact instead of publishing an update
        // whose references cannot be enumerated. The pending path resolves to the
        // same canonical target the published file will have.
        if output_guard.is_some() {
            let store = crate::shell::output_store::OutputStore::open(self.cwd.as_path())
                .map_err(|error| {
                    format!(
                        "Session save refused: its output reference source could not be registered: {error}"
                    )
                })?;
            store
                .register_pending_reference_source(path)
                .map_err(|error| {
                    format!(
                        "Session save refused: its output reference source could not be registered: {error}"
                    )
                })?;
        }
        temp.persist(path)
            .map_err(|e| format!("Failed to persist to {}: {e}", path.display()))?;
        self.updated = snapshot.updated;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        if !fs::symlink_metadata(path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("Session must be a regular file, not a directory or symlink".into());
        }
        let mut file =
            File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        let mut buf = Vec::new();
        let read_bytes = (&mut file)
            .take((MAX_SESSION_FILE_SIZE + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

        if read_bytes > MAX_SESSION_FILE_SIZE {
            return Err(format!(
                "Session file {} exceeds {MAX_SESSION_FILE_SIZE} bytes",
                path.display()
            ));
        }

        let session: Self = serde_json::from_slice(&buf)
            .map_err(|e| format!("Corrupted session in {}: {e}", path.display()))?;
        session.validate()?;
        Ok(session)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != CURRENT_SESSION_VERSION {
            return Err(format!(
                "Unsupported version {} (expected {CURRENT_SESSION_VERSION})",
                self.version
            ));
        }
        if !self.cwd.is_absolute() || self.updated < self.created {
            return Err("Invalid session workspace or timestamps".into());
        }
        Ok(())
    }

    pub fn list(directory: &Path) -> Result<(Vec<SessionMetadata>, Vec<String>), String> {
        Self::list_with_limit(directory, MAX_SESSION_LIST)
    }

    /// List at most `limit` sessions, newest file first.
    ///
    /// An incomplete scan fails rather than presenting a partial set as newest.
    /// Every directory entry is inspected and ordered by modification time
    /// *before* the limit is applied, so the newest session is always a
    /// candidate even in directories larger than the limit. This replaces the
    /// previous behaviour of truncating an unsorted directory enumeration and
    /// only then sorting, which could hide the most recent session.
    pub fn list_with_limit(
        directory: &Path,
        limit: usize,
    ) -> Result<(Vec<SessionMetadata>, Vec<String>), String> {
        let (candidates, mut warnings, truncated) = Self::scan(directory)?;
        if truncated {
            return Err(format!(
                "Cannot list or select the latest session: directory exceeds {MAX_SESSION_SCAN} entries; use an explicit session path"
            ));
        }
        let total = candidates.len();
        let mut list = Vec::new();
        for (_, path) in candidates.iter().take(limit) {
            match Self::load(path) {
                Ok(s) => list.push(SessionMetadata {
                    version: s.version,
                    id: s.id,
                    parent_id: s.parent_id,
                    name: s.name,
                    cwd: s.cwd,
                    created: s.created,
                    updated: s.updated,
                    message_count: s.history.len(),
                    file_path: path.clone(),
                }),
                Err(err) => warnings.push(format!(
                    "Failed to parse session at {}: {err}",
                    path.display()
                )),
            }
        }
        if total > limit {
            warnings.push(format!(
                "Session listing shows the {limit} most recently modified sessions out of {total}"
            ));
        }
        Ok((list, warnings))
    }

    /// Enumerate candidate session files and order them newest-first.
    ///
    /// Returns `(candidates, warnings, truncated)` where `truncated` is true
    /// when the directory exceeded [`MAX_SESSION_SCAN`] entries.
    fn scan(directory: &Path) -> Result<SessionScan, String> {
        if !directory.is_dir() {
            return Err(format!("Not a directory: {}", directory.display()));
        }
        let read_dir = fs::read_dir(directory).map_err(|e| format!("Failed read dir: {e}"))?;
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();
        let mut visited = 0usize;
        let mut truncated = false;
        for entry in read_dir {
            visited += 1;
            if visited > MAX_SESSION_SCAN {
                truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warnings.push(format!("Cannot read session directory entry: {e}"));
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    warnings.push(format!("Cannot get file type of {:?}: {e}", entry.path()));
                    continue;
                }
            };
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            // Metadata only: parsing every history is deferred until after the
            // newest-first ordering (and the caller's limit) are known.
            match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
                Ok(modified) => candidates.push((modified, path)),
                Err(e) => warnings.push(format!(
                    "Cannot read modification time of {}: {e}",
                    path.display()
                )),
            }
        }
        if truncated {
            warnings.push(format!(
                "Session scan stopped at {MAX_SESSION_SCAN} directory entries"
            ));
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok((candidates, warnings, truncated))
    }

    /// Resolve a session selector (`path`, UUID/short-UUID prefix, or session name).
    ///
    /// Matching is ranked: an exact UUID always wins, then a unique exact name,
    /// then a unique UUID prefix, then a unique name substring. Ambiguity inside
    /// a rank is an error rather than an arbitrary first match.
    /// If `query` looks like a file path (contains `/` or ends with `.json`), returns `cwd.join(query)`
    /// even if it does not exist yet so `--session path.json` can create a new session at that path.
    pub fn resolve(directory: &Path, cwd: &Path, query: &str) -> Result<PathBuf, String> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err("Session selector cannot be empty".into());
        }
        let direct = cwd.join(trimmed);
        if direct.exists() {
            return Ok(direct);
        }
        if directory.is_dir() {
            let (candidates, _warnings, truncated) = Self::scan(directory)?;
            if truncated {
                return Err(format!(
                    "Cannot resolve session selector '{trimmed}': session directory exceeds {MAX_SESSION_SCAN} entries"
                ));
            }
            let q_lower = trimmed.to_ascii_lowercase();
            let mut exact_uuid: Option<PathBuf> = None;
            let mut exact_names: Vec<PathBuf> = Vec::new();
            let mut uuid_prefixes: Vec<PathBuf> = Vec::new();
            let mut name_substrings: Vec<PathBuf> = Vec::new();
            for (_, path) in &candidates {
                let Ok(session) = Self::load(path) else {
                    continue;
                };
                let id = session.id.to_string();
                if id.eq_ignore_ascii_case(trimmed) {
                    exact_uuid.get_or_insert_with(|| path.clone());
                    continue;
                }
                if session
                    .name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(trimmed))
                {
                    exact_names.push(path.clone());
                    continue;
                }
                if id.to_ascii_lowercase().starts_with(&q_lower) {
                    uuid_prefixes.push(path.clone());
                    continue;
                }
                if session
                    .name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(&q_lower))
                {
                    name_substrings.push(path.clone());
                }
            }
            if let Some(path) = exact_uuid {
                return Ok(path);
            }
            if !exact_names.is_empty() {
                if exact_names.len() == 1 {
                    return Ok(exact_names.remove(0));
                }
                return Err(format!(
                    "Ambiguous session selector '{trimmed}' matched {} sessions with the same name",
                    exact_names.len()
                ));
            }
            if !uuid_prefixes.is_empty() {
                if uuid_prefixes.len() == 1 {
                    return Ok(uuid_prefixes.remove(0));
                }
                return Err(format!(
                    "Ambiguous session selector '{trimmed}' matched {} session ids",
                    uuid_prefixes.len()
                ));
            }
            if !name_substrings.is_empty() {
                if name_substrings.len() == 1 {
                    return Ok(name_substrings.remove(0));
                }
                return Err(format!(
                    "Ambiguous session selector '{trimmed}' matched {} sessions by name",
                    name_substrings.len()
                ));
            }
        }
        Ok(direct)
    }

    /// Export this session to a self-contained HTML file.
    pub fn export_html(&self, output_path: &Path) -> Result<(), String> {
        if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create export directory: {e}"))?;
        }
        let title = self
            .name
            .as_deref()
            .map(escape_html)
            .unwrap_or_else(|| format!("Session {}", self.id));
        let mut html = format!(
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Mio — {title}</title>\n\
            <style>\n\
            body {{ background:#0d1117; color:#e6edf3; font-family:system-ui,-apple-system,sans-serif; max-width:920px; margin:2rem auto; padding:0 1rem; line-height:1.5; }}\n\
            header {{ border-bottom:1px solid #30363d; padding-bottom:1rem; margin-bottom:1.5rem; }}\n\
            .meta {{ color:#8b949e; font-size:0.875rem; font-family:monospace; }}\n\
            .msg {{ border:1px solid #30363d; border-radius:8px; padding:1rem; margin-bottom:1rem; background:#161b22; }}\n\
            .role {{ font-weight:600; font-size:0.85rem; text-transform:uppercase; margin-bottom:0.5rem; color:#58a6ff; }}\n\
            .role.assistant {{ color:#3fb950; }}\n\
            pre {{ white-space:pre-wrap; word-break:break-word; background:#0d1117; padding:0.75rem; border-radius:6px; overflow-x:auto; margin:0.5rem 0 0; font-size:0.875rem; }}\n\
            </style>\n</head>\n<body>\n<header>\n<h1>{title}</h1>\n\
            <div class=\"meta\">ID: {} · Workspace: {} · Messages: {}</div>\n</header>\n<main>\n",
            self.id,
            escape_html(&self.cwd.display().to_string()),
            self.history.len()
        );
        for msg in &self.history {
            let val = serde_json::to_value(msg).unwrap_or_default();
            let role = val["role"].as_str().unwrap_or("message");
            let role_class = if role == "assistant" {
                "role assistant"
            } else {
                "role"
            };
            let content_pretty =
                serde_json::to_string_pretty(&val["content"]).unwrap_or_else(|_| val.to_string());
            html.push_str(&format!(
                "<section class=\"msg\"><div class=\"{role_class}\">{}</div><pre>{}</pre></section>\n",
                escape_html(role),
                escape_html(&content_pretty)
            ));
        }
        html.push_str("</main>\n</body>\n</html>\n");
        fs::write(output_path, html)
            .map_err(|e| format!("Failed to write HTML export {}: {e}", output_path.display()))
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_session_roundtrip_and_fork() {
        let dir = tempdir().unwrap();
        let session_file = dir.path().join("sess.json");
        let mut session = Session::new(dir.path()).unwrap();
        assert_eq!(session.version, 1);
        assert!(session.parent_id.is_none());

        session.history.push(crate::Message::user("Hello"));
        session.history.push(crate::Message::assistant("World"));
        session.save(&session_file).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&session_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let loaded = Session::load(&session_file).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.history.len(), 2);

        let forked = loaded.fork();
        assert_ne!(forked.id, loaded.id);
        assert_eq!(forked.parent_id, Some(loaded.id));
        assert_eq!(forked.history.len(), 2);
    }

    #[test]
    fn test_corrupted_and_unsupported_version() {
        let dir = tempdir().unwrap();
        let bad_json = dir.path().join("bad.json");
        fs::write(&bad_json, "{ bad json }").unwrap();
        assert!(Session::load(&bad_json).is_err());

        let bad_ver = dir.path().join("version2.json");
        let mut invalid = Session::new(dir.path()).unwrap();
        invalid.version = 999;
        fs::write(&bad_ver, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(
            Session::load(&bad_ver)
                .unwrap_err()
                .contains("Unsupported version")
        );
        invalid.version = 1;
        invalid.cwd = "relative".into();
        fs::write(&bad_ver, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(Session::load(&bad_ver).unwrap_err().contains("workspace"));
        assert!(Session::load(dir.path()).is_err());
    }

    #[test]
    fn test_missing_parent_directory() {
        let dir = tempdir().unwrap();
        let mut session = Session::new(dir.path()).unwrap();
        assert!(
            session
                .save(&dir.path().join("missing").join("sess.json"))
                .is_err()
        );
    }

    #[test]
    fn test_list_sessions_with_warnings() {
        let dir = tempdir().unwrap();
        let mut s1 = Session::new(dir.path()).unwrap();
        s1.save(&dir.path().join("s1.json")).unwrap();
        fs::write(dir.path().join("corrupt.json"), "invalid").unwrap();
        fs::write(dir.path().join("ignore.txt"), "text").unwrap();

        let (list, warnings) = Session::list(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, s1.id);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Failed to parse session"));
    }

    /// Save a named session and force a distinct file mtime so ordering is
    /// deterministic even when wall-clock seconds collide.
    fn save_named(dir: &Path, file: &str, name: &str, mtime_nanos: u64) -> Session {
        let path = dir.join(file);
        let mut session = Session::new(dir).unwrap().with_name(name);
        session.save(&path).unwrap();
        let times = std::fs::FileTimes::new()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_nanos(mtime_nanos));
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();
        session
    }

    #[test]
    fn test_list_orders_by_mtime_before_applying_the_limit() {
        let dir = tempdir().unwrap();
        // Ten sessions; the newest is written last with the highest mtime.
        let mut newest = None;
        for index in 0..10u64 {
            let session = save_named(
                dir.path(),
                &format!("s{index}.json"),
                &format!("s{index}"),
                1_000 + index,
            );
            if index == 9 {
                newest = Some(session.id);
            }
        }
        // A small limit must still contain the globally newest session.
        let (list, warnings) = Session::list_with_limit(dir.path(), 3).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, newest.unwrap());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("most recently modified"))
        );
        assert_eq!(list[1].name.as_deref(), Some("s8"));
        assert_eq!(list[2].name.as_deref(), Some("s7"));
    }

    #[test]
    fn test_resolve_prefers_uuid_and_reports_ambiguous_names() {
        let dir = tempdir().unwrap();
        let s1 = save_named(dir.path(), "one.json", "duplicate", 10);
        let s2 = save_named(dir.path(), "two.json", "Duplicate", 20);
        let s3 = save_named(dir.path(), "three.json", "other", 30);

        // Case-insensitive duplicate names are ambiguous, not "first match".
        let err = Session::resolve(dir.path(), dir.path(), "duplicate").unwrap_err();
        assert!(err.contains("Ambiguous"), "unexpected error: {err}");

        // A full UUID always resolves, even when it could also look like a name.
        let by_id = Session::resolve(dir.path(), dir.path(), &s2.id.to_string()).unwrap();
        assert_eq!(by_id, dir.path().join("two.json"));

        // A unique short-uuid prefix resolves.
        let prefix = &s3.id.to_string()[..8];
        let by_prefix = Session::resolve(dir.path(), dir.path(), prefix).unwrap();
        assert_eq!(by_prefix, dir.path().join("three.json"));

        // A unique name substring resolves.
        let by_substring = Session::resolve(dir.path(), dir.path(), "othe").unwrap();
        assert_eq!(by_substring, dir.path().join("three.json"));

        // An unknown selector falls back to a new path under cwd.
        let fallback = Session::resolve(dir.path(), dir.path(), "brand-new").unwrap();
        assert_eq!(fallback, dir.path().join("brand-new"));

        // Keep s1 alive as an explicit reference to the parsed session.
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn oversized_scan_refuses_latest_and_preserves_explicit_path_access() {
        let dir = tempdir().unwrap();
        let saved = save_named(dir.path(), "selected.json", "selected", 10);
        for index in 0..MAX_SESSION_SCAN {
            fs::write(dir.path().join(format!("entry-{index}")), b"").unwrap();
        }
        let error = Session::list_with_limit(dir.path(), 1).unwrap_err();
        assert!(error.contains("explicit session path"), "{error}");
        assert!(Session::resolve(dir.path(), dir.path(), "selected").is_err());
        let path = Session::resolve(dir.path(), dir.path(), "selected.json").unwrap();
        assert_eq!(Session::load(&path).unwrap().id, saved.id);
    }
}
