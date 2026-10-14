use crate::{AgentEvent, Host};
use mio_core::tool::PortableTool;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const MAX_READ_INPUT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB 硬上限
pub const MAX_READ_OUTPUT_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_WRITE_BYTES: usize = 1024 * 1024; // 1 MiB
pub const MAX_GREP_FILE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
pub const MAX_GREP_TOTAL_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_GREP_LINE_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_SEARCH_VISITED_ENTRIES: usize = 5000;
pub const MAX_SEARCH_DEPTH: usize = 64;
pub const MAX_GREP_MATCHES: usize = 500;
pub const MAX_FIND_MATCHES: usize = 500;
pub const MAX_LS_ENTRIES: usize = 1000;
pub const SHELL_TIMEOUT_SECS: u64 = 120;

const IGNORED_NAMES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".idea",
    ".vscode",
    "dist",
    "build",
];

/// Workspace 工具，绑定在指定工作目录并在 Host 上下文中运行。
#[derive(Clone)]
pub struct Workspace {
    pub(crate) cwd: PathBuf,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) read_only: bool,
    pub(crate) subagents: Option<Arc<crate::subagent::SubagentManager>>,
    pub(crate) tool_filter: crate::pattern::ToolFilter,
}

impl Workspace {
    /// 创建一个新的 Workspace 工具实例，校验 cwd 规范化且为目录。
    pub fn new(cwd: impl AsRef<Path>, host: Arc<dyn Host>) -> Result<Self, String> {
        let canonical_cwd = cwd
            .as_ref()
            .canonicalize()
            .map_err(|e| format!("工作目录不可用: {e}"))?;
        if !canonical_cwd.is_dir() {
            return Err("工作目录不是文件夹".into());
        }
        Ok(Self {
            cwd: canonical_cwd,
            host,
            read_only: false,
            subagents: None,
            tool_filter: crate::pattern::ToolFilter::all(),
        })
    }

    /// 创建一个只读的 Workspace 工具实例。
    /// 在工具执行层拒绝 write, edit 和 shell 操作。
    pub fn new_read_only(cwd: impl AsRef<Path>, host: Arc<dyn Host>) -> Result<Self, String> {
        let mut workspace = Self::new(cwd, host)?;
        workspace.read_only = true;
        Ok(workspace)
    }

    /// 配置工具过滤器。
    pub fn with_tool_filter(mut self, tool_filter: crate::pattern::ToolFilter) -> Self {
        self.tool_filter = tool_filter;
        self
    }

    /// 是否为只读模式
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 获取绑定的工作目录绝对路径。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 执行 Workspace 工具操作。
    pub async fn execute(&self, args: Arguments) -> Result<String, String> {
        let op = args.operation.as_str();
        let is_write = matches!(op, "write" | "edit" | "shell" | "bash");
        let _lease = self
            .subagents
            .as_ref()
            .map(|manager| manager.parent_operation(is_write))
            .transpose()?;
        if !self.tool_filter.allows(op) && !self.tool_filter.allows("workspace") {
            return Err(format!("工具过滤策略禁止执行 '{op}' 操作"));
        }
        if !self.tool_filter.allows(op) && self.tool_filter.is_read_only_only() && is_write {
            return Err(format!("工具过滤策略禁止执行 '{op}' 操作"));
        }
        if self.read_only && is_write {
            return Err(format!("只读工作区禁止执行 '{op}' 操作"));
        }
        if is_write {
            self.host.approve_tool(op, &args.target())?;
        }

        let mut output = match op {
            "read" => self.execute_read(args).await?,
            "write" => self.execute_write(args).await?,
            "edit" => self.execute_edit(args).await?,
            "grep" => self.execute_grep(args).await?,
            "find" => self.execute_find(args).await?,
            "ls" => self.execute_ls(args).await?,
            "shell" | "bash" => self.execute_shell(args).await?,
            _ => return Err("未知工具操作".into()),
        };

        if let Some(steering) = self.host.poll_steering() {
            let trimmed = steering.trim();
            if !trimmed.is_empty() {
                output.push_str("\n\n[User Steering Message]: ");
                output.push_str(trimmed);
            }
        }

        Ok(output)
    }

