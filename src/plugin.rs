//! Mio Plugin Standard v1 (`mio-plugin.json`), Plugin Manager, Validator, Scaffolder,
//! Community Registry (`Ravomi/mio-plugins`), and Declarative Plugin Tools.

use crate::mcp::McpServerSpec;
use crate::pattern::ToolFilter;
use crate::unknown_effect::{UnknownReason, UnknownSideEffect};
use crate::workspace::workspace_capability::{NodeKind, Root, StatKind};
use crate::{AgentEvent, Host};
use mio_core::{
    message::ToolName,
    tool::{DynamicTool, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

pub const CURRENT_PLUGIN_SCHEMA_VERSION: u32 = 1;
pub const COMMUNITY_REPO_URL: &str = "https://github.com/Ravomi/mio-plugins.git";
pub const COMMUNITY_WEB_URL: &str = "https://github.com/Ravomi/mio-plugins";

/// Hard limit for `mio-plugin.json` and plugin MCP configuration files.
const MAX_PLUGIN_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PLUGIN_SKILL_ENTRIES: usize = 1000;
/// Wall-clock budget for one declarative plugin tool invocation.
pub const PLUGIN_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

fn read_plugin_config(root: &Root, relative: &str, display: &Path) -> Result<String, String> {
    let file = root.target(relative)?.open_read()?;
    crate::context::read_bounded_file(file, display, MAX_PLUGIN_CONFIG_BYTES)
}

fn default_skills_dirs() -> Vec<String> {
    vec!["skills".to_string()]
}

fn default_prompts_dirs() -> Vec<String> {
    vec!["prompts".to_string()]
}

fn default_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": true
    })
}

/// Declarative executable tool bundled inside a `mio-plugin.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginToolSpec {
    pub name: String,
    pub description: String,
    #[serde(default = "default_tool_parameters")]
    pub parameters: Value,
    pub command: Vec<String>,
}

/// Official `mio-plugin.json` manifest (Schema v1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_skills_dirs")]
    pub skills: Vec<String>,
    #[serde(default = "default_prompts_dirs")]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub mcp: Option<String>,
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
}

/// Installed and validated plugin on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub root_dir: PathBuf,
}

/// Entry in the community `registry.json` index (`Ravomi/mio-plugins`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub license: String,
    pub repository: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// Community plugin registry index (`registry.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRegistry {
    pub schema_version: u32,
    pub community_url: String,
    pub plugins: Vec<RegistryEntry>,
}

/// Validate a plugin identifier (`[a-z0-9][a-z0-9-_]{0,63}`).
pub fn validate_plugin_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "Invalid plugin name '{name}': length must be 1..=64"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "Invalid plugin name '{name}': must start with lowercase ASCII letter or digit"
        ));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err(format!(
            "Invalid plugin name '{name}': only lowercase alphanumeric, '-' and '_' allowed"
        ));
    }
    Ok(())
}

pub(crate) fn validate_relative_subpath(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("Relative path in plugin manifest cannot be empty".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(format!("Plugin path '{rel}' must be relative"));
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("Plugin path '{rel}' cannot contain '..'")),
        }
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize plugin root {}: {e}", root.display()))?;
    // Inspect every component, not only the last one: a symlinked intermediate
    // directory (for example `linked/context.md` where `linked` points outside
    // the plugin) must not be followed. Missing components end the walk so a
    // declared-but-absent path is still reported by the caller's existence check.
    let mut resolved = canonical_root.clone();
    for comp in p.components() {
        if matches!(comp, Component::CurDir) {
            continue;
        }
        resolved.push(comp.as_os_str());
        match fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "Plugin path '{rel}' must not traverse a symlink ({})",
                    resolved.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("Cannot inspect plugin path '{rel}': {e}")),
        }
    }
    if resolved.exists() {
        let canonical = resolved
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize plugin path '{rel}': {e}"))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "Plugin path '{rel}' escapes the plugin directory {}",
                canonical_root.display()
            ));
        }
    }
    Ok(resolved)
}

