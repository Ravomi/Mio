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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub cwd: PathBuf,
    pub preamble: String,
    pub skills: Vec<SkillSummary>,
}

impl ProjectContext {
    pub fn load(cwd: &Path) -> Result<Self, String> {
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

        let mut ancestors = Vec::new();
        let mut curr: Option<&Path> = Some(&canonical_cwd);
        while let Some(dir) = curr {
            ancestors.push(dir);
            curr = dir.parent();
        }
        ancestors.reverse();

        for dir in ancestors {
            let candidate = dir.join("AGENTS.md");
            if let Some(content) = read_optional(&candidate)? {
                append_preamble(
                    &mut preamble,
                    &mut total_preamble_bytes,
                    &format!("--- AGENTS.md from {} ---\n", candidate.display()),
                    &content,
                )?;
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

        let skills = scan_skills(&canonical_cwd)?;
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
        let canonical_tmpl =
            crate::checked_path(&self.cwd, &format!(".mio/prompts/{name}.md"), false)?;
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

fn read_bounded_utf8(path: &Path, max_bytes: usize) -> Result<String, String> {
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
    let scan_dirs = [
        PathBuf::from(".agents").join("skills"),
        PathBuf::from(".mio").join("skills"),
    ];
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