    async fn execute_read(&self, args: Arguments) -> Result<String, String> {
        let path_str = args.path.ok_or("read 操作缺少 path 参数")?;
        let cwd = self.cwd.clone();

        if args.offset == Some(0) {
            return Err("offset 须从 1 开始".into());
        }
        if args.limit == Some(0) {
            return Err("limit 须大于 0".into());
        }

        let offset = args.offset.unwrap_or(1);
        let limit = args.limit;

        tokio::task::spawn_blocking(move || {
            let path = checked_path(&cwd, &path_str, false)?;
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("不支持直接读取符号链接文件".into());
            }
            if !metadata.is_file() {
                return Err("目标不是常规文件".into());
            }

            let file = File::open(&path).map_err(|e| e.to_string())?;
            let mut handle = file.take((MAX_READ_INPUT_BYTES + 1) as u64);
            let mut bytes = Vec::new();
            handle.read_to_end(&mut bytes).map_err(|e| e.to_string())?;

            if bytes.len() > MAX_READ_INPUT_BYTES {
                return Err(format!(
                    "文件超过最大读取上限 ({} 字节 / 8 MiB)，请使用 shell 命令分段处理",
                    MAX_READ_INPUT_BYTES
                ));
            }

            let text = String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 编码文本")?;

            let lines: Vec<&str> = text.split('\n').collect();
            let total_lines = lines.len();

            let start_line_idx = (offset - 1) as usize;
            if start_line_idx >= total_lines {
                if total_lines == 0 || (total_lines == 1 && lines[0].is_empty()) {
                    return Ok(String::new());
                }
                return Err(format!(
                    "offset {offset} 超出文件总行数 {total_lines}"
                ));
            }

            let max_lines = limit.map(|l| l as usize).unwrap_or(usize::MAX);
            let mut out = String::new();
            let mut current_idx = start_line_idx;
            let mut truncated = false;
            let mut hit_limit = false;

            while current_idx < total_lines && (current_idx - start_line_idx) < max_lines {
                let line = lines[current_idx];
                let line_len_with_nl = line.len() + if current_idx + 1 < total_lines { 1 } else { 0 };

                let header_len = format!(
                    "[TRUNCATED: Showing lines {}..{} of {}. Next offset: {}]\n",
                    offset, total_lines, total_lines, total_lines + 1
                )
                .len();
                let budget = MAX_READ_OUTPUT_BYTES.saturating_sub(header_len);

                if out.is_empty() && line_len_with_nl > budget {
                    return Err(format!(
                        "第 {} 行长度 ({} 字节) 超出单次输出上限预算，无法展示完整行，请使用 shell 分段读取",
                        current_idx + 1,
                        line_len_with_nl
                    ));
                }

                if out.len() + line_len_with_nl > budget {
                    truncated = true;
                    break;
                }

                out.push_str(line);
                if current_idx + 1 < total_lines {
                    out.push('\n');
                }
                current_idx += 1;
            }

            if current_idx < total_lines && (current_idx - start_line_idx) >= max_lines {
                hit_limit = true;
            }

            if truncated || hit_limit {
                let next_offset = current_idx + 1;
                let header = format!(
                    "[TRUNCATED: Showing lines {offset}..{current_idx} of {total_lines}. Next offset: {next_offset}]\n"
                );
                Ok(format!("{header}{out}"))
            } else {
                Ok(out)
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_write(&self, args: Arguments) -> Result<String, String> {
        let path_str = args.path.ok_or("write 操作缺少 path 参数")?;
        let value = args
            .value
            .or(args.content)
            .ok_or("write 操作缺少 value 或 content 参数")?;
        if value.len() > MAX_WRITE_BYTES {
            return Err("写入内容超过 1 MiB".into());
        }

        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let path = checked_path(&cwd, &path_str, true)?;
            if let Ok(meta) = std::fs::symlink_metadata(&path)
                && meta.file_type().is_symlink()
            {
                return Err("写入目标不能是符号链接".into());
            }

            let existing_meta = std::fs::symlink_metadata(&path).ok();
            let parent = path.parent().ok_or("无效父目录")?;
            let temp_file = tempfile::NamedTempFile::new_in(parent)
                .map_err(|e| format!("创建临时文件失败: {e}"))?;

            if let Some(ref meta) = existing_meta {
                std::fs::set_permissions(temp_file.path(), meta.permissions())
                    .map_err(|e| format!("设置权限失败: {e}"))?;
            }

            std::fs::write(temp_file.path(), value.as_bytes())
                .map_err(|e| format!("写入文件失败: {e}"))?;

            temp_file
                .persist(&path)
                .map_err(|e| format!("原子持久化文件失败: {e}"))?;

            Ok("文件已写入".into())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_edit(&self, args: Arguments) -> Result<String, String> {
        let path_str = args.path.ok_or("edit 操作缺少 path 参数")?;
        let old_text = args.old_text.ok_or("edit 操作缺少 old_text 参数")?;
        let new_text = args.new_text.ok_or("edit 操作缺少 new_text 参数")?;

        if old_text.is_empty() {
            return Err("old_text 不能为空".into());
        }
        if new_text.len() > MAX_WRITE_BYTES {
            return Err("替换内容超过 1 MiB 上限".into());
        }

        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let path = checked_path(&cwd, &path_str, false)?;
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("不可编辑符号链接".into());
            }
            if !metadata.is_file() {
                return Err("目标不是常规文件".into());
            }

            let permissions = metadata.permissions();
            let file = File::open(&path).map_err(|e| e.to_string())?;
            let mut handle = file.take((MAX_WRITE_BYTES + 1) as u64);
            let mut original_bytes = Vec::new();
            handle
                .read_to_end(&mut original_bytes)
                .map_err(|e| e.to_string())?;

            if original_bytes.len() > MAX_WRITE_BYTES {
                return Err(format!(
                    "文件大小超过 edit 处理上限 ({} 字节 / 1 MiB)",
                    MAX_WRITE_BYTES
                ));
            }

            let original_text =
                String::from_utf8(original_bytes).map_err(|_| "文件不是有效的 UTF-8 编码文本")?;

            let mut match_indices = Vec::new();
            let mut search_start = 0;
            while search_start <= original_text.len() {
                if let Some(found_idx) = original_text[search_start..].find(&old_text) {
                    let abs_idx = search_start + found_idx;
                    match_indices.push(abs_idx);
                    if let Some(next_char) = original_text[abs_idx..].chars().next() {
                        search_start = abs_idx + next_char.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            if match_indices.is_empty() {
                return Err("编辑失败：未找到匹配的 old_text".into());
            }
            if match_indices.len() > 1 {
                return Err(format!(
                    "编辑失败：找到 {} 处匹配的 old_text，存在歧义或重叠，必须唯一匹配",
                    match_indices.len()
                ));
            }

            let match_idx = match_indices[0];
            let start_line = original_text[..match_idx]
                .bytes()
                .filter(|&b| b == b'\n')
                .count()
                + 1;
            let mut modified = String::with_capacity(
                original_text.len() + new_text.len().saturating_sub(old_text.len()),
            );
            modified.push_str(&original_text[..match_idx]);
            modified.push_str(&new_text);
            modified.push_str(&original_text[match_idx + old_text.len()..]);

            if modified.len() > MAX_WRITE_BYTES {
                return Err("编辑后内容超过 1 MiB 上限".into());
            }

            let parent = path.parent().ok_or("无效父目录")?;
            let temp_file = tempfile::NamedTempFile::new_in(parent)
                .map_err(|e| format!("创建临时文件失败: {e}"))?;

            std::fs::set_permissions(temp_file.path(), permissions)
                .map_err(|e| format!("设置权限失败: {e}"))?;

            std::fs::write(temp_file.path(), modified.as_bytes())
                .map_err(|e| format!("写入临时文件失败: {e}"))?;

            temp_file
                .persist(&path)
                .map_err(|e| format!("原子更新文件失败: {e}"))?;

            let diff = crate::pattern::unified_diff(&path_str, start_line, &old_text, &new_text);
            Ok(format!("文件已成功编辑\n{diff}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_grep(&self, args: Arguments) -> Result<String, String> {
        let pattern = args
            .pattern
            .or(args.value)
            .ok_or("grep 操作缺少 pattern 或 value 参数")?;
        let use_regex = args.regex.unwrap_or(false);
        let case_insensitive = args.case_insensitive.unwrap_or(false);
        let matcher = crate::pattern::LineMatcher::new(&pattern, use_regex, case_insensitive)?;

        let cwd = self.cwd.clone();
        let subpath = args.path.unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let target_path = if subpath.trim().is_empty() {
                cwd.clone()
            } else {
                checked_path(&cwd, &subpath, false)?
            };

            let symlink_meta =
                std::fs::symlink_metadata(&target_path).map_err(|e| e.to_string())?;
            if symlink_meta.file_type().is_symlink() {
                return Err("不支持搜索符号链接".into());
            }

            let mut files_to_scan = Vec::new();
            let mut scanned_entries = 0;
            let mut truncated_traversal = false;
            let mut read_errors = 0;

            if symlink_meta.is_file() {
                files_to_scan.push(target_path);
            } else if symlink_meta.is_dir() {
                let mut ctx = TraversalContext {
                    visited: 0,
                    max_entries: MAX_SEARCH_VISITED_ENTRIES,
                    max_depth: MAX_SEARCH_DEPTH,
                    truncated: false,
                };
                collect_files_recursive(&target_path, 0, &mut ctx, &mut files_to_scan)?;
                scanned_entries = ctx.visited;
                truncated_traversal = ctx.truncated;
                files_to_scan.sort();
            } else {
                return Err("目标不是常规文件或目录".into());
            }

            let mut matches = Vec::new();
            let mut hit_match_limit = false;
            let mut total_output_bytes = 0;
            let mut hit_byte_limit = false;

            for file_path in files_to_scan {
                let meta = match std::fs::symlink_metadata(&file_path) {
                    Ok(m) => m,
                    Err(_) => {
                        read_errors += 1;
                        continue;
                    }
                };
                if meta.file_type().is_symlink() || !meta.is_file() {
                    continue;
                }

                let file = match File::open(&file_path) {
                    Ok(f) => f,
                    Err(_) => {
                        read_errors += 1;
                        continue;
                    }
                };
                let mut handle = file.take((MAX_GREP_FILE_BYTES + 1) as u64);
                let mut content = Vec::new();
                if handle.read_to_end(&mut content).is_err() {
                    read_errors += 1;
                    continue;
                }

                if content.len() > MAX_GREP_FILE_BYTES {
                    continue; // 超过 2 MiB 跳过
                }

                let text = match String::from_utf8(content) {
                    Ok(t) => t,
                    Err(_) => continue, // 非 UTF8 跳过
                };

                let rel_path = file_path
                    .strip_prefix(&cwd)
                    .unwrap_or(&file_path)
                    .to_string_lossy();

                let notice_budget: usize = 256;
                let effective_budget = MAX_GREP_TOTAL_BYTES.saturating_sub(notice_budget);

                for (idx, line) in text.lines().enumerate() {
                    if matcher.is_match(line) {
                        let prefix = format!("{}:{}: ", rel_path, idx + 1);
                        let prefix_len = prefix.len();

                        // 单行截断限制
                        let line_trunc_tag = " ... [TRUNCATED LINE]";
                        let max_line_payload =
                            MAX_GREP_LINE_BYTES.saturating_sub(line_trunc_tag.len());

                        // 同时受总输出剩余预算约束，确保总长度不超过有效预算
                        let remaining_total =
                            effective_budget.saturating_sub(total_output_bytes + prefix_len + 1);
                        if remaining_total == 0 {
                            hit_byte_limit = true;
                            break;
                        }

                        let allowed_line_bytes = max_line_payload.min(remaining_total);

                        let formatted = if line.len() > allowed_line_bytes {
                            let mut end = allowed_line_bytes.saturating_sub(line_trunc_tag.len());
                            while end > 0 && !line.is_char_boundary(end) {
                                end -= 1;
                            }
                            format!("{}{}{}", prefix, &line[..end], line_trunc_tag)
                        } else {
                            format!("{}{}", prefix, line)
                        };

                        let needed_bytes = formatted.len() + 1;
                        if total_output_bytes + needed_bytes > effective_budget {
                            hit_byte_limit = true;
                            break;
                        }

                        total_output_bytes += needed_bytes;
                        matches.push(formatted);
                        if matches.len() >= MAX_GREP_MATCHES {
                            hit_match_limit = true;
                            break;
                        }
                    }
                }

                if hit_match_limit || hit_byte_limit {
                    break;
                }
            }

            let mut matches_str = matches.join("\n");
            let mut notices = Vec::new();
            if hit_match_limit {
                notices.push(format!("达到最大匹配数限制 {MAX_GREP_MATCHES}"));
            }
            if hit_byte_limit {
                notices.push(format!("达到最大输出字节数限制 {MAX_GREP_TOTAL_BYTES}"));
            }
            if truncated_traversal {
                notices.push(format!(
                    "目录遍历达到上限 (已访问 {scanned_entries} 项/深度 {MAX_SEARCH_DEPTH})"
                ));
            }
            if read_errors > 0 {
                notices.push(format!("{read_errors} 个文件读取失败/权限不足"));
            }

            if matches.is_empty() {
                if read_errors > 0 {
                    Ok(format!(
                        "未找到匹配内容 (警告: {read_errors} 个文件因权限不足等原因未能读取)"
                    ))
                } else {
                    Ok("未找到匹配内容".into())
                }
            } else {
                if !notices.is_empty() {
                    let note = format!("\n[TRUNCATED: {}]", notices.join(", "));
                    matches_str.push_str(&note);
                }
                Ok(matches_str)
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_find(&self, args: Arguments) -> Result<String, String> {
        let pattern = args
            .pattern
            .or(args.value)
            .ok_or("find 操作缺少 pattern 或 value 参数")?;

        let cwd = self.cwd.clone();
        let subpath = args.path.unwrap_or_default();
        let is_glob = crate::pattern::has_glob_meta(&pattern);

        tokio::task::spawn_blocking(move || {
            let start_dir = if subpath.trim().is_empty() {
                cwd.clone()
            } else {
                checked_path(&cwd, &subpath, false)?
            };

            let symlink_meta = std::fs::symlink_metadata(&start_dir).map_err(|e| e.to_string())?;
            if symlink_meta.file_type().is_symlink() {
                return Err("不支持搜索符号链接".into());
            }

            let mut all_paths = Vec::new();
            let mut ctx = TraversalContext {
                visited: 0,
                max_entries: MAX_SEARCH_VISITED_ENTRIES,
                max_depth: MAX_SEARCH_DEPTH,
                truncated: false,
            };

            if symlink_meta.is_dir() {
                collect_all_entries_recursive(&start_dir, 0, &mut ctx, &mut all_paths)?;
            } else {
                all_paths.push(start_dir);
            }

            let mut matched_entries = Vec::new();
            let mut hit_limit = false;

            all_paths.sort();

            for path in all_paths {
                let rel_path = path.strip_prefix(&cwd).unwrap_or(&path).to_string_lossy();
                let file_name = path
                    .file_name()
                    .map(|f| f.to_string_lossy())
                    .unwrap_or_default();

                let matched = if is_glob {
                    crate::pattern::matches_glob(&pattern, &rel_path)
                        || (!pattern.contains('/')
                            && crate::pattern::matches_glob(&pattern, &file_name))
                } else {
                    file_name.contains(&pattern) || rel_path.contains(&pattern)
                };

                if matched {
                    matched_entries.push(rel_path.to_string());
                    if matched_entries.len() >= MAX_FIND_MATCHES {
                        hit_limit = true;
                        break;
                    }
                }
            }

            let mut result = matched_entries.join("\n");
            let mut notices = Vec::new();
            if hit_limit {
                notices.push(format!("达到最大匹配数限制 {MAX_FIND_MATCHES}"));
            }
            if ctx.truncated {
                notices.push(format!(
                    "目录遍历达到上限 (已访问 {} 项/深度 {MAX_SEARCH_DEPTH})",
                    ctx.visited
                ));
            }

            if !notices.is_empty() {
                result.push_str(&format!("\n[TRUNCATED: {}]", notices.join(", ")));
            }

            Ok(if result.is_empty() {
                "未找到匹配文件或目录".into()
            } else {
                result
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_ls(&self, args: Arguments) -> Result<String, String> {
        let cwd = self.cwd.clone();
        let subpath = args.path.unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let target_dir = if subpath.trim().is_empty() {
                cwd.clone()
            } else {
                checked_path(&cwd, &subpath, false)?
            };

            let meta = std::fs::symlink_metadata(&target_dir).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err("不可列出符号链接目录".into());
            }
            if !meta.is_dir() {
                return Err("目标不是目录".into());
            }

            let read_dir = std::fs::read_dir(&target_dir).map_err(|e| e.to_string())?;
            let mut entries = Vec::new();
            let mut truncated = false;

            for (total_scanned, item) in read_dir.enumerate() {
                if total_scanned >= MAX_SEARCH_VISITED_ENTRIES {
                    truncated = true;
                    break;
                }

                let entry = item.map_err(|e| e.to_string())?;
                let file_name = entry.file_name().to_string_lossy().to_string();

                if is_ignored_name(&file_name) {
                    continue;
                }

                let file_type = entry.file_type().map_err(|e| e.to_string())?;
                let suffix = if file_type.is_dir() {
                    "/"
                } else if file_type.is_symlink() {
                    "@"
                } else {
                    ""
                };

                entries.push(format!("{file_name}{suffix}"));
            }

            entries.sort();
            let total_count = entries.len();
            if entries.len() > MAX_LS_ENTRIES {
                entries.truncate(MAX_LS_ENTRIES);
                truncated = true;
            }

            let mut result = entries.join("\n");
            if truncated {
                result.push_str(&format!(
                    "\n[TRUNCATED: Showing {} of {} entries]",
                    entries.len(),
                    total_count
                ));
            }

            Ok(result)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_shell(&self, args: Arguments) -> Result<String, String> {
        let command = args
            .value
            .or(args.command)
            .ok_or("shell 操作缺少 value 或 command 参数")?;
        let shell_fut = self.host.shell(&command, &self.cwd);
        tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), shell_fut)
            .await
            .map_err(|_| "命令超过 120 秒，已取消命令协程；宿主须自行回收子进程".to_string())?
    }
}

fn is_ignored_name(name: &str) -> bool {
    IGNORED_NAMES.contains(&name)
}

struct TraversalContext {
    visited: usize,
    max_entries: usize,
    max_depth: usize,
    truncated: bool,
}

fn collect_files_recursive(
    dir: &Path,
    depth: usize,
    ctx: &mut TraversalContext,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth > ctx.max_depth || ctx.visited >= ctx.max_entries {
        ctx.truncated = true;
        return Ok(());
    }

    let read_dir =
        std::fs::read_dir(dir).map_err(|e| format!("读取目录 {} 失败: {e}", dir.display()))?;

    let mut subdirs = Vec::new();

    for item in read_dir {
        if ctx.visited >= ctx.max_entries {
            ctx.truncated = true;
            break;
        }
        ctx.visited += 1;

        let entry = item.map_err(|e| format!("读取目录项失败: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("获取文件类型失败: {e}"))?;

        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if is_ignored_name(&name) {
                continue;
            }
            subdirs.push(path);
        } else if file_type.is_file() {
            out.push(path);
        }
    }

    for subdir in subdirs {
        collect_files_recursive(&subdir, depth + 1, ctx, out)?;
    }

    Ok(())
}

fn collect_all_entries_recursive(
    dir: &Path,
    depth: usize,
    ctx: &mut TraversalContext,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth > ctx.max_depth || ctx.visited >= ctx.max_entries {
        ctx.truncated = true;
        return Ok(());
    }

    let read_dir =
        std::fs::read_dir(dir).map_err(|e| format!("读取目录 {} 失败: {e}", dir.display()))?;

    let mut subdirs = Vec::new();

    for item in read_dir {
        if ctx.visited >= ctx.max_entries {
            ctx.truncated = true;
            break;
        }
        ctx.visited += 1;

        let entry = item.map_err(|e| format!("读取目录项失败: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("获取文件类型失败: {e}"))?;

        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if is_ignored_name(&name) {
                continue;
            }
            out.push(path.clone());
            subdirs.push(path);
        } else {
            out.push(path);
        }
    }

    for subdir in subdirs {
        collect_all_entries_recursive(&subdir, depth + 1, ctx, out)?;
    }

    Ok(())
}

/// Workspace 工具参数定义。保留 legacy 字段同时支持新参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Arguments {
    pub operation: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub new_text: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub regex: Option<bool>,
    #[serde(default)]
    pub case_insensitive: Option<bool>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

impl Arguments {
    /// 构造 read 操作参数
    pub fn read(path: impl Into<String>) -> Self {
        Self {
            operation: "read".into(),
            path: Some(path.into()),
            ..Self::default()
        }
    }

    /// 构造带分页的 read 操作参数
    pub fn read_paged(path: impl Into<String>, offset: u64, limit: u64) -> Self {
        Self {
            operation: "read".into(),
            path: Some(path.into()),
            offset: Some(offset),
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// 构造 write 操作参数
    pub fn write(path: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            operation: "write".into(),
            path: Some(path.into()),
            value: Some(value.into()),
            ..Self::default()
        }
    }

    /// 构造 edit 操作参数
    pub fn edit(
        path: impl Into<String>,
        old_text: impl Into<String>,
        new_text: impl Into<String>,
    ) -> Self {
        Self {
            operation: "edit".into(),
            path: Some(path.into()),
            old_text: Some(old_text.into()),
            new_text: Some(new_text.into()),
            ..Self::default()
        }
    }

    /// 构造 grep 操作参数
    pub fn grep(pattern: impl Into<String>, path: Option<String>) -> Self {
        Self {
            operation: "grep".into(),
            path,
            pattern: Some(pattern.into()),
            ..Self::default()
        }
    }

    /// 构造带正则/大小写选项的 grep 操作参数
    pub fn grep_with_options(
        pattern: impl Into<String>,
        path: Option<String>,
        regex: bool,
        case_insensitive: bool,
    ) -> Self {
        Self {
            operation: "grep".into(),
            path,
            pattern: Some(pattern.into()),
            regex: Some(regex),
            case_insensitive: Some(case_insensitive),
            ..Self::default()
        }
    }

    /// 构造 find 操作参数
    pub fn find(pattern: impl Into<String>, path: Option<String>) -> Self {
        Self {
            operation: "find".into(),
            path,
            pattern: Some(pattern.into()),
            ..Self::default()
        }
    }

    /// 构造 ls 操作参数
    pub fn ls(path: Option<String>) -> Self {
        Self {
            operation: "ls".into(),
            path,
            ..Self::default()
        }
    }

    /// 构造 shell 操作参数
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            operation: "shell".into(),
            value: Some(command.into()),
            ..Self::default()
        }
    }

    /// 提取当前操作的目标标识（路径、模式或命令）
    pub fn target(&self) -> String {
        if matches!(self.operation.as_str(), "shell" | "bash") {
            self.value
                .clone()
                .or_else(|| self.command.clone())
                .unwrap_or_default()
        } else if let Some(ref p) = self.path {
            p.clone()
        } else if let Some(ref pat) = self.pattern {
            pat.clone()
        } else {
            self.value.clone().unwrap_or_default()
        }
    }
}

impl PortableTool for Workspace {
    const NAME: &'static str = "workspace";
    type Args = Arguments;
    type Output = Value;
    type Error = std::convert::Infallible;

    fn description(&self) -> String {
        "Read, write, edit files, search contents (grep), find files (find), list directories (ls), or execute a noninteractive shell command in the node working directory. All paths are relative to workspace root. Commands time out after 120 seconds.".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["read", "write", "edit", "grep", "find", "ls", "shell", "bash"],
                    "description": "The workspace operation to perform"
                },
                "path": {
                    "type": "string",
                    "description": "Relative file or directory path"
                },
                "value": {
                    "type": "string",
                    "description": "File content for write or shell command"
                },
                "content": {
                    "type": "string",
                    "description": "File content alias for write"
                },
                "command": {
                    "type": "string",
                    "description": "Command alias for shell/bash"
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact text to replace for edit (must match uniquely)"
                },
                "new_text": {
                    "type": "string",
                    "description": "Replacement text for edit"
                },
                "pattern": {
                    "type": "string",
                    "description": "Substring, regex, or glob pattern to search for in grep or find"
                },
                "regex": {
                    "type": "boolean",
                    "description": "Enable regular expression matching for grep"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Enable case-insensitive matching for grep"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number to start reading from (1-based, default 1)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Arguments) -> Result<Value, Self::Error> {
        let target = if matches!(args.operation.as_str(), "shell" | "bash") {
            args.value
                .clone()
                .or_else(|| args.command.clone())
                .unwrap_or_default()
        } else if let Some(ref p) = args.path {
            p.clone()
        } else if let Some(ref pat) = args.pattern {
            pat.clone()
        } else {
            args.value.clone().unwrap_or_default()
        };
        let operation = args.operation.clone();

        self.host.on_event(AgentEvent::ToolStarted {
            operation: operation.clone(),
            target: target.clone(),
        });

        let result = self.execute(args).await;
        self.host.on_event(AgentEvent::ToolFinished {
            operation,
            target,
            result: result.clone(),
        });

        let value = match result {
            Ok(output) => json!({"output": output}),
            Err(error) => json!({"error": error}),
        };

        Ok(value)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Clone)]
pub struct ReadTool(pub Workspace);
impl PortableTool for ReadTool {
    const NAME: &'static str = "read";
    type Args = ReadArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Read UTF-8 file contents relative to workspace root, with optional 1-based line offset and limit.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative file path"},
                "offset": {"type": "integer", "minimum": 1, "description": "1-based starting line number"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to read"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: ReadArgs) -> Result<Value, Self::Error> {
        self.0
            .call(Arguments {
                operation: "read".into(),
                path: Some(args.path),
                offset: args.offset,
                limit: args.limit,
                ..Arguments::default()
            })
            .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Clone)]
pub struct WriteTool(pub Workspace);
impl PortableTool for WriteTool {
    const NAME: &'static str = "write";
    type Args = WriteArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Atomically write UTF-8 content to a relative file path (parent directory must exist, max 1 MiB).".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative file path"},
                "content": {"type": "string", "description": "File content to write"},
                "value": {"type": "string", "description": "Alias for content"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: WriteArgs) -> Result<Value, Self::Error> {
        self.0
            .call(Arguments {
                operation: "write".into(),
                path: Some(args.path),
                content: args.content,
                value: args.value,
                ..Arguments::default()
            })
            .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone)]
pub struct EditTool(pub Workspace);
impl PortableTool for EditTool {
    const NAME: &'static str = "edit";
    type Args = EditArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Edit a file by replacing an exact, uniquely matching old_text with new_text.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative file path"},
                "old_text": {"type": "string", "description": "Exact existing text to replace (must match uniquely)"},
                "new_text": {"type": "string", "description": "Replacement text"}
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: EditArgs) -> Result<Value, Self::Error> {
        self.0
            .call(Arguments::edit(args.path, args.old_text, args.new_text))
            .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BashArgs {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Clone)]
pub struct BashTool(pub Workspace);
impl PortableTool for BashTool {
    const NAME: &'static str = "bash";
    type Args = BashArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Execute a noninteractive shell command in the workspace directory (120s timeout).".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "value": {"type": "string", "description": "Alias for command"}
            },
            "additionalProperties": false
        })
    }
    async fn call(&self, args: BashArgs) -> Result<Value, Self::Error> {
        self.0
            .call(Arguments {
                operation: "shell".into(),
                command: args.command,
                value: args.value,
                ..Arguments::default()
            })
            .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub regex: Option<bool>,
    #[serde(default)]
    pub case_insensitive: Option<bool>,
}

#[derive(Clone)]
pub struct GrepTool(pub Workspace);
impl PortableTool for GrepTool {
    const NAME: &'static str = "grep";
    type Args = GrepArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Search file contents by literal string or regular expression within the workspace.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (literal or regex)"},
                "path": {"type": "string", "description": "Optional relative file or directory path"},
                "regex": {"type": "boolean", "description": "Treat pattern as a regular expression"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: GrepArgs) -> Result<Value, Self::Error> {
        self.0
            .call(Arguments {
                operation: "grep".into(),
                pattern: Some(args.pattern),
                path: args.path,
                regex: args.regex,
                case_insensitive: args.case_insensitive,
                ..Arguments::default()
            })
            .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindArgs {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone)]
pub struct FindTool(pub Workspace);
impl PortableTool for FindTool {
    const NAME: &'static str = "find";
    type Args = FindArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Find files or directories by glob pattern (e.g. '**/*.rs', '*.toml') or substring.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern or substring to match"},
                "path": {"type": "string", "description": "Optional relative starting directory"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: FindArgs) -> Result<Value, Self::Error> {
        self.0.call(Arguments::find(args.pattern, args.path)).await
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LsArgs {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone)]
pub struct LsTool(pub Workspace);
impl PortableTool for LsTool {
    const NAME: &'static str = "ls";
    type Args = LsArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "List directory contents in the workspace (sorted, bounded).".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Optional relative directory path"}
            },
            "additionalProperties": false
        })
    }
    async fn call(&self, args: LsArgs) -> Result<Value, Self::Error> {
        self.0.call(Arguments::ls(args.path)).await
    }
}

/// 校验读写路径必须严格在工作目录内，防止目录遍历逃逸与符号链接越界。
pub fn checked_path(cwd: &Path, path: &str, write: bool) -> Result<PathBuf, String> {
    let raw_path = Path::new(path);
    if raw_path.is_absolute() {
        return Err("必须使用相对路径".into());
    }

    // 检查 path 组件，空或者含 ParentDir (..) 均拒绝
    let mut has_component = false;
    for comp in raw_path.components() {
        match comp {
            Component::ParentDir => return Err("路径不能包含父目录导航 (..)".into()),
            Component::Normal(_) | Component::CurDir => {
                has_component = true;
            }
            _ => {}
        }
    }
    if !has_component {
        return Err("路径不能为空".into());
    }

    // 在 canonicalize 前用 cwd.join(path) 逐组件 symlink_metadata 检测任何符号链接拒绝（目录/文件），免得 canonicalize 擦掉 symlink
    let mut current = cwd.to_path_buf();
    for comp in raw_path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(c) => {
                current.push(c);
                if let Ok(meta) = std::fs::symlink_metadata(&current)
                    && meta.file_type().is_symlink()
                {
                    return Err("不支持符号链接".into());
                }
            }
            _ => return Err("非法路径组件".into()),
        }
    }

    let target = cwd.join(raw_path);

    let resolved = if write && !target.exists() {
        let parent = target.parent().ok_or("无效路径")?;
        let canonical_parent = parent.canonicalize().map_err(|e| e.to_string())?;
        let file_name = target.file_name().ok_or("无效文件名")?;
        canonical_parent.join(file_name)
    } else {
        target.canonicalize().map_err(|e| e.to_string())?
    };

    if !resolved.starts_with(cwd) {
        return Err("文件路径必须位于节点工作目录中".into());
    }

    if let Ok(symlink_meta) = std::fs::symlink_metadata(&resolved)
        && symlink_meta.file_type().is_symlink()
    {
        return Err("不支持符号链接".into());
    }

    Ok(resolved)
}
