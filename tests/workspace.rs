use mio_coding_agent::{
    Arguments, Host, HostFuture, Workspace, checked_path, workspace::MAX_READ_OUTPUT_BYTES,
};
use std::{fs::File, io::Write, path::Path, sync::Arc};

struct DummyHost;
impl Host for DummyHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
}

#[tokio::test]
async fn test_workspace_new_validation() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);

    // 有效目录
    assert!(Workspace::new(folder.path(), host.clone()).is_ok());

    // 不存在的路径
    let nonexistent = folder.path().join("does_not_exist");
    assert!(Workspace::new(nonexistent, host.clone()).is_err());

    // 是文件而非目录
    let file_path = folder.path().join("file.txt");
    std::fs::write(&file_path, "hello").unwrap();
    assert!(Workspace::new(file_path, host).is_err());
}

#[tokio::test]
async fn test_checked_path_parent_dir_and_empty() {
    let folder = tempfile::tempdir().unwrap();
    let cwd = folder.path().canonicalize().unwrap();

    // 拒绝包含 ..
    assert!(checked_path(&cwd, "../outside.txt", false).is_err());
    assert!(checked_path(&cwd, "a/../../outside.txt", false).is_err());
    assert!(checked_path(&cwd, "..", false).is_err());

    // 拒绝空路径或纯点
    assert!(checked_path(&cwd, "", false).is_err());
    assert!(checked_path(&cwd, "   ", false).is_err());

    // 正常相对路径
    let sub = cwd.join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(checked_path(&cwd, "sub", false).is_ok());
}

#[tokio::test]
async fn test_legacy_json_argument_parsing() {
    let raw_json = serde_json::json!({
        "operation": "read",
        "path": "foo.txt",
        "value": "bar"
    });
    let parsed: Arguments = serde_json::from_value(raw_json).unwrap();
    assert_eq!(parsed.operation, "read");
    assert_eq!(parsed.path.as_deref(), Some("foo.txt"));
    assert_eq!(parsed.value.as_deref(), Some("bar"));
}

#[tokio::test]
async fn test_read_pagination_chinese_crlf_zero_limit() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    // 创建中文 + CRLF 文件
    let content = "第一行\r\n第二行：测试中文\r\n第三行：CRLF换行\r\n第四行：结束";
    std::fs::write(folder.path().join("test.txt"), content).unwrap();

    // limit 为 0 校验报错
    let args_zero_limit = Arguments {
        operation: "read".into(),
        path: Some("test.txt".into()),
        limit: Some(0),
        ..Default::default()
    };
    assert!(ws.execute(args_zero_limit).await.is_err());

    // offset 为 0 校验报错
    let args_zero_offset = Arguments {
        operation: "read".into(),
        path: Some("test.txt".into()),
        offset: Some(0),
        ..Default::default()
    };
    assert!(ws.execute(args_zero_offset).await.is_err());

    // 分页读取：第 2 行开始，读 2 行
    let paged = ws
        .execute(Arguments::read_paged("test.txt", 2, 2))
        .await
        .unwrap();
    assert!(paged.contains("[TRUNCATED: Showing lines 2..3 of 4. Next offset: 4]"));
    assert!(paged.contains("第二行：测试中文\r\n第三行：CRLF换行"));
}

#[tokio::test]
async fn test_read_huge_line_budget_and_file_limit() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    // 1. 单行超过 64 KiB 预算
    let huge_line = "A".repeat(MAX_READ_OUTPUT_BYTES + 100);
    std::fs::write(folder.path().join("huge_line.txt"), &huge_line).unwrap();

    let err = ws
        .execute(Arguments::read("huge_line.txt"))
        .await
        .unwrap_err();
    assert!(err.contains("超出单次输出上限预算"));
    assert!(err.contains("请使用 shell 分段读取"));

    // 2. 文件大小超过 8 MiB 硬限
    let big_path = folder.path().join("big.bin");
    let mut file = File::create(&big_path).unwrap();
    // 写入 8 MiB + 10 字节
    let chunk = vec![0u8; 1024 * 1024];
    for _ in 0..8 {
        file.write_all(&chunk).unwrap();
    }
    file.write_all(b"1234567890").unwrap();
    drop(file);

    let err = ws.execute(Arguments::read("big.bin")).await.unwrap_err();
    assert!(err.contains("文件超过最大读取上限"));
    assert!(err.contains("8 MiB"));
}

