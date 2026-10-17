//! Persistent, per-workspace trust store for project-scope MCP servers.
//!
//! Project MCP configuration runs repository-controlled commands, so starting
//! one requires explicit host approval (AUD-03). That approval is bound to a
//! fingerprint over the command, arguments, environment and source workspace,
//! and — for hosts that explicitly opt in — recorded in a user-scope store so a
//! later instance in the same workspace does not have to ask again while the
//! configuration is unchanged.
//!
//! The store lives **outside the project**, under the user's configuration
//! directory:
//!
//! ```text
//! <config_dir>/mcp-trust/<sha256(canonical-workspace)>/trust.json
//! ```
//!
//! A project-controlled path (for example `<workspace>/.mio/mcp-trust.json`)
//! can never be the trust root: a malicious repository could otherwise
//! pre-seed an approval for its own command. A host may point the store at an
//! isolated directory through [`crate::Host::mcp_trust_directory`]; a directory
//! that resolves inside the workspace is refused fail-closed.
//!
//! The store never contains raw command or environment values: only a SHA-256
//! fingerprint, the server name, the canonical workspace and an approval
//! timestamp. It is written atomically with mode `0600` inside a `0700`
//! directory. A symlinked directory or store file is refused, and a corrupt or
//! unsupported store grants no trust (fail-closed): every server is re-approved
//! before it is spawned again.
//!
//! This module is declared from [`crate::mcp`] with an explicit `#[path]`, so
//! it does not need a second declaration in `lib.rs`.
//!
//! Concurrent approvals from two processes in the same workspace are
//! last-writer-wins: the atomic rename prevents a corrupt store, but a lost
//! entry only means that server must be approved again (fail-closed), never
//! that an unapproved server is trusted.

use crate::mcp::McpServerSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// On-disk trust store schema version.
pub const MCP_TRUST_VERSION: u32 = 1;
/// Directory created under the user configuration directory.
pub const MCP_TRUST_ROOT_DIR: &str = "mcp-trust";
/// File name inside a workspace's trust directory.
pub const MCP_TRUST_FILE: &str = "trust.json";
/// Hard limit for the trust store file.
const MAX_TRUST_FILE_BYTES: u64 = 256 * 1024;
/// Bound on the number of remembered server approvals.
const MAX_TRUST_SERVERS: usize = 256;
/// Longest human-readable approval target shown to a host.
const MAX_DISPLAY_CHARS: usize = 1024;

/// One remembered approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTrustEntry {
    /// SHA-256 over workspace, server, command, arguments and environment.
    pub fingerprint: String,
    /// Canonical workspace the approval was granted in.
    #[serde(default)]
    pub workspace: String,
    /// Unix seconds when the host approved the fingerprint.
    #[serde(default)]
    pub approved_at: u64,
}

/// Serialized trust store.
#[derive(Debug, Default, Serialize, Deserialize)]
struct McpTrustFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, McpTrustEntry>,
}

/// A workspace's user-scope trust store.
///
/// Only the fingerprint digest, server name, canonical workspace and timestamp
/// are persisted. Command and environment values never reach the file. The
/// store is only ever loaded by a host that opted in through
/// [`crate::Host::allow_persisted_mcp_trust`]; a non-opt-in host approves every
/// project server start itself.
pub struct McpTrustStore {
    workspace: String,
    path: PathBuf,
    file: McpTrustFile,
    writable: bool,
    diagnostic: Option<String>,
}

impl McpTrustStore {
    /// Load the trust store for a workspace from the user's Mio profile.
    ///
    /// When the profile directory cannot be resolved the store fails closed:
    /// no persisted trust is read and none can be written, so the host must
    /// approve each server start itself.
    pub fn load(workspace: &Path) -> Self {
        match crate::profile::config_directory() {
            Ok(config_dir) => Self::load_in(workspace, &config_dir),
            Err(error) => {
                let workspace = canonical_workspace(workspace);
                let mut store = Self {
                    workspace: workspace.to_string_lossy().into_owned(),
                    path: PathBuf::new(),
                    file: McpTrustFile::default(),
                    writable: true,
                    diagnostic: None,
                };
                store.fail_closed(format!("user configuration directory unavailable: {error}"));
                store
            }
        }
    }

