//! Mio Plugin Standard v1 (`mio-plugin.json`), Plugin Manager, Validator, Scaffolder,
//! Community Registry (`Ravomi/mio-plugins`), and Declarative Plugin Tools.

use crate::mcp::McpServerSpec;
use crate::pattern::ToolFilter;
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

pub const CURRENT_PLUGIN_SCHEMA_VERSION: u32 = 1;
pub const COMMUNITY_REPO_URL: &str = "https://github.com/Ravomi/mio-plugins.git";
pub const COMMUNITY_WEB_URL: &str = "https://github.com/Ravomi/mio-plugins";

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

fn validate_relative_subpath(root: &Path, rel: &str) -> Result<PathBuf, String> {
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
    let joined = root.join(p);
    if let Ok(meta) = fs::symlink_metadata(&joined)
        && meta.file_type().is_symlink()
    {
        return Err(format!("Plugin path '{rel}' cannot be a symlink"));
    }
    Ok(joined)
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
    let raw = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read {}: {e}", manifest_path.display()))?;
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
        let path = validate_relative_subpath(plugin_dir, ctx_rel)?;
        if !path.is_file() {
            return Err(format!(
                "Plugin '{}' declared missing context_file '{ctx_rel}'",
                manifest.name
            ));
        }
    }

    // Validate skills directories (if they exist)
    for skill_dir_rel in &manifest.skills {
        let dir = validate_relative_subpath(plugin_dir, skill_dir_rel)?;
        if dir.exists() {
            if !dir.is_dir() {
                return Err(format!(
                    "Plugin '{}' skills path '{skill_dir_rel}' is not a directory",
                    manifest.name
                ));
            }
            for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                    let skill_md = entry.path().join("SKILL.md");
                    if !skill_md.is_file() {
                        return Err(format!(
                            "Plugin '{}' skill directory '{}' is missing SKILL.md",
                            manifest.name,
                            entry.file_name().to_string_lossy()
                        ));
                    }
                }
            }
        }
    }

    // Validate prompts directories (if they exist)
    for prompt_dir_rel in &manifest.prompts {
        let dir = validate_relative_subpath(plugin_dir, prompt_dir_rel)?;
        if dir.exists() && !dir.is_dir() {
            return Err(format!(
                "Plugin '{}' prompts path '{prompt_dir_rel}' is not a directory",
                manifest.name
            ));
        }
    }

    // Validate mcp file (if declared)
    if let Some(ref mcp_rel) = manifest.mcp {
        let mcp_path = validate_relative_subpath(plugin_dir, mcp_rel)?;
        if !mcp_path.is_file() {
            return Err(format!(
                "Plugin '{}' declared missing mcp file '{mcp_rel}'",
                manifest.name
            ));
        }
        let mcp_raw = fs::read_to_string(&mcp_path).map_err(|e| e.to_string())?;
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
    let plugins_dir = cwd.join(".mio").join("plugins");
    if !plugins_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut plugins = BTreeMap::new();
    for entry in fs::read_dir(&plugins_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let dir = entry.path();
        if !dir.join("mio-plugin.json").is_file() {
            continue;
        }
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
        let mcp_path = plugin.root_dir.join(mcp_rel);
        if !mcp_path.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&mcp_path).map_err(|e| e.to_string())?;
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

async fn execute_plugin_command(
    command: &[String],
    args_json: &str,
    cwd: &Path,
) -> Result<String, String> {
    if command.is_empty() {
        return Err("Empty plugin command".into());
    }
    let mut cmd = tokio::process::Command::new(&command[0]);
    for arg in &command[1..] {
        cmd.arg(arg.replace("{args}", args_json));
    }
    cmd.current_dir(cwd)
        .env("MIO_TOOL_ARGS", args_json)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| "Plugin tool timed out after 60s".to_string())?
        .map_err(|e| format!("Failed to spawn plugin tool '{}': {e}", command[0]))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if output.status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "Plugin tool exited with {}: {}",
            output.status,
            combined.trim()
        ))
    }
}

/// Load all declarative `DynamicTool`s from installed plugins in `cwd`.
pub fn load_plugin_tools(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    if filter.is_disabled() {
        return Ok(Vec::new());
    }
    let mut tools: Vec<DynamicTool> = Vec::new();
    for plugin in discover_plugins(cwd)? {
        for spec in plugin.manifest.tools {
            let full_name = format!(
                "plugin__{}__{}",
                plugin.manifest.name.replace('-', "_"),
                spec.name.replace('-', "_")
            );
            if !filter.allows(&full_name) && !filter.allows(&spec.name) {
                continue;
            }
            let Ok(tool_name) = ToolName::new(full_name.clone()) else {
                continue;
            };
            let command = spec.command.clone();
            let cwd_buf = cwd.to_path_buf();
            let host_clone = host.clone();
            let qualified = full_name.clone();
            let dyn_tool = DynamicTool::new_with_context(
                tool_name,
                spec.description,
                spec.parameters,
                move |_ctx, args| {
                    let command = command.clone();
                    let cwd_buf = cwd_buf.clone();
                    let host = host_clone.clone();
                    let qualified = qualified.clone();
                    Box::pin(async move {
                        let args_json = args.to_string();
                        host.on_event(AgentEvent::ToolStarted {
                            operation: qualified.clone(),
                            target: args_json.clone(),
                        });
                        let outcome = match host.approve_tool(&qualified, &args_json) {
                            Ok(()) => execute_plugin_command(&command, &args_json, &cwd_buf).await,
                            Err(err) => Err(err),
                        };
                        host.on_event(AgentEvent::ToolFinished {
                            operation: qualified,
                            target: args_json,
                            result: outcome.clone(),
                        });
                        let mut payload = match outcome {
                            Ok(output) => json!({ "output": output }),
                            Err(err) => json!({ "error": err }),
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
}