/// Validate a plugin directory against the Mio Plugin Standard v1 (`mio-plugin.json`).
pub fn validate_plugin_dir(plugin_dir: &Path) -> Result<PluginManifest, String> {
    if !plugin_dir.is_dir() {
        return Err(format!(
            "Plugin directory does not exist: {}",
            plugin_dir.display()
        ));
    }
    let manifest_path = plugin_dir.join("mio-plugin.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "Missing mio-plugin.json in {}",
            plugin_dir.display()
        ));
    }
    let absolute_dir = if plugin_dir.is_absolute() {
        plugin_dir.to_path_buf()
    } else {
        plugin_dir.canonicalize().map_err(|e| {
            format!(
                "Failed to canonicalize plugin root {}: {e}",
                plugin_dir.display()
            )
        })?
    };
    let root = Root::open(&absolute_dir)?;
    let raw = read_plugin_config(&root, "mio-plugin.json", &manifest_path)?;
    let manifest: PluginManifest = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid mio-plugin.json in {}: {e}", plugin_dir.display()))?;

    if manifest.schema_version != CURRENT_PLUGIN_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported plugin schema_version {} (expected {CURRENT_PLUGIN_SCHEMA_VERSION})",
            manifest.schema_version
        ));
    }
    validate_plugin_name(&manifest.name)?;
    if manifest.version.trim().is_empty() {
        return Err(format!(
            "Plugin '{}' version cannot be empty",
            manifest.name
        ));
    }
    if manifest.description.trim().is_empty() {
        return Err(format!(
            "Plugin '{}' description cannot be empty",
            manifest.name
        ));
    }

    // Validate context_files
    for ctx_rel in &manifest.context_files {
        validate_relative_subpath(plugin_dir, ctx_rel)?;
        if root.target(ctx_rel)?.stat()?.kind != StatKind::File {
            return Err(format!(
                "Plugin '{}' declared missing context_file '{ctx_rel}'",
                manifest.name
            ));
        }
    }

    // Validate skills directories (if they exist)
    let mut visited_skill_entries = 0;
    for skill_dir_rel in &manifest.skills {
        let dir = validate_relative_subpath(plugin_dir, skill_dir_rel)?;
        if dir.exists() {
            let skills_dir = root.dir(skill_dir_rel).map_err(|_| {
                format!(
                    "Plugin '{}' skills path '{skill_dir_rel}' is not a directory",
                    manifest.name
                )
            })?;
            for entry in skills_dir.children()? {
                visited_skill_entries += 1;
                if visited_skill_entries > MAX_PLUGIN_SKILL_ENTRIES {
                    return Err(format!(
                        "Plugin '{}' skills exceed {MAX_PLUGIN_SKILL_ENTRIES} directory entries",
                        manifest.name
                    ));
                }
                let entry = entry?;
                if entry.kind() == NodeKind::Dir {
                    let skill_dir = entry.open_dir()?;
                    if skill_dir.target("SKILL.md")?.stat()?.kind != StatKind::File {
                        return Err(format!(
                            "Plugin '{}' skill directory '{}' is missing SKILL.md",
                            manifest.name,
                            entry.name()
                        ));
                    }
                }
            }
        }
    }

    // Validate prompts directories (if they exist)
    for prompt_dir_rel in &manifest.prompts {
        let dir = validate_relative_subpath(plugin_dir, prompt_dir_rel)?;
        if dir.exists() {
            root.dir(prompt_dir_rel).map_err(|_| {
                format!(
                    "Plugin '{}' prompts path '{prompt_dir_rel}' is not a directory",
                    manifest.name
                )
            })?;
        }
    }

    // Validate mcp file (if declared)
    if let Some(ref mcp_rel) = manifest.mcp {
        let mcp_path = validate_relative_subpath(plugin_dir, mcp_rel)?;
        if root.target(mcp_rel)?.stat()?.kind != StatKind::File {
            return Err(format!(
                "Plugin '{}' declared missing mcp file '{mcp_rel}'",
                manifest.name
            ));
        }
        let mcp_raw = read_plugin_config(&root, mcp_rel, &mcp_path)?;
        let parsed: Value = serde_json::from_str(&mcp_raw).map_err(|e| {
            format!(
                "Plugin '{}' invalid MCP JSON '{mcp_rel}': {e}",
                manifest.name
            )
        })?;
        if !parsed.is_object() {
            return Err(format!(
                "Plugin '{}' MCP config '{mcp_rel}' must be a JSON object",
                manifest.name
            ));
        }
    }

    // Validate declarative tools
    for tool in &manifest.tools {
        validate_plugin_name(&tool.name)?;
        if tool.description.trim().is_empty() {
            return Err(format!(
                "Plugin '{}' tool '{}' description cannot be empty",
                manifest.name, tool.name
            ));
        }
        if tool.command.is_empty() || tool.command[0].trim().is_empty() {
            return Err(format!(
                "Plugin '{}' tool '{}' command cannot be empty",
                manifest.name, tool.name
            ));
        }
        if !tool.parameters.is_object() {
            return Err(format!(
                "Plugin '{}' tool '{}' parameters must be a JSON Schema object",
                manifest.name, tool.name
            ));
        }
    }

    Ok(manifest)
}

/// Validate a community `registry.json` file.
pub fn validate_registry_file(path: &Path) -> Result<PluginRegistry, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let registry: PluginRegistry = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid registry JSON {}: {e}", path.display()))?;
    if registry.schema_version != CURRENT_PLUGIN_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported registry schema_version {} (expected {CURRENT_PLUGIN_SCHEMA_VERSION})",
            registry.schema_version
        ));
    }
    let mut seen = BTreeMap::new();
    for item in &registry.plugins {
        validate_plugin_name(&item.name)?;
        if seen.insert(item.name.clone(), true).is_some() {
            return Err(format!("Duplicate plugin '{}' in registry.json", item.name));
        }
        if item.version.trim().is_empty() || item.description.trim().is_empty() {
            return Err(format!(
                "Registry entry '{}' missing version or description",
                item.name
            ));
        }
    }
    Ok(registry)
}