    /// Load the trust store for a workspace from an explicit configuration
    /// directory.
    ///
    /// `trusted_config_dir` is the base directory that contains the
    /// `mcp-trust/` root, normally the user's Mio configuration directory. It
    /// exists so an embedding host — and the test suite — can isolate the store
    /// without ever touching the real user profile.
    ///
    /// A missing store yields empty trust. A root or store that resolves inside
    /// the workspace, a symlinked directory or file, a non-regular store, an
    /// unreadable file, an unsupported version and an oversized file are all
    /// reported as fail-closed diagnostics: no entry is trusted and the store is
    /// not overwritten. A merely corrupt JSON body also trusts nothing, but is
    /// left writable so an explicit re-approval can atomically repair it.
    pub fn load_in(workspace: &Path, trusted_config_dir: &Path) -> Self {
        let workspace = canonical_workspace(workspace);
        let workspace_string = workspace.to_string_lossy().into_owned();
        let root = trust_root(trusted_config_dir);
        let directory = root.join(workspace_key(&workspace));
        let path = directory.join(MCP_TRUST_FILE);
        let mut store = Self {
            workspace: workspace_string,
            path,
            file: McpTrustFile::default(),
            writable: true,
            diagnostic: None,
        };

        // The trust anchor must never live inside the project whose server is
        // being approved, or a repository could pre-seed its own approval.
        if path_is_within(&root, &workspace) {
            store.fail_closed(format!(
                "MCP trust directory {} is inside workspace {}; project-controlled trust is refused",
                root.display(),
                workspace.display()
            ));
            return store;
        }

        // Both the trust root and the per-workspace directory must be real
        // directories. A symlink could redirect the store into the project.
        for dir in [&root, &directory] {
            match fs::symlink_metadata(dir) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    store.fail_closed(format!(
                        "{} must be a real directory, not a symlink or file",
                        dir.display()
                    ));
                    return store;
                }
                // Nothing is persisted yet; empty trust and a writable store.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
                Err(error) => {
                    store.fail_closed(format!("cannot inspect {}: {error}", dir.display()));
                    return store;
                }
            }
        }

        match fs::symlink_metadata(&store.path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                // A trust store that is not private cannot be trusted: another
                // local user (or a world-writable copy) may have edited it. It
                // is still writable so an explicit approval can repair it.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = metadata.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        store.diagnostic = Some(format!(
                            "{} is not private (mode {mode:03o}); existing trust is ignored",
                            store.path.display()
                        ));
                        return store;
                    }
                }
            }
            Ok(_) => {
                let diagnostic = format!(
                    "{} must be a regular file, not a symlink",
                    store.path.display()
                );
                store.fail_closed(diagnostic);
                return store;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
            Err(error) => {
                let diagnostic = format!("cannot inspect {}: {error}", store.path.display());
                store.fail_closed(diagnostic);
                return store;
            }
        }

        let bytes = match read_bounded(&store.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                store.fail_closed(error);
                return store;
            }
        };
        match serde_json::from_slice::<McpTrustFile>(&bytes) {
            Ok(file) if file.version != MCP_TRUST_VERSION => {
                store.fail_closed(format!(
                    "unsupported MCP trust store version {}",
                    file.version
                ));
            }
            Ok(file) if file.servers.len() > MAX_TRUST_SERVERS => {
                store.fail_closed(format!(
                    "MCP trust store records more than {MAX_TRUST_SERVERS} servers"
                ));
            }
            Ok(file) => store.file = file,
            Err(error) => {
                // Corrupt JSON grants no trust but is repairable by an explicit
                // re-approval, which rewrites the file atomically.
                store.diagnostic = Some(format!("corrupt MCP trust store: {error}"));
            }
        }
        store
    }

    /// Path of the trust store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Canonical workspace this store was loaded for.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// A fail-closed or repair diagnostic, when the store was not clean.
    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    /// Whether the exact fingerprint for this server is already approved.
    pub fn is_trusted(&self, server: &str, fingerprint: &str) -> bool {
        self.file.servers.get(server).is_some_and(|entry| {
            entry.fingerprint == fingerprint && entry.workspace == self.workspace
        })
    }

    /// Remember one approved fingerprint and persist the store.
    ///
    /// Refuses to write when the store location failed closed (symlinked or
    /// project-controlled location, unsupported version or oversized file). The
    /// caller may still start the server for this process; the approval simply
    /// will not be reused.
    pub fn record(&mut self, server: &str, fingerprint: &str) -> Result<(), String> {
        if !self.writable {
            return Err(self
                .diagnostic
                .clone()
                .unwrap_or_else(|| "MCP trust store is not writable".into()));
        }
        self.file.version = MCP_TRUST_VERSION;
        self.file.servers.insert(
            server.to_owned(),
            McpTrustEntry {
                fingerprint: fingerprint.to_owned(),
                workspace: self.workspace.clone(),
                approved_at: now_secs(),
            },
        );
        while self.file.servers.len() > MAX_TRUST_SERVERS {
            let oldest = self
                .file
                .servers
                .iter()
                .min_by_key(|(_, entry)| entry.approved_at)
                .map(|(name, _)| name.clone());
            match oldest {
                Some(name) => {
                    self.file.servers.remove(&name);
                }
                None => break,
            }
        }
        self.persist()
    }

    fn fail_closed(&mut self, diagnostic: String) {
        self.writable = false;
        self.diagnostic = Some(diagnostic);
    }

    fn persist(&self) -> Result<(), String> {
        let directory = self
            .path
            .parent()
            .ok_or("MCP trust store has no parent directory")?;
        match fs::create_dir_all(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot create MCP trust directory {}: {error}",
                    directory.display()
                ));
            }
        }
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| format!("cannot inspect MCP trust directory: {error}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("MCP trust directories must be real directories, not symlinks".into());
        }
        set_private_dir(directory)?;
        if let Some(root) = directory.parent() {
            set_private_dir(root)?;
        }

        let mut payload = serde_json::to_vec_pretty(&self.file)
            .map_err(|error| format!("cannot serialize MCP trust store: {error}"))?;
        payload.push(b'\n');
        if payload.len() as u64 > MAX_TRUST_FILE_BYTES {
            return Err(format!(
                "MCP trust store exceeds {MAX_TRUST_FILE_BYTES} bytes"
            ));
        }

        let mut temp = tempfile::NamedTempFile::new_in(directory).map_err(|error| {
            format!(
                "cannot create MCP trust tempfile in {}: {error}",
                directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("cannot set MCP trust mode 0600: {error}"))?;
        }
        temp.write_all(&payload)
            .map_err(|error| format!("cannot write MCP trust store: {error}"))?;
        temp.as_file()
            .sync_all()
            .map_err(|error| format!("cannot sync MCP trust store: {error}"))?;
        temp.persist(&self.path).map_err(|error| {
            format!(
                "cannot persist MCP trust store to {}: {error}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

/// The `mcp-trust/` root inside a configuration directory.
pub fn trust_root(trusted_config_dir: &Path) -> PathBuf {
    trusted_config_dir.join(MCP_TRUST_ROOT_DIR)
}

/// Full trust-file path for a workspace and configuration directory.
pub fn trust_file_path(workspace: &Path, trusted_config_dir: &Path) -> PathBuf {
    let workspace = canonical_workspace(workspace);
    trust_root(trusted_config_dir)
        .join(workspace_key(&workspace))
        .join(MCP_TRUST_FILE)
}

/// Per-workspace directory name: SHA-256 over the canonical workspace path.
fn workspace_key(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
    hex(digest)
}

/// Whether `path` resolves to `ancestor` or a descendant of it.
///
/// Missing path components are resolved against the longest existing ancestor,
/// so a not-yet-created trust directory is still compared correctly.
fn path_is_within(path: &Path, ancestor: &Path) -> bool {
    canonicalize_lenient(path).starts_with(canonicalize_lenient(ancestor))
}

fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return resolved;
            }
            Err(_) => match current.parent() {
                Some(parent) => {
                    if let Some(name) = current.file_name() {
                        suffix.push(name.to_os_string());
                    }
                    current = parent;
                }
                None => return path.to_path_buf(),
            },
        }
    }
}

