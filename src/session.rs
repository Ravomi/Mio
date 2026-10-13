use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use uuid::Uuid;

const CURRENT_SESSION_VERSION: u32 = 1;
const MAX_SESSION_FILE_SIZE: usize = 16 * 1024 * 1024;
const MAX_SESSION_LIST: usize = 1000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub cwd: PathBuf,
    pub created: u64,
    pub updated: u64,
    pub history: Vec<crate::Message>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMetadata {
    pub version: u32,
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
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
            cwd: canonical_cwd,
            created: now,
            updated: now,
            history: Vec::new(),
        })
    }

    pub fn fork(&self) -> Self {
        let now = now_secs();
        Self {
            version: CURRENT_SESSION_VERSION,
            id: Uuid::new_v4(),
            parent_id: Some(self.id),
            cwd: self.cwd.clone(),
            created: now,
            updated: now,
            history: self.history.clone(),
        }
    }

    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        let mut snapshot = self.clone();
        snapshot.updated = now_secs().max(snapshot.created);
        snapshot.validate()?;
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
        if !directory.is_dir() {
            return Err(format!("Not a directory: {}", directory.display()));
        }
        let read_dir = fs::read_dir(directory).map_err(|e| format!("Failed read dir: {e}"))?;
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        for (visited, entry) in read_dir.enumerate() {
            if visited >= MAX_SESSION_LIST {
                warnings.push(format!(
                    "Session listing truncated at {MAX_SESSION_LIST} directory entries"
                ));
                break;
            }
            match entry {
                Ok(entry) => entries.push(entry),
                Err(e) => warnings.push(format!("Cannot read session directory entry: {e}")),
            }
        }
        entries.sort_by_key(|e| e.file_name());

        let mut list = Vec::new();
        for entry in entries {
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

            match Self::load(&path) {
                Ok(s) => list.push(SessionMetadata {
                    version: s.version,
                    id: s.id,
                    parent_id: s.parent_id,
                    cwd: s.cwd,
                    created: s.created,
                    updated: s.updated,
                    message_count: s.history.len(),
                    file_path: path,
                }),
                Err(err) => warnings.push(format!(
                    "Failed to parse session at {}: {err}",
                    path.display()
                )),
            }
        }
        Ok((list, warnings))
    }
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
}