/// Built-in official community plugin catalog (`Ravomi/mio-plugins`).
pub fn builtin_community_registry() -> PluginRegistry {
    PluginRegistry {
        schema_version: CURRENT_PLUGIN_SCHEMA_VERSION,
        community_url: COMMUNITY_WEB_URL.to_string(),
        plugins: vec![
            RegistryEntry {
                name: "git-workflow".into(),
                version: "0.1.0".into(),
                description: "Git status/diff summary tool, commit message template, and PR review skill".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/git-workflow".into()),
                keywords: vec!["git".into(), "commit".into(), "review".into(), "pr".into()],
            },
            RegistryEntry {
                name: "rust-expert".into(),
                version: "0.1.0".into(),
                description: "Cargo check/clippy/test diagnostic tool, Rust unsafe & ownership audit skill, and benchmark template".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/rust-expert".into()),
                keywords: vec!["rust".into(), "cargo".into(), "clippy".into(), "audit".into()],
            },
            RegistryEntry {
                name: "security-audit".into(),
                version: "0.1.0".into(),
                description: "Workspace secret & supply-chain scanner tool, OWASP code review skill, and threat model prompt".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/security-audit".into()),
                keywords: vec!["security".into(), "audit".into(), "owasp".into(), "secrets".into()],
            },
            RegistryEntry {
                name: "docs-writer".into(),
                version: "0.1.0".into(),
                description: "Technical documentation architect skill and Architecture Decision Record (ADR) prompt template".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/docs-writer".into()),
                keywords: vec!["docs".into(), "adr".into(), "architecture".into(), "markdown".into()],
            },
            RegistryEntry {
                name: "mcp-github".into(),
                version: "0.1.0".into(),
                description: "GitHub CLI & MCP bridge for issues, pull requests, workflow runs, and repository search".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/mcp-github".into()),
                keywords: vec!["github".into(), "mcp".into(), "pr".into(), "issues".into(), "ci".into()],
            },
            RegistryEntry {
                name: "test-runner".into(),
                version: "0.1.0".into(),
                description: "Polyglot test suite detector & runner (Cargo, Pytest, Vitest/Jest, Go), TDD skill, and regression test prompt".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/test-runner".into()),
                keywords: vec!["testing".into(), "tdd".into(), "cargo-test".into(), "pytest".into(), "coverage".into()],
            },
            RegistryEntry {
                name: "perf-profiler".into(),
                version: "0.1.0".into(),
                description: "Binary size, startup latency, RSS memory footprint analyzer, and hot-path optimization skill".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/perf-profiler".into()),
                keywords: vec!["performance".into(), "benchmark".into(), "profiling".into(), "memory".into(), "latency".into()],
            },
            RegistryEntry {
                name: "docker-ops".into(),
                version: "0.1.0".into(),
                description: "Container & Compose status inspection tool, multi-stage Dockerfile hardening skill, and deployment prompt".into(),
                author: "Ravomi Community".into(),
                license: "MIT".into(),
                repository: COMMUNITY_REPO_URL.into(),
                path: Some("plugins/docker-ops".into()),
                keywords: vec!["docker".into(), "containers".into(), "devops".into(), "compose".into(), "ci".into()],
            },
        ],
    }
}

/// Search community registry plugins by optional keyword/substring.
pub fn search_community_plugins(query: Option<&str>) -> Vec<RegistryEntry> {
    let reg = builtin_community_registry();
    let q = query.unwrap_or("").trim().to_ascii_lowercase();
    reg.plugins
        .into_iter()
        .filter(|entry| {
            q.is_empty()
                || entry.name.to_ascii_lowercase().contains(&q)
                || entry.description.to_ascii_lowercase().contains(&q)
                || entry
                    .keywords
                    .iter()
                    .any(|k| k.to_ascii_lowercase().contains(&q))
        })
        .collect()
}

/// Scaffold a new standard-compliant Mio plugin inside `target_dir`.
pub fn scaffold_plugin(target_dir: &Path, name: &str) -> Result<PathBuf, String> {
    validate_plugin_name(name)?;
    fs::create_dir_all(target_dir)
        .map_err(|e| format!("Failed to create {}: {e}", target_dir.display()))?;
    let skill_dir = target_dir.join("skills").join(name);
    let prompts_dir = target_dir.join("prompts");
    fs::create_dir_all(&skill_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&prompts_dir).map_err(|e| e.to_string())?;

    let manifest = PluginManifest {
        schema_version: CURRENT_PLUGIN_SCHEMA_VERSION,
        name: name.to_string(),
        version: "0.1.0".to_string(),
        description: format!("{name} plugin for Mio coding agent"),
        author: Some("Mio Plugin Author".to_string()),
        license: Some("MIT".to_string()),
        repository: Some(COMMUNITY_WEB_URL.to_string()),
        keywords: vec![name.to_string(), "mio-plugin".to_string()],
        skills: vec!["skills".to_string()],
        prompts: vec!["prompts".to_string()],
        context_files: vec!["CONTEXT.md".to_string()],
        mcp: None,
        tools: Vec::new(),
    };
    let manifest_json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    fs::write(
        target_dir.join("mio-plugin.json"),
        format!("{manifest_json}\n"),
    )
    .map_err(|e| e.to_string())?;

    let skill_md = format!(
        "---\nname: {name}\ndescription: Domain knowledge and workflow for {name}\n---\n# {name} Skill\n\nFollow structured verification steps when working on {name} tasks.\n"
    );
    fs::write(skill_dir.join("SKILL.md"), skill_md).map_err(|e| e.to_string())?;

    let prompt_md =
        format!("Execute the `{name}` workflow on the following target:\n\n$ARGUMENTS\n");
    fs::write(prompts_dir.join(format!("{name}.md")), prompt_md).map_err(|e| e.to_string())?;

    let context_md =
        format!("# {name} Plugin Context\n\nThis workspace has the `{name}` Mio plugin enabled.\n");
    fs::write(target_dir.join("CONTEXT.md"), context_md).map_err(|e| e.to_string())?;

    let readme_md = format!(
        "# {name}\n\nA standard-compliant [Mio]({COMMUNITY_WEB_URL}) plugin.\n\n## Install\n\n```bash\nmio --plugin-install ./{name}\n```\n"
    );
    fs::write(target_dir.join("README.md"), readme_md).map_err(|e| e.to_string())?;

    validate_plugin_dir(target_dir)?;
    Ok(target_dir.to_path_buf())
}