/// Canonical workspace root, falling back to the given path when it cannot be
/// resolved. Two spellings of the same directory therefore share trust, while a
/// dropped or unreadable path stays isolated instead of silently trusting `/`.
pub fn canonical_workspace(workspace: &Path) -> PathBuf {
    fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// A stable, credential-free fingerprint of one MCP server in one workspace.
///
/// It binds the source workspace, server name, command, argument order and the
/// full environment. Length-prefixed fields keep the pre-image unambiguous, so
/// no arrangement of arguments or environment values can collide with another
/// configuration. The digest itself never reveals the environment values.
pub fn server_fingerprint(workspace: &Path, server: &str, spec: &McpServerSpec) -> String {
    let workspace = canonical_workspace(workspace);
    let mut hasher = Sha256::new();
    field(&mut hasher, b"schema", b"mio-mcp-trust-v1");
    field(
        &mut hasher,
        b"workspace",
        workspace.to_string_lossy().as_bytes(),
    );
    field(&mut hasher, b"server", server.as_bytes());
    field(&mut hasher, b"command", spec.command.as_bytes());
    for (index, argument) in spec.args.iter().enumerate() {
        let label = format!("arg{index}");
        field(&mut hasher, label.as_bytes(), argument.as_bytes());
    }
    for (key, value) in &spec.env {
        field(&mut hasher, b"env-key", key.as_bytes());
        field(&mut hasher, b"env-value", value.as_bytes());
    }
    hex(hasher.finalize())
}

/// Human-readable approval target for a project MCP server.
///
/// Environment *values* are never shown: they routinely carry credentials and
/// the fingerprint already binds them. The digest prefix lets a user tell two
/// revisions of the same command apart.
pub fn server_display_target(
    workspace: &Path,
    server: &str,
    spec: &McpServerSpec,
    fingerprint: &str,
) -> String {
    let env_keys: Vec<&str> = spec.env.keys().map(String::as_str).collect();
    let digest: String = fingerprint.chars().take(16).collect();
    let mut display = format!(
        "server={server}; command={}; args=[{}]; env=[{}]; workspace={}; fingerprint={digest}",
        spec.command,
        spec.args.join(" "),
        env_keys.join(","),
        canonical_workspace(workspace).display()
    );
    if display.chars().count() > MAX_DISPLAY_CHARS {
        display = display.chars().take(MAX_DISPLAY_CHARS).collect();
        display.push_str("...");
    }
    display
}

fn field(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_TRUST_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_TRUST_FILE_BYTES {
        return Err(format!(
            "{} exceeds {MAX_TRUST_FILE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn set_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot set MCP trust directory mode 0700: {error}"))
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(command: &str, env: &[(&str, &str)]) -> McpServerSpec {
        McpServerSpec {
            command: command.to_owned(),
            args: vec!["-c".into(), "true".into()],
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>(),
            disabled: false,
            required: None,
            optional: None,
        }
    }

    #[test]
    fn fingerprint_binds_workspace_command_args_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let base = spec("sh", &[("TOKEN", "a")]);
        let fingerprint = server_fingerprint(dir.path(), "srv", &base);
        assert_eq!(fingerprint.len(), 64);
        assert_eq!(
            fingerprint,
            server_fingerprint(dir.path(), "srv", &base.clone())
        );
        // Same command in another workspace is a different trust.
        assert_ne!(fingerprint, server_fingerprint(other.path(), "srv", &base));
        // Environment values and argument order both change the fingerprint.
        assert_ne!(
            fingerprint,
            server_fingerprint(dir.path(), "srv", &spec("sh", &[("TOKEN", "b")]))
        );
        let mut reordered = base.clone();
        reordered.args = vec!["true".into(), "-c".into()];
        assert_ne!(
            fingerprint,
            server_fingerprint(dir.path(), "srv", &reordered)
        );
    }

    #[test]
    fn display_target_does_not_leak_environment_values() {
        let dir = tempfile::tempdir().unwrap();
        let base = spec("sh", &[("SECRET_TOKEN", "super-secret-value")]);
        let fingerprint = server_fingerprint(dir.path(), "srv", &base);
        let display = server_display_target(dir.path(), "srv", &base, &fingerprint);
        assert!(display.contains("SECRET_TOKEN"));
        assert!(display.contains("fingerprint="));
        assert!(!display.contains("super-secret-value"));
    }

    #[test]
    fn store_round_trips_and_rejects_changed_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut store = McpTrustStore::load_in(dir.path(), config.path());
        assert!(!store.is_trusted("srv", "aaa"));
        store.record("srv", "aaa").unwrap();
        let reloaded = McpTrustStore::load_in(dir.path(), config.path());
        assert!(reloaded.is_trusted("srv", "aaa"));
        assert!(!reloaded.is_trusted("srv", "bbb"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let workspace_dir = reloaded.path().parent().unwrap();
            let dir_mode = fs::metadata(workspace_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(reloaded.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn trust_store_lives_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut store = McpTrustStore::load_in(dir.path(), config.path());
        store.record("srv", "aaa").unwrap();
        assert!(
            store.path().starts_with(config.path()),
            "trust must live under the user config directory"
        );
        assert!(
            !store.path().starts_with(dir.path()),
            "trust must never live inside the workspace"
        );
        // The repository-local legacy path is untouched.
        assert!(!dir.path().join(".mio").exists());
    }

    #[test]
    fn project_controlled_trust_directory_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        // A host that (mistakenly) points the store inside the project must be
        // refused: a malicious repository could otherwise pre-seed it.
        let mut store = McpTrustStore::load_in(dir.path(), &dir.path().join(".mio"));
        assert!(store.diagnostic().is_some());
        assert!(!store.is_trusted("srv", "aaa"));
        assert!(store.record("srv", "aaa").is_err());
    }

    #[test]
    fn corrupt_store_trusts_nothing_but_can_be_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let path = trust_file_path(dir.path(), config.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        let mut store = McpTrustStore::load_in(dir.path(), config.path());
        assert!(store.diagnostic().is_some());
        assert!(!store.is_trusted("srv", "aaa"));
        store.record("srv", "aaa").unwrap();
        assert!(McpTrustStore::load_in(dir.path(), config.path()).is_trusted("srv", "aaa"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_store_fails_closed_and_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let path = trust_file_path(dir.path(), config.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let mut store = McpTrustStore::load_in(dir.path(), config.path());
        assert!(store.diagnostic().is_some());
        assert!(store.record("srv", "aaa").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched".to_vec());
    }

    #[test]
    fn source_workspace_is_bound_into_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut store = McpTrustStore::load_in(dir.path(), config.path());
        // A fingerprint recorded for another workspace never matches.
        store.file.version = MCP_TRUST_VERSION;
        store.file.servers.insert(
            "srv".into(),
            McpTrustEntry {
                fingerprint: "aaa".into(),
                workspace: "/somewhere/else".into(),
                approved_at: 1,
            },
        );
        assert!(!store.is_trusted("srv", "aaa"));
    }
}
