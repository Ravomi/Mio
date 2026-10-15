use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_TOTAL_PREAMBLE: usize = 128 * 1024;
const MAX_FILE_READ: usize = 32 * 1024;
const MAX_SKILLS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub relative_path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextOptions {
    pub no_context_files: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub cwd: PathBuf,
    pub preamble: String,
    pub skills: Vec<SkillSummary>,
}

impl ProjectContext {
    pub fn load(cwd: &Path) -> Result<Self, String> {
        Self::load_with_options(cwd, &ContextOptions::default())
    }

    pub fn load_with_options(cwd: &Path, options: &ContextOptions) -> Result<Self, String> {
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize cwd {}: {e}", cwd.display()))?;
        if !canonical_cwd.is_dir() {
            return Err(format!(
                "cwd is not a directory: {}",
                canonical_cwd.display()
            ));
        }

        let mut preamble = String::new();
        let mut total_preamble_bytes = 0usize;

        if !options.no_context_files {
            let mut ancestors = Vec::new();
            let mut curr: Option<&Path> = Some(&canonical_cwd);
            while let Some(dir) = curr {
                ancestors.push(dir);
                curr = dir.parent();
            }
            ancestors.reverse();

            for dir in ancestors {
                for filename in ["AGENTS.md", "CLAUDE.md"] {
                    let candidate = dir.join(filename);
                    if let Some(content) = read_optional(&candidate)? {
                        append_preamble(
                            &mut preamble,
                            &mut total_preamble_bytes,
                            &format!("--- {filename} from {} ---\n", candidate.display()),
                            &content,
                        )?;
                    }
                }
            }

            let system_md = canonical_cwd.join(".mio").join("SYSTEM.md");
            if system_md.try_exists().map_err(|e| e.to_string())? {
                let system_md = crate::checked_path(&canonical_cwd, ".mio/SYSTEM.md", false)?;
                let content = read_bounded_utf8(&system_md, MAX_FILE_READ)?;
                append_preamble(
                    &mut preamble,
                    &mut total_preamble_bytes,
                    &format!("--- SYSTEM.md from {} ---\n", system_md.display()),
                    &content,
                )?;
            }

            for plugin in crate::plugin::discover_plugins(&canonical_cwd)? {
                for ctx_rel in &plugin.manifest.context_files {
                    let ctx_path =
                        crate::plugin::validate_relative_subpath(&plugin.root_dir, ctx_rel)?;
                    if let Some(content) = read_optional(&ctx_path)? {
                        append_preamble(
                            &mut preamble,
                            &mut total_preamble_bytes,
                            &format!("--- Plugin {} ({ctx_rel}) ---\n", plugin.manifest.name),
                            &content,
                        )?;
                    }
                }
            }
        }

        if let Some(ref sys) = options.system_prompt {
            let resolved = resolve_prompt_input(&canonical_cwd, sys)?;
            append_preamble(
                &mut preamble,
                &mut total_preamble_bytes,
                "--- Custom System Prompt ---\n",
                &resolved,
            )?;
        }

        if let Some(ref append) = options.append_system_prompt {
            let resolved = resolve_prompt_input(&canonical_cwd, append)?;
            append_preamble(
                &mut preamble,
                &mut total_preamble_bytes,
                "--- Appended System Prompt ---\n",
                &resolved,
            )?;
        }

        let skills = if options.no_skills {
            Vec::new()
        } else {
            scan_skills(&canonical_cwd)?
        };
        if !skills.is_empty() {
            append_preamble(
                &mut preamble,
                &mut total_preamble_bytes,
                "--- Project skills ---\n",
                "Read the corresponding SKILL.md with workspace read when a skill is relevant.",
            )?;
            for skill in &skills {
                append_preamble(
                    &mut preamble,
                    &mut total_preamble_bytes,
                    "",
                    &format!(
                        "{}: {} [{}]",
                        skill.name,
                        skill.description,
                        skill.relative_path.display()
                    ),
                )?;
            }
        }
        Ok(Self {
            cwd: canonical_cwd,
            preamble,
            skills,
        })
    }