/// Discover all installed plugins in `.mio/plugins/*` inside `cwd`.
pub fn discover_plugins(cwd: &Path) -> Result<Vec<InstalledPlugin>, String> {
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize cwd {}: {e}", cwd.display()))?;
    let plugins_dir = canonical_cwd.join(".mio").join("plugins");
    if !plugins_dir.is_dir() {
        return Ok(Vec::new());
    }
    let root = Root::open(&canonical_cwd)?;
    let plugins_dir_handle = root.dir(".mio/plugins")?;
    let mut plugins = BTreeMap::new();
    for entry in plugins_dir_handle.children()? {
        let entry = entry?;
        if entry.kind() != NodeKind::Dir {
            continue;
        }
        let plugin_dir = entry.open_dir()?;
        if plugin_dir.target("mio-plugin.json")?.stat()?.kind != StatKind::File {
            continue;
        }
        let dir = plugins_dir.join(entry.name());
        let manifest = validate_plugin_dir(&dir)?;
        plugins.insert(
            manifest.name.clone(),
            InstalledPlugin {
                manifest,
                root_dir: dir,
            },
        );
    }
    Ok(plugins.into_values().collect())
}

/// Install a plugin into `.mio/plugins/<name>` from a local path, community registry name,
/// `github:owner/repo[#subdir]`, or Git URL.
pub fn install_plugin(cwd: &Path, spec: &str) -> Result<InstalledPlugin, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("Plugin install spec cannot be empty".into());
    }

    // 1. Check if `spec` is an existing local directory
    let candidate_local = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        cwd.join(trimmed)
    };
    if candidate_local.is_dir() && candidate_local.join("mio-plugin.json").is_file() {
        return install_from_local_dir(cwd, &candidate_local);
    }

    // 2. Resolve community registry name or Git/GitHub spec
    let (git_url, subdir) = resolve_remote_spec(trimmed)?;
    let temp = tempfile::tempdir().map_err(|e| format!("Failed to create temp dir: {e}"))?;
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--quiet",
            &git_url,
            temp.path().to_str().unwrap_or("."),
        ])
        .status()
        .map_err(|e| format!("Failed to run git clone: {e}"))?;
    if !clone_status.success() {
        return Err(format!("git clone failed for '{git_url}'"));
    }

    let source_dir = match subdir {
        Some(ref sub) => validate_relative_subpath(temp.path(), sub)?,
        None => temp.path().to_path_buf(),
    };
    install_from_local_dir(cwd, &source_dir)
}

fn resolve_remote_spec(spec: &str) -> Result<(String, Option<String>), String> {
    // Check community registry first
    if let Some(entry) = builtin_community_registry()
        .plugins
        .into_iter()
        .find(|p| p.name == spec)
    {
        return Ok((entry.repository, entry.path));
    }

    let (repo_part, subdir) = match spec.split_once('#') {
        Some((r, s)) => (r, Some(s.to_string())),
        None => (spec, None),
    };

    if let Some(gh) = repo_part.strip_prefix("github:") {
        return Ok((format!("https://github.com/{gh}.git"), subdir));
    }
    if repo_part.starts_with("https://")
        || repo_part.starts_with("http://")
        || repo_part.starts_with("git@")
        || repo_part.starts_with("file://")
    {
        return Ok((repo_part.to_string(), subdir));
    }
    if repo_part.split('/').count() == 2 && !repo_part.starts_with('.') {
        return Ok((format!("https://github.com/{repo_part}.git"), subdir));
    }
    Err(format!(
        "Unknown plugin '{spec}': not a local directory, community plugin name, or GitHub owner/repo"
    ))
}

fn install_from_local_dir(cwd: &Path, source_dir: &Path) -> Result<InstalledPlugin, String> {
    let manifest = validate_plugin_dir(source_dir)?;
    let plugins_root = cwd.join(".mio").join("plugins");
    fs::create_dir_all(&plugins_root)
        .map_err(|e| format!("Failed to create {}: {e}", plugins_root.display()))?;
    let dest_dir = plugins_root.join(&manifest.name);
    if dest_dir.exists() {
        fs::remove_dir_all(&dest_dir)
            .map_err(|e| format!("Failed to replace existing plugin '{}': {e}", manifest.name))?;
    }
    copy_dir_filtered(source_dir, &dest_dir)?;
    let validated = validate_plugin_dir(&dest_dir)?;
    Ok(InstalledPlugin {
        manifest: validated,
        root_dir: dest_dir,
    })
}