#[tokio::test]
async fn test_edit_multi_matches_overlapping_and_permissions_crlf() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    let file_path = folder.path().join("edit_test.txt");

    // 1. 重叠匹配测试：aaaa 查找 aaa，有两个重叠位置 (0 和 1)，应拒绝
    std::fs::write(&file_path, "aaaa").unwrap();
    let err = ws
        .execute(Arguments::edit("edit_test.txt", "aaa", "bbb"))
        .await
        .unwrap_err();
    assert!(err.contains("存在歧义或重叠"));
    // 确认原文件未被修改
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "aaaa");

    // 2. 多处不重叠匹配：a_a_a 查找 a
    std::fs::write(&file_path, "a_a_a").unwrap();
    let err = ws
        .execute(Arguments::edit("edit_test.txt", "a", "b"))
        .await
        .unwrap_err();
    assert!(err.contains("存在歧义或重叠"));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "a_a_a");

    // 3. 零匹配测试
    let err = ws
        .execute(Arguments::edit("edit_test.txt", "not_exist", "b"))
        .await
        .unwrap_err();
    assert!(err.contains("未找到匹配的 old_text"));

    // 4. 唯一匹配 + CRLF + 保留文件权限
    let original = "line1\r\ntarget_unique\r\nline3\r\n";
    std::fs::write(&file_path, original).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&file_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&file_path, perms).unwrap();
    }

    ws.execute(Arguments::edit(
        "edit_test.txt",
        "target_unique\r\n",
        "replaced_success\r\n",
    ))
    .await
    .unwrap();

    let modified = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(modified, "line1\r\nreplaced_success\r\nline3\r\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

#[tokio::test]
async fn test_grep_and_find_features() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    // 创建测试目录结构
    let sub = folder.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    let git_dir = folder.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("git_file.txt"), "target keyword").unwrap();

    let node_dir = folder.path().join("node_modules");
    std::fs::create_dir(&node_dir).unwrap();
    std::fs::write(node_dir.join("npm_file.txt"), "target keyword").unwrap();

    let f1 = folder.path().join("b_file.txt");
    std::fs::write(&f1, "hello keyword world\nsecond line").unwrap();

    let f2 = sub.join("a_file.txt");
    std::fs::write(&f2, "another keyword match").unwrap();

    // 1. grep 忽略 .git 与 node_modules，排序结果 (b_file.txt vs sub/a_file.txt)
    let res = ws.execute(Arguments::grep("keyword", None)).await.unwrap();
    assert!(!res.contains(".git"));
    assert!(!res.contains("node_modules"));
    let lines: Vec<&str> = res.lines().collect();
    assert_eq!(lines.len(), 2);
    // 严格断言 b_file 第一，sub/a 第二
    assert!(lines[0].starts_with("b_file.txt:"));
    assert!(lines[1].starts_with("sub/a_file.txt:"));

    // 2. grep 直接指定文件
    let single_res = ws
        .execute(Arguments::grep(
            "keyword",
            Some("sub/a_file.txt".to_string()),
        ))
        .await
        .unwrap();
    assert!(single_res.contains("sub/a_file.txt:1: another keyword match"));

    // 3. find 功能与排序/忽略
    let find_res = ws.execute(Arguments::find("file.txt", None)).await.unwrap();
    assert!(!find_res.contains(".git"));
    assert!(!find_res.contains("node_modules"));
    assert!(find_res.contains("b_file.txt"));
    assert!(find_res.contains("sub/a_file.txt"));

    #[cfg(unix)]
    {
        // 符号链接跳过或报错
        std::os::unix::fs::symlink(&f1, folder.path().join("link_file.txt")).unwrap();
        let grep_symlink = ws
            .execute(Arguments::grep(
                "keyword",
                Some("link_file.txt".to_string()),
            ))
            .await;
        assert!(grep_symlink.is_err());
    }
}

#[tokio::test]
async fn test_grep_unreadable_file_reports_warning() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    let file_path = folder.path().join("unreadable.txt");
    std::fs::write(&file_path, "secret keyword").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&file_path).unwrap().permissions();
        perms.set_mode(0o000);
        let _ = std::fs::set_permissions(&file_path, perms);

        let res = ws
            .execute(Arguments::grep("secret", None))
            .await
            .unwrap_or_else(|e| e);
        // 根据当前运行环境是否有实际读取能力来确定断言
        let can_read = File::open(&file_path).is_ok();
        if can_read {
            assert!(res.contains("secret"));
        } else {
            assert!(res.contains("读取失败") || res.contains("权限不足") || res.contains("警告"));
        }

        // 恢复权限以便 tempdir 正常清理
        let mut restore = std::fs::metadata(&file_path).unwrap().permissions();
        restore.set_mode(0o644);
        let _ = std::fs::set_permissions(&file_path, restore);
    }
}

#[tokio::test]
async fn test_grep_chinese_long_line_and_total_budget() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    // 1. 中文长行截断测试（验证不会在 UTF-8 字符中间截断导致 panic，且包含截断标记）
    let long_chinese = "测试中文长行内容。".repeat(10_000);
    let test_file = folder.path().join("chinese_long.txt");
    std::fs::write(&test_file, format!("prefix target_key {}\n", long_chinese)).unwrap();

    let grep_res = ws
        .execute(Arguments::grep(
            "target_key",
            Some("chinese_long.txt".into()),
        ))
        .await
        .unwrap();

    assert!(grep_res.contains("chinese_long.txt:1: prefix target_key"));
    assert!(grep_res.contains("[TRUNCATED LINE]"));
    assert!(grep_res.len() <= mio_coding_agent::workspace::MAX_GREP_TOTAL_BYTES);

    // 2. 总字节上限测试（生成多行匹配，总长度超过 64 KiB）
    let many_matches_file = folder.path().join("many_matches.txt");
    let line_content = format!("match_total_key {}\n", "x".repeat(500));
    std::fs::write(&many_matches_file, line_content.repeat(200)).unwrap();

    let total_res = ws
        .execute(Arguments::grep(
            "match_total_key",
            Some("many_matches.txt".into()),
        ))
        .await
        .unwrap();

    assert!(total_res.contains("达到最大输出字节数限制"));
    assert!(total_res.len() <= mio_coding_agent::workspace::MAX_GREP_TOTAL_BYTES);
}