    pub fn select_skill(&self, name: &str) -> Result<String, String> {
        let summary = self
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("Skill not found: {name}"))?;
        let canonical_skill =
            crate::checked_path(&self.cwd, &summary.relative_path.to_string_lossy(), false)?;
        read_bounded_utf8(&canonical_skill, MAX_FILE_READ)
    }

    pub fn template(&self, name: &str, args: &str) -> Result<String, String> {
        validate_name(name)?;
        let mut candidate_rel = format!(".mio/prompts/{name}.md");
        if !self.cwd.join(&candidate_rel).exists()
            && let Ok(plugins) = crate::plugin::discover_plugins(&self.cwd)
        {
            for plugin in plugins {
                for prompt_dir in &plugin.manifest.prompts {
                    let rel = format!(
                        ".mio/plugins/{}/{}/{name}.md",
                        plugin.manifest.name, prompt_dir
                    );
                    if self.cwd.join(&rel).is_file() {
                        candidate_rel = rel;
                        break;
                    }
                }
            }
        }
        let canonical_tmpl = crate::checked_path(&self.cwd, &candidate_rel, false)?;
        let content = read_bounded_utf8(&canonical_tmpl, MAX_FILE_READ)?;
        let count = content.matches("$ARGUMENTS").count();
        let expanded_len = content
            .len()
            .saturating_sub(count * "$ARGUMENTS".len())
            .saturating_add(count.saturating_mul(args.len()));
        if expanded_len > MAX_TOTAL_PREAMBLE {
            return Err("Rendered template exceeds maximum size limit".to_string());
        }
        Ok(content.replace("$ARGUMENTS", args))
    }
}

fn resolve_prompt_input(canonical_cwd: &Path, input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if !trimmed.is_empty()
        && !trimmed.contains('\n')
        && let Ok(path) = crate::checked_path(canonical_cwd, trimmed, false)
        && path.is_file()
    {
        return read_bounded_utf8(&path, MAX_FILE_READ);
    }
    Ok(input.to_owned())
}

/// Expand `@file` and `@file:start-end` references inside a prompt relative to `cwd`.
/// Tokens that do not correspond to files inside `cwd` (such as `@username` or decorators)
/// are left unchanged unless the token explicitly starts with `@./` or `@/` or fails line-range bounds.
pub fn expand_at_files(cwd: &Path, prompt: &str) -> Result<String, String> {
    if !prompt.contains('@') {
        return Ok(prompt.to_owned());
    }
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize cwd {}: {e}", cwd.display()))?;
    let mut attachments = String::new();
    let mut total_bytes = prompt.len();

    for token in prompt.split_whitespace() {
        let Some(raw_ref) = token.strip_prefix('@') else {
            continue;
        };
        let clean_ref = raw_ref.trim_end_matches([',', '.', ';', ')', ']', ':']);
        if clean_ref.is_empty() {
            continue;
        }
        let (path_str, line_range) = parse_file_and_range(clean_ref);
        let Ok(resolved) = crate::checked_path(&canonical_cwd, path_str, false) else {
            if clean_ref.starts_with("./") || clean_ref.starts_with("../") {
                return Err(format!("Invalid @file reference '@{clean_ref}'"));
            }
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&resolved) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let content = read_bounded_utf8(&resolved, MAX_FILE_READ)?;
        let selected = if let Some((start, end)) = line_range {
            let lines: Vec<&str> = content.lines().collect();
            if start == 0 || start > end || start > lines.len().max(1) {
                return Err(format!(
                    "Invalid line range {start}-{end} for @{path_str} ({} lines)",
                    lines.len()
                ));
            }
            let clamp_end = end.min(lines.len());
            lines[start - 1..clamp_end].join("\n")
        } else {
            content
        };
        let header = match line_range {
            Some((s, e)) => format!("\n\n--- Referenced @{path_str}:{s}-{e} ---\n"),
            None => format!("\n\n--- Referenced @{path_str} ---\n"),
        };
        let addition = header.len() + selected.len() + 1;
        if total_bytes + addition > MAX_TOTAL_PREAMBLE {
            return Err(format!(
                "Expanded @file references exceed maximum size limit of {MAX_TOTAL_PREAMBLE} bytes"
            ));
        }
        attachments.push_str(&header);
        attachments.push_str(&selected);
        if !selected.ends_with('\n') {
            attachments.push('\n');
        }
        total_bytes += addition;
    }

    if attachments.is_empty() {
        Ok(prompt.to_owned())
    } else {
        Ok(format!("{prompt}{attachments}"))
    }
}

fn parse_file_and_range(spec: &str) -> (&str, Option<(usize, usize)>) {
    if let Some((path, range_str)) = spec.rsplit_once(':') {
        if let Some((s, e)) = range_str.split_once('-') {
            if let (Ok(start), Ok(end)) = (s.parse::<usize>(), e.parse::<usize>()) {
                return (path, Some((start, end)));
            }
        } else if let Ok(line) = range_str.parse::<usize>() {
            return (path, Some((line, line)));
        }
    }
    (spec, None)
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid name: '{name}', only alphanumeric, '-' and '_' allowed"
        ));
    }
    Ok(())
}