fn copy_dir_filtered(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("Failed to create {}: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() {
            return Err(format!(
                "Refusing to copy symlink in plugin: {}",
                entry.path().display()
            ));
        }
        let dest_path = dst.join(&name);
        if ft.is_dir() {
            copy_dir_filtered(&entry.path(), &dest_path)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &dest_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Remove an installed plugin from `.mio/plugins/<name>`.
pub fn remove_plugin(cwd: &Path, name: &str) -> Result<(), String> {
    validate_plugin_name(name)?;
    let target = cwd.join(".mio").join("plugins").join(name);
    if !target.exists() {
        return Err(format!(
            "Plugin '{name}' is not installed in this workspace"
        ));
    }
    fs::remove_dir_all(&target).map_err(|e| format!("Failed to remove plugin '{name}': {e}"))?;
    Ok(())
}

/// Collect MCP server specs declared across all installed plugins.
pub fn load_plugin_mcp_specs(cwd: &Path) -> Result<BTreeMap<String, McpServerSpec>, String> {
    let mut out = BTreeMap::new();
    for plugin in discover_plugins(cwd)? {
        let Some(ref mcp_rel) = plugin.manifest.mcp else {
            continue;
        };
        let mcp_path = validate_relative_subpath(&plugin.root_dir, mcp_rel)?;
        if !mcp_path.is_file() {
            continue;
        }
        let root = Root::open(&plugin.root_dir)?;
        let raw = read_plugin_config(&root, mcp_rel, &mcp_path)?;
        #[derive(Deserialize)]
        struct McpConfig {
            #[serde(rename = "mcpServers", alias = "servers", default)]
            mcp_servers: BTreeMap<String, McpServerSpec>,
        }
        let parsed: McpConfig = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        for (k, v) in parsed.mcp_servers {
            out.insert(format!("{}_{}", plugin.manifest.name, k), v);
        }
    }
    Ok(out)
}

/// Why one declarative plugin tool did not return a normal result.
///
/// The classification is typed and conservative: a supervision timeout means
/// the command may already have run its side effect, so it is `Unknown` and is
/// never reclassified by parsing the timeout message. A non-zero exit is an
/// observed failure (`Failed`) and stays as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PluginToolError {
    /// The command provably failed (could not start, or exited non-zero).
    Failed(String),
    /// The command may have run: its supervision timeout fired, so the effect
    /// cannot be confirmed.
    Unknown(String),
}

impl PluginToolError {
    fn into_message(self) -> String {
        match self {
            Self::Failed(message) | Self::Unknown(message) => message,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Failed(message) | Self::Unknown(message) => message,
        }
    }
}

/// Legacy unrestricted entry point used by the module's tests.
#[cfg(test)]
async fn execute_plugin_command(
    command: &[String],
    args_json: &str,
    cwd: &Path,
) -> Result<String, String> {
    execute_plugin_command_with_timeout(command, args_json, cwd, PLUGIN_TOOL_TIMEOUT).await
}

/// Run a declarative plugin tool unrestricted. Kept for callers without a host
/// policy; the engine uses the policy-aware path below.
#[cfg(test)]
async fn execute_plugin_command_with_timeout(
    command: &[String],
    args_json: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<String, String> {
    execute_plugin_command_under(
        command,
        args_json,
        cwd,
        &crate::execution_backend::ExecutionPolicy::Unrestricted,
        timeout,
    )
    .await
    .map_err(PluginToolError::into_message)
}

/// Run a declarative plugin tool under the host's execution policy.
///
/// The command's argv is preserved verbatim (only `{args}` substitution is
/// applied), `MIO_TOOL_ARGS` is passed as explicit configuration rather than by
/// inheriting the whole host environment into a sandbox, and the child runs in
/// its own process group. It is killed with its descendants when the future is
/// dropped (abort/cancellation), on timeout, or when the pipe closes, and
/// stdout/stderr are captured with a hard 32 KiB per-stream limit.
///
/// When the requested backend cannot run, this fails instead of falling back to
/// an unsandboxed command.
async fn execute_plugin_command_under(
    command: &[String],
    args_json: &str,
    cwd: &Path,
    policy: &crate::execution_backend::ExecutionPolicy,
    timeout: Duration,
) -> Result<String, PluginToolError> {
    use crate::execution_backend::SupervisedCommand;
    if command.is_empty() {
        return Err(PluginToolError::Failed("Empty plugin command".into()));
    }
    let request = command[1..]
        .iter()
        .fold(SupervisedCommand::new(&command[0], cwd), |request, arg| {
            request.arg(arg.replace("{args}", args_json))
        })
        .env("MIO_TOOL_ARGS", args_json);
    let native = policy
        .command(&request)
        .map_err(|error| PluginToolError::Failed(error.to_string()))?;
    let mut cmd = tokio::process::Command::from(native);
    cmd.stdin(std::process::Stdio::null());

    // Typed supervision: a pre-spawn failure is a known `Failed` (nothing ran),
    // while any after-spawn loss (timeout, wait, capture) is `Unknown` because
    // the command may already have run its side effect. The classification is
    // structural, so it is never re-derived by parsing the message.
    let output = match crate::shell::run_supervised_with_timeout(&mut cmd, timeout).await {
        Ok(output) => output,
        Err(crate::shell::SupervisionError::PreSpawn(message)) => {
            return Err(PluginToolError::Failed(message));
        }
        Err(crate::shell::SupervisionError::PostSpawn {
            loss: crate::shell::SupervisionLoss::Timeout,
            ..
        }) => {
            return Err(PluginToolError::Unknown(format!(
                "Plugin tool timed out after {}s",
                timeout.as_secs()
            )));
        }
        Err(crate::shell::SupervisionError::PostSpawn { loss, detail }) => {
            return Err(PluginToolError::Unknown(format!(
                "Plugin tool supervision lost its result ({}): {detail}",
                loss.as_str()
            )));
        }
    };

    let combined = output.combined();
    if output.status.success() {
        Ok(combined)
    } else {
        Err(PluginToolError::Failed(format!(
            "Plugin tool exited with {}: {}",
            output.status,
            combined.trim()
        )))
    }
}

/// Load all declarative `DynamicTool`s from installed plugins in `cwd`.
pub fn load_plugin_tools(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    let policy = host.execution_policy();
    load_plugin_tools_with_policy(cwd, host, filter, policy)
}

/// Load all handlers under one immutable policy snapshot. The sealed MCP/plugin
/// loader shares this same snapshot across every extension in a batch.
pub(crate) fn load_plugin_tools_with_policy(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
    policy: crate::execution_backend::ExecutionPolicy,
) -> Result<Vec<DynamicTool>, String> {
    if filter.is_disabled() {
        return Ok(Vec::new());
    }
    let mut tools: Vec<DynamicTool> = Vec::new();
    // Track declarations independently of the filter so a normalized-name
    // collision is reported instead of one tool silently shadowing another.
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    for plugin in discover_plugins(cwd)? {
        for spec in plugin.manifest.tools {
            let full_name = format!(
                "plugin__{}__{}",
                plugin.manifest.name.replace('-', "_"),
                spec.name.replace('-', "_")
            );
            let declared_by = format!("plugin '{}' tool '{}'", plugin.manifest.name, spec.name);
            if let Some(previous) = owners.insert(full_name.clone(), declared_by.clone()) {
                return Err(format!(
                    "Plugin tool name collision: '{full_name}' is declared by both {previous} and {declared_by}. Rename one of them."
                ));
            }
            // Deny wins across the qualified name and the short declared name.
            if !filter.allows_aliased(&[full_name.as_str(), spec.name.as_str()]) {
                continue;
            }
            let tool_name = ToolName::new(full_name.clone())
                .map_err(|_| format!("Plugin tool name '{full_name}' is empty"))?;
            let command = spec.command.clone();
            let cwd_buf = cwd.to_path_buf();
            let host_clone = host.clone();
            let qualified = full_name.clone();
            let policy = policy.clone();
            let dyn_tool = DynamicTool::new_with_context(
                tool_name,
                spec.description,
                spec.parameters,
                move |ctx, args| {
                    let command = command.clone();
                    let cwd_buf = cwd_buf.clone();
                    let host = host_clone.clone();
                    let qualified = qualified.clone();
                    let policy = policy.clone();
                    Box::pin(async move {
                        let args_json = args.to_string();
                        host.on_event(AgentEvent::ToolStarted {
                            operation: qualified.clone(),
                            target: args_json.clone(),
                        });
                        // Admission (tool filter and approval) happens once at
                        // the run's dispatch boundary, before this handler runs.
                        let outcome = execute_plugin_command_under(
                            &command,
                            &args_json,
                            &cwd_buf,
                            &policy,
                            PLUGIN_TOOL_TIMEOUT,
                        )
                        .await;
                        // A supervision timeout is a typed unknown: the command
                        // may have run its effect. Trip the run fence and record
                        // `Unknown` before the tool-finished event so a durable
                        // host never marks it failed or executed.
                        if let Err(PluginToolError::Unknown(_)) = &outcome {
                            let unknown = UnknownSideEffect::new(
                                qualified.clone(),
                                args_json.clone(),
                                UnknownReason::PluginSupervision,
                                crate::tool_identity::current_tool_call_id(),
                            );
                            let fence = crate::unknown_effect::mark_unknown(ctx, unknown.clone());
                            if host.on_unknown_side_effect(&unknown).is_err()
                                && let Some(fence) = &fence
                            {
                                fence.mark_persist_failed();
                            }
                        }
                        let event_result: Result<String, String> = match &outcome {
                            Ok(output) => Ok(output.clone()),
                            Err(error) => Err(error.message().to_owned()),
                        };
                        host.on_event(AgentEvent::ToolFinished {
                            operation: qualified,
                            target: args_json,
                            result: event_result,
                        });
                        let mut payload = match outcome {
                            Ok(output) => json!({ "output": output }),
                            Err(error) => json!({ "error": error.into_message() }),
                        };
                        if let Some(steer) = host.poll_steering()
                            && !steer.trim().is_empty()
                            && let Some(obj) = payload.as_object_mut()
                        {
                            obj.insert("steering".into(), json!(steer.trim()));
                        }
                        Ok(ToolOutput::json(payload))
                    })
                },
            );
            tools.push(dyn_tool);
        }
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scaffold_validate_install_discover_and_remove_plugin() {
        let src_tmp = tempfile::tempdir().unwrap();
        let plugin_src = src_tmp.path().join("demo-plugin");
        scaffold_plugin(&plugin_src, "demo-plugin").unwrap();

        let manifest = validate_plugin_dir(&plugin_src).unwrap();
        assert_eq!(manifest.name, "demo-plugin");
        assert_eq!(manifest.schema_version, 1);

        let ws_tmp = tempfile::tempdir().unwrap();
        let installed = install_plugin(ws_tmp.path(), plugin_src.to_str().unwrap()).unwrap();
        assert_eq!(installed.manifest.name, "demo-plugin");

        let discovered = discover_plugins(ws_tmp.path()).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].manifest.name, "demo-plugin");

        remove_plugin(ws_tmp.path(), "demo-plugin").unwrap();
        assert!(discover_plugins(ws_tmp.path()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn plugin_config_and_discovery_reject_external_directory_links() {
        use std::os::unix::fs::symlink;

        let (workspace, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let plugin_dir = workspace.path().join(".mio/plugins/demo");
        scaffold_plugin(&plugin_dir, "demo").unwrap();
        let outside_manifest = outside.path().join("mio-plugin.json");
        fs::rename(plugin_dir.join("mio-plugin.json"), &outside_manifest).unwrap();
        symlink(&outside_manifest, plugin_dir.join("mio-plugin.json")).unwrap();
        assert!(validate_plugin_dir(&plugin_dir).is_err());

        fs::remove_file(plugin_dir.join("mio-plugin.json")).unwrap();
        fs::copy(&outside_manifest, plugin_dir.join("mio-plugin.json")).unwrap();
        let plugins_dir = workspace.path().join(".mio/plugins");
        fs::rename(&plugins_dir, workspace.path().join("old-plugins")).unwrap();
        symlink(outside.path(), &plugins_dir).unwrap();
        assert!(discover_plugins(workspace.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plugin_skill_file_link_outside_is_rejected() {
        use std::os::unix::fs::symlink;

        let (plugin, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let plugin_dir = plugin.path().join("demo");
        scaffold_plugin(&plugin_dir, "demo").unwrap();
        let skill_file = plugin_dir.join("skills/demo/SKILL.md");
        fs::remove_file(&skill_file).unwrap();
        let outside_file = outside.path().join("SKILL.md");
        fs::write(&outside_file, "outside").unwrap();
        symlink(&outside_file, &skill_file).unwrap();

        assert!(
            validate_plugin_dir(&plugin_dir)
                .unwrap_err()
                .contains("missing SKILL.md")
        );
    }

    #[test]
    fn plugin_skill_entry_scan_has_a_limit() {
        let plugin = tempfile::tempdir().unwrap();
        let plugin_dir = plugin.path().join("demo");
        scaffold_plugin(&plugin_dir, "demo").unwrap();
        for index in 0..MAX_PLUGIN_SKILL_ENTRIES {
            fs::write(plugin_dir.join(format!("skills/unrelated-{index}")), "").unwrap();
        }

        assert!(
            validate_plugin_dir(&plugin_dir)
                .unwrap_err()
                .contains("skills exceed 1000 directory entries")
        );
    }

    #[test]
    fn test_community_registry_entries_are_valid() {
        let reg = builtin_community_registry();
        assert_eq!(reg.schema_version, 1);
        assert!(reg.plugins.len() >= 4);
        for item in &reg.plugins {
            validate_plugin_name(&item.name).unwrap();
        }
        let rust_matches = search_community_plugins(Some("rust"));
        assert_eq!(rust_matches.len(), 1);
        assert_eq!(rust_matches[0].name, "rust-expert");
    }

    struct TestHost;

    impl Host for TestHost {
        fn shell(&self, _: &str, _: &std::path::Path) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _: AgentEvent) {}
    }

    /// Write `.mio/plugins/<name>/mio-plugin.json` declaring the given tools.
    fn install_plugin_fixture(cwd: &std::path::Path, plugin_name: &str, tools: Value) {
        let dir = cwd.join(".mio").join("plugins").join(plugin_name);
        fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "schema_version": 1,
            "name": plugin_name,
            "version": "0.1.0",
            "description": "fixture plugin",
            "tools": tools,
        });
        fs::write(
            dir.join("mio-plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn tool_spec(name: &str, script: &str) -> Value {
        json!({
            "name": name,
            "description": "fixture tool",
            "parameters": {"type": "object", "properties": {}},
            "command": ["sh", "-c", script],
        })
    }

    fn load_tools(cwd: &std::path::Path, filter: &ToolFilter) -> Result<Vec<String>, String> {
        load_plugin_tools(cwd, Arc::new(TestHost), filter)
            .map(|tools| tools.iter().map(|t| t.name().as_str().to_owned()).collect())
    }

    #[test]
    fn test_plugin_tool_filter_exact_and_deny_priority() {
        let cwd = tempfile::tempdir().unwrap();
        install_plugin_fixture(
            cwd.path(),
            "probe",
            json!([tool_spec("read", "true"), tool_spec("early", "true"),]),
        );

        // Exact qualified name resolves (previously rejected as unsupported).
        let exact = ToolFilter::from_cli(Some("plugin__probe__read"), None, false).unwrap();
        assert_eq!(
            load_tools(cwd.path(), &exact).unwrap(),
            ["plugin__probe__read"]
        );

        // Wildcard exclusion covers the whole plugin tool set.
        let wildcard = ToolFilter::from_cli(None, Some("plugin__probe__read*"), false).unwrap();
        assert_eq!(
            load_tools(cwd.path(), &wildcard).unwrap(),
            ["plugin__probe__early"]
        );

        // Deny on the short alias wins even when the qualified name is allowed.
        let mixed = ToolFilter::from_cli(Some("plugin__probe__read"), Some("read"), false).unwrap();
        assert!(load_tools(cwd.path(), &mixed).unwrap().is_empty());

        // Deny on the qualified name wins even though the short alias is present.
        let deny_qualified =
            ToolFilter::from_cli(None, Some("plugin__probe__read"), false).unwrap();
        assert_eq!(
            load_tools(cwd.path(), &deny_qualified).unwrap(),
            ["plugin__probe__early"]
        );
    }

    #[test]
    fn test_plugin_normalized_tool_name_collision_is_reported() {
        let cwd = tempfile::tempdir().unwrap();
        install_plugin_fixture(cwd.path(), "a-b", json!([tool_spec("c", "true")]));
        install_plugin_fixture(cwd.path(), "a_b", json!([tool_spec("c", "true")]));
        let err = load_tools(cwd.path(), &ToolFilter::all()).unwrap_err();
        assert!(err.contains("collision"), "unexpected error: {err}");
        assert!(err.contains("plugin__a_b__c"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_plugin_output_is_bounded() {
        let cwd = tempfile::tempdir().unwrap();
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "head -c 200000 /dev/zero | tr '\\0' 'a'".to_owned(),
        ];
        let output = execute_plugin_command(&command, "{}", cwd.path())
            .await
            .unwrap();
        assert!(output.contains("TRUNCATED"));
        assert!(
            output.len() < 40 * 1024,
            "output grew to {} bytes",
            output.len()
        );
    }

    /// A supervision timeout, a lost capture and a pre-spawn failure are three
    /// different facts. Only the after-spawn losses are `Unknown`; a rejected
    /// backend/spawn stays a known `Failed`.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_plugin_supervision_failures_are_typed() {
        use crate::execution_backend::ExecutionPolicy;
        let cwd = tempfile::tempdir().unwrap();

        let timed = execute_plugin_command_under(
            &["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
            "{}",
            cwd.path(),
            &ExecutionPolicy::Unrestricted,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(timed, PluginToolError::Unknown(ref message) if message.contains("timed out")),
            "a supervision timeout must be a typed unknown: {timed:?}"
        );

        let captured = execute_plugin_command_under(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "head -c 8388609 /dev/zero".to_owned(),
            ],
            "{}",
            cwd.path(),
            &ExecutionPolicy::Unrestricted,
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(captured, PluginToolError::Unknown(ref message) if message.contains("capture")),
            "a lost capture after spawn must be a typed unknown: {captured:?}"
        );

        let pre_spawn = execute_plugin_command_under(
            &["/mio-no-such-plugin-binary".to_owned()],
            "{}",
            cwd.path(),
            &ExecutionPolicy::Unrestricted,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(pre_spawn, PluginToolError::Failed(_)),
            "a spawn failure must stay a known failure: {pre_spawn:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_plugin_timeout_kills_process_group() {
        let cwd = tempfile::tempdir().unwrap();
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 30 & echo $! > child.pid; wait".to_owned(),
        ];
        let err = execute_plugin_command_with_timeout(
            &command,
            "{}",
            cwd.path(),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "unexpected error: {err}");

        let pid: i32 = fs::read_to_string(cwd.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only probes for existence of a test child.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                #[cfg(target_os = "linux")]
                if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
                    && stat
                        .split_once(") ")
                        .is_some_and(|(_, s)| s.starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("plugin child survived timeout");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_plugin_cancellation_kills_process_group() {
        let cwd = tempfile::tempdir().unwrap();
        install_plugin_fixture(
            cwd.path(),
            "probe",
            json!([tool_spec(
                "late",
                "echo started > started.txt; sleep 30; echo late > after-abort.txt"
            )]),
        );
        let tools = load_plugin_tools(
            cwd.path(),
            Arc::new(TestHost),
            &ToolFilter::from_cli(Some("plugin__probe__late"), None, false).unwrap(),
        )
        .unwrap();
        assert_eq!(tools.len(), 1);
        let tool = tools[0].clone();
        let workdir = cwd.path().to_path_buf();
        let handle = tokio::spawn(async move { tool.execute(json!({})).await });

        let started = cwd.path().join("started.txt");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("plugin tool did not start");

        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());

        // The process group must already be dead: a late write would land here.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!workdir.join("after-abort.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_plugin_context_linked_directory_is_rejected() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("context.md"), "escaped").unwrap();
        let plugin_dir = cwd.path().join("linked-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        std::os::unix::fs::symlink(outside.path(), plugin_dir.join("linked")).unwrap();
        let manifest = json!({
            "schema_version": 1,
            "name": "linked-plugin",
            "version": "0.1.0",
            "description": "fixture plugin",
            "context_files": ["linked/context.md"],
        });
        fs::write(
            plugin_dir.join("mio-plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let err = validate_plugin_dir(&plugin_dir).unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
    }
}