#[tokio::test]
async fn test_search_match_truncation_and_ls_sorting() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host).unwrap();

    // 1. 单文件 > 500 匹配行，验证 TRUNCATED
    let single_file = folder.path().join("many_lines.txt");
    let mut file_content = String::new();
    for i in 1..=550 {
        file_content.push_str(&format!("line {} special_tag\n", i));
    }
    std::fs::write(&single_file, file_content).unwrap();

    let grep_res = ws
        .execute(Arguments::grep(
            "special_tag",
            Some("many_lines.txt".into()),
        ))
        .await
        .unwrap();
    assert!(grep_res.contains("达到最大匹配数限制 500"));
    assert!(grep_res.contains("[TRUNCATED:"));

    // 2. ls 排序测试与目录/文件标识
    let dir_b = folder.path().join("dir_b");
    let dir_a = folder.path().join("dir_a");
    std::fs::create_dir(&dir_b).unwrap();
    std::fs::create_dir(&dir_a).unwrap();
    std::fs::write(folder.path().join("file_z.txt"), "z").unwrap();
    std::fs::write(folder.path().join("file_a.txt"), "a").unwrap();

    let ls_res = ws.execute(Arguments::ls(None)).await.unwrap();
    let ls_lines: Vec<&str> = ls_res.lines().collect();
    // 排序后应为：dir_a/, dir_b/, file_a.txt, file_z.txt 等（以及前面已创建的 many_lines.txt）
    assert_eq!(ls_lines[0], "dir_a/");
    assert_eq!(ls_lines[1], "dir_b/");
    assert_eq!(ls_lines[2], "file_a.txt");
}

#[tokio::test]
async fn test_regex_grep_glob_find_unified_diff_and_tool_filter() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let ws = Workspace::new(folder.path(), host.clone()).unwrap();

    std::fs::create_dir_all(folder.path().join("src/nested")).unwrap();
    std::fs::write(
        folder.path().join("src/lib.rs"),
        "pub fn alpha() {}\nfn beta_helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        folder.path().join("src/nested/mod.rs"),
        "PUB FN GAMMA() {}\n",
    )
    .unwrap();
    std::fs::write(folder.path().join("README.md"), "docs\n").unwrap();

    // Regex grep
    let mut regex_grep = Arguments::grep(r"^pub\s+fn\s+\w+", None);
    regex_grep.regex = Some(true);
    let out = ws.execute(regex_grep).await.unwrap();
    assert!(out.contains("src/lib.rs:1: pub fn alpha() {}"));
    assert!(!out.contains("beta_helper"));

    // Case-insensitive regex grep
    let mut ci_grep = Arguments::grep(r"pub\s+fn\s+gamma", None);
    ci_grep.regex = Some(true);
    ci_grep.case_insensitive = Some(true);
    let out_ci = ws.execute(ci_grep).await.unwrap();
    assert!(out_ci.contains("src/nested/mod.rs:1: PUB FN GAMMA() {}"));

    // Glob find
    let glob_out = ws.execute(Arguments::find("**/*.rs", None)).await.unwrap();
    assert!(glob_out.contains("src/lib.rs"));
    assert!(glob_out.contains("src/nested/mod.rs"));
    assert!(!glob_out.contains("README.md"));

    // Unified diff on edit
    let diff_out = ws
        .execute(Arguments::edit(
            "src/lib.rs",
            "pub fn alpha() {}",
            "pub fn alpha_v2() {}",
        ))
        .await
        .unwrap();
    assert!(diff_out.contains("--- a/src/lib.rs"));
    assert!(diff_out.contains("+++ b/src/lib.rs"));
    assert!(diff_out.contains("-pub fn alpha() {}"));
    assert!(diff_out.contains("+pub fn alpha_v2() {}"));

    // ToolFilter enforcement
    let filter = mio_coding_agent::ToolFilter::from_cli(Some("read,grep"), None, false).unwrap();
    let filtered_ws = Workspace::new(folder.path(), host)
        .unwrap()
        .with_tool_filter(filter);
    assert!(
        filtered_ws
            .execute(Arguments::read("README.md"))
            .await
            .is_ok()
    );
    assert!(
        filtered_ws
            .execute(Arguments::write("blocked.txt", "no"))
            .await
            .is_err()
    );
}