pub(crate) fn read_bounded_utf8(path: &Path, max_bytes: usize) -> Result<String, String> {
    if !std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err(format!("Not a regular file: {}", path.display()));
    }
    let mut file =
        File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    let mut buf = Vec::new();
    let read_bytes = file
        .by_ref()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if read_bytes > max_bytes {
        return Err(format!(
            "File {} exceeds maximum allowed limit of {} bytes",
            path.display(),
            max_bytes
        ));
    }
    String::from_utf8(buf)
        .map_err(|e| format!("File {} contains invalid UTF-8: {e}", path.display()))
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::metadata(path) {
        Ok(_) => read_bounded_utf8(path, MAX_FILE_READ).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn append_preamble(
    preamble: &mut String,
    total_bytes: &mut usize,
    header: &str,
    content: &str,
) -> Result<(), String> {
    let addition_len = header.len() + content.len() + 1;
    if *total_bytes + addition_len > MAX_TOTAL_PREAMBLE {
        return Err(format!(
            "Preamble total size exceeds limit of {MAX_TOTAL_PREAMBLE} bytes"
        ));
    }
    preamble.push_str(header);
    preamble.push_str(content);
    if !content.ends_with('\n') {
        preamble.push('\n');
    }
    *total_bytes += addition_len;
    Ok(())
}

fn scan_skills(cwd: &Path) -> Result<Vec<SkillSummary>, String> {
    let mut scan_dirs = vec![
        PathBuf::from(".agents").join("skills"),
        PathBuf::from(".mio").join("skills"),
        PathBuf::from(".claude").join("skills"),
    ];
    if let Ok(plugins) = crate::plugin::discover_plugins(cwd) {
        for plugin in plugins {
            for skill_dir in &plugin.manifest.skills {
                scan_dirs.push(
                    PathBuf::from(".mio")
                        .join("plugins")
                        .join(&plugin.manifest.name)
                        .join(skill_dir),
                );
            }
        }
    }
    let mut skill_map: BTreeMap<String, SkillSummary> = BTreeMap::new();
    let mut count = 0;
    let mut visited = 0;

    for rel_base in &scan_dirs {
        let full_base = cwd.join(rel_base);
        if !full_base.try_exists().map_err(|e| e.to_string())? {
            continue;
        }
        let base = crate::checked_path(cwd, &rel_base.to_string_lossy(), false)?;
        let read_dir = std::fs::read_dir(base).map_err(|e| e.to_string())?;

        for entry in read_dir {
            visited += 1;
            if visited > 1000 {
                return Err("Skill discovery exceeds 1000 directory entries".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            let file_type = entry.file_type().map_err(|e| e.to_string())?;
            if !file_type.is_dir() {
                continue;
            }

            let skill_file = entry.path().join("SKILL.md");
            let metadata = match std::fs::symlink_metadata(&skill_file) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.to_string()),
            };
            if !metadata.is_file() {
                continue;
            }

            let skill_content = read_bounded_utf8(&skill_file, MAX_FILE_READ)?;
            let (name, description) =
                parse_frontmatter(&skill_content, &entry.file_name().to_string_lossy())?;
            validate_name(&name)?;

            let rel_path = skill_file
                .strip_prefix(cwd)
                .map_err(|e| format!("Failed strip cwd: {e}"))?
                .to_path_buf();
            if skill_map.contains_key(&name) {
                return Err(format!("Duplicate skill name detected: '{name}'"));
            }
            count += 1;
            if count > MAX_SKILLS {
                return Err(format!("Skill count exceeds limit of {MAX_SKILLS}"));
            }
            skill_map.insert(
                name.clone(),
                SkillSummary {
                    name,
                    description,
                    relative_path: rel_path,
                },
            );
        }
    }
    Ok(skill_map.into_values().collect())
}

fn parse_frontmatter(content: &str, fallback: &str) -> Result<(String, String), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((fallback.to_string(), String::new()));
    }
    let (mut name, mut desc, mut in_fm) = (None, None, false);
    for line in trimmed.lines() {
        let line_trimmed = line.trim();
        if line_trimmed == "---" {
            if in_fm {
                break;
            } else {
                in_fm = true;
                continue;
            }
        }
        if in_fm && let Some((k, v)) = line_trimmed.split_once(':') {
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if k.trim() == "name" {
                name = Some(val);
            } else if k.trim() == "description" {
                desc = Some(val);
            }
        }
    }
    Ok((
        name.unwrap_or_else(|| fallback.to_string()),
        desc.unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_load_ancestor_order_and_system_md() {
        let root = tempdir().unwrap();
        let parent = root.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.path().join("AGENTS.md"), "root agents\n").unwrap();
        fs::write(parent.join("AGENTS.md"), "parent agents\n").unwrap();
        fs::write(child.join("AGENTS.md"), "child agents\n").unwrap();
        fs::create_dir_all(child.join(".mio")).unwrap();
        fs::write(
            child.join(".mio").join("SYSTEM.md"),
            "system instructions\n",
        )
        .unwrap();

        let ctx = ProjectContext::load(&child).unwrap();
        let (r, p, c, s) = (
            ctx.preamble.find("root agents").unwrap(),
            ctx.preamble.find("parent agents").unwrap(),
            ctx.preamble.find("child agents").unwrap(),
            ctx.preamble.find("system instructions").unwrap(),
        );
        assert!(r < p && p < c && c < s);
    }

    #[test]
    fn test_claude_md_context_options_and_at_file_expansion() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "claude rule\n").unwrap();
        fs::write(
            dir.path().join("sample.rs"),
            "line 1\nline 2\nline 3\nline 4\n",
        )
        .unwrap();

        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert!(ctx.preamble.contains("claude rule"));

        let no_ctx = ProjectContext::load_with_options(
            dir.path(),
            &ContextOptions {
                no_context_files: true,
                system_prompt: Some("custom sys".into()),
                append_system_prompt: Some("extra sys".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!no_ctx.preamble.contains("claude rule"));
        assert!(no_ctx.preamble.contains("custom sys"));
        assert!(no_ctx.preamble.contains("extra sys"));

        let expanded =
            expand_at_files(dir.path(), "Please inspect @sample.rs:2-3 and @not_a_file").unwrap();
        assert!(expanded.contains("--- Referenced @sample.rs:2-3 ---"));
        assert!(expanded.contains("line 2\nline 3"));
        assert!(!expanded.contains("line 1"));
    }

    #[test]
    fn test_file_read_limits() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), vec![b'a'; MAX_FILE_READ + 1]).unwrap();
        assert!(
            ProjectContext::load(dir.path())
                .unwrap_err()
                .contains("exceeds maximum allowed limit")
        );
    }

    #[test]
    fn test_skills_and_symlink_escape() {
        let (dir, outside) = (tempdir().unwrap(), tempdir().unwrap());
        let skill_dir = dir.path().join(".mio").join("skills").join("calc");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: calc\ndescription: Calc skill\n---\nbody",
        )
        .unwrap();

        let outside_skill = outside.path().join("evil");
        fs::create_dir_all(&outside_skill).unwrap();
        fs::write(outside_skill.join("SKILL.md"), "evil").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &outside_skill,
            dir.path().join(".mio").join("skills").join("evil_link"),
        )
        .unwrap();

        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.skills.len(), 1);
        assert_eq!(ctx.skills[0].name, "calc");
        assert!(ctx.select_skill("calc").unwrap().contains("body"));
        assert!(ctx.preamble.contains(".mio/skills/calc/SKILL.md"));
        assert!(!ctx.preamble.contains("---\nbody"));
    }

    #[test]
    fn test_duplicate_skills_error() {
        let dir = tempdir().unwrap();
        let (s1, s2) = (
            dir.path().join(".agents/skills/dup"),
            dir.path().join(".mio/skills/dup"),
        );
        fs::create_dir_all(&s1).unwrap();
        fs::create_dir_all(&s2).unwrap();
        fs::write(s1.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();
        fs::write(s2.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();
        assert!(
            ProjectContext::load(dir.path())
                .unwrap_err()
                .contains("Duplicate skill name")
        );
    }

    #[test]
    fn test_template_substitution() {
        let dir = tempdir().unwrap();
        let prompts = dir.path().join(".mio/prompts");
        fs::create_dir_all(&prompts).unwrap();
        fs::write(prompts.join("greeting.md"), "Hello, $ARGUMENTS!").unwrap();

        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.template("greeting", "World").unwrap(), "Hello, World!");
        assert!(ctx.template("../evil", "test").is_err());
        assert!(
            ctx.template("greeting", &"x".repeat(MAX_TOTAL_PREAMBLE))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn template_directory_cannot_escape() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(dir.path().join(".mio")).unwrap();
        fs::write(outside.path().join("review.md"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".mio/prompts")).unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert!(ctx.template("review", "").is_err());
    }
}
