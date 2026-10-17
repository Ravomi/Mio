use mio_coding_agent::{
    Arguments, EditTool, Host, HostFuture, ReadTool, Workspace, checked_path,
    checkpoint::CheckpointStore,
    workspace::{EditArgs, MAX_READ_OUTPUT_BYTES},
};
use mio_core::tool::PortableTool;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Write, path::Path, sync::Arc};

struct DummyHost;
impl Host for DummyHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
}

struct SteeringHost;
impl Host for SteeringHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
    fn poll_steering(&self) -> Option<String> {
        Some("请先检查预览".into())
    }
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
async fn edit_hash_anchor_rejects_stale_content_even_when_old_text_still_matches() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("source.txt");
    let original = "before\ntarget\nafter\n";
    std::fs::write(&path, original).unwrap();
    let original_hash = format!("{:x}", Sha256::digest(original.as_bytes()));
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost)).unwrap();
    let page = ws
        .execute(Arguments {
            include_sha256: Some(true),
            ..Arguments::read_paged("source.txt", 2, 1)
        })
        .await
        .unwrap();
    assert!(page.starts_with(&format!("[sha256: {original_hash}]\n")));
    assert!(page.contains("target"));

    let response = ws
        .execute(Arguments::edit_with_hash(
            "source.txt",
            "target",
            "changed",
            original_hash.to_uppercase(),
        ))
        .await
        .unwrap();
    assert!(response.contains("文件已成功编辑"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "before\nchanged\nafter\n"
    );

    // The old text still matches uniquely after an external edit. The hash
    // guards the entire file version, not merely the selected substring.
    std::fs::write(&path, "external\nchanged\nafter\n").unwrap();
    let stale = ws
        .execute(Arguments::edit_with_hash(
            "source.txt",
            "changed",
            "replacement",
            original_hash,
        ))
        .await
        .unwrap_err();
    assert!(stale.contains("expected_sha256"), "{stale}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "external\nchanged\nafter\n"
    );

    let invalid = ws
        .execute(Arguments::edit_with_hash(
            "source.txt",
            "changed",
            "x",
            "bad",
        ))
        .await
        .unwrap_err();
    assert!(invalid.contains("64 位"), "{invalid}");
    let mut wrong_operation = Arguments::read("source.txt");
    wrong_operation.expected_sha256 = Some("0".repeat(64));
    assert!(
        ws.execute(wrong_operation)
            .await
            .unwrap_err()
            .contains("仅适用于 edit")
    );
    let mut wrong_operation = Arguments::read("source.txt");
    wrong_operation.expected_after_sha256 = Some("0".repeat(64));
    assert!(
        ws.execute(wrong_operation)
            .await
            .unwrap_err()
            .contains("仅适用于 edit")
    );
    let mut byte_page = Arguments::read("source.txt");
    byte_page.byte_offset = Some(0);
    byte_page.include_sha256 = Some(true);
    let byte_page: serde_json::Value =
        serde_json::from_str(&ws.execute(byte_page).await.unwrap()).unwrap();
    assert_eq!(
        byte_page["sha256"],
        format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()))
    );
}

#[tokio::test]
async fn read_hash_header_stays_within_the_existing_output_budget() {
    let folder = tempfile::tempdir().unwrap();
    let body = "line\n".repeat(1000);
    std::fs::write(folder.path().join("many.txt"), &body).unwrap();
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost))
        .unwrap()
        .with_read_output_limit_bytes(256);
    let page = ws
        .execute(Arguments {
            include_sha256: Some(true),
            ..Arguments::read("many.txt")
        })
        .await
        .unwrap();
    assert!(page.starts_with("[sha256: "));
    assert!(page.contains("[TRUNCATED:"));
    assert!(page.len() <= 256, "{}", page.len());
}

#[tokio::test]
async fn standalone_edit_tool_passes_the_hash_anchor_to_workspace() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("source.txt");
    std::fs::write(&path, "target").unwrap();
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost)).unwrap();
    assert!(
        PortableTool::parameters(&ReadTool(ws.clone()))["properties"]["include_sha256"].is_object()
    );
    let tool = EditTool(ws);
    assert!(PortableTool::parameters(&tool)["properties"]["expected_sha256"].is_object());
    assert!(PortableTool::parameters(&tool)["properties"]["expected_after_sha256"].is_object());
    assert!(PortableTool::parameters(&tool)["properties"]["preview"].is_object());
    let args = EditArgs {
        path: "source.txt".into(),
        old_text: "target".into(),
        new_text: "changed".into(),
        expected_sha256: Some("0".repeat(64)),
        expected_after_sha256: None,
        preview: None,
    };
    let result = PortableTool::call(&tool, args).await.unwrap();
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("expected_sha256")
    );
    let mismatch = PortableTool::call(
        &tool,
        EditArgs {
            path: "source.txt".into(),
            old_text: "target".into(),
            new_text: "changed".into(),
            expected_sha256: Some(format!("{:x}", Sha256::digest(b"target"))),
            expected_after_sha256: Some("0".repeat(64)),
            preview: None,
        },
    )
    .await
    .unwrap();
    assert!(
        mismatch["error"]
            .as_str()
            .unwrap()
            .contains("expected_after_sha256")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "target");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "target");

    let preview = PortableTool::call(
        &tool,
        EditArgs {
            path: "source.txt".into(),
            old_text: "target".into(),
            new_text: "changed".into(),
            expected_sha256: Some(format!("{:x}", Sha256::digest(b"target"))),
            expected_after_sha256: None,
            preview: Some(true),
        },
    )
    .await
    .unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(preview["output"].as_str().unwrap()).unwrap();
    assert_eq!(payload["status"], "preview");
    assert_eq!(
        std::fs::read_to_string(folder.path().join("source.txt")).unwrap(),
        "target"
    );
}

#[tokio::test]
async fn edit_preview_reports_hashes_without_writing_or_recording_a_checkpoint() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("source.txt");
    let original = "one\ntarget\nthree\n";
    let modified = "one\nchanged\nthree\n";
    std::fs::write(&path, original).unwrap();
    let before_sha256 = format!("{:x}", Sha256::digest(original.as_bytes()));
    let checkpoints = Arc::new(CheckpointStore::open(folder.path()).unwrap());
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost))
        .unwrap()
        .with_checkpoints(checkpoints.clone());
    let mut args = Arguments::preview_edit("source.txt", "target", "changed");
    args.expected_sha256 = Some(before_sha256.clone());
    let preview: serde_json::Value =
        serde_json::from_str(&ws.execute(args).await.unwrap()).unwrap();
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["path"], "source.txt");
    assert_eq!(preview["start_line"], 2);
    assert_eq!(preview["before_sha256"], before_sha256);
    assert_eq!(
        preview["after_sha256"],
        format!("{:x}", Sha256::digest(modified.as_bytes()))
    );
    assert_eq!(preview["before_bytes"], original.len());
    assert_eq!(preview["after_bytes"], modified.len());
    assert_eq!(preview["diff_truncated"], false);
    assert!(preview["diff"].as_str().unwrap().contains("+changed"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(checkpoints.list().unwrap().is_empty());

    std::fs::write(&path, "external\ntarget\nthree\n").unwrap();
    let mut stale = Arguments::preview_edit("source.txt", "target", "changed");
    stale.expected_sha256 = Some(before_sha256);
    assert!(
        ws.execute(stale)
            .await
            .unwrap_err()
            .contains("expected_sha256")
    );
    assert!(checkpoints.list().unwrap().is_empty());
}

#[tokio::test]
async fn edit_from_preview_requires_the_same_input_and_exact_previewed_output() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("source.txt");
    let original = "one\ntarget\nthree\n";
    let modified = "one\nchanged\nthree\n";
    std::fs::write(&path, original).unwrap();
    let checkpoints = Arc::new(CheckpointStore::open(folder.path()).unwrap());
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost))
        .unwrap()
        .with_checkpoints(checkpoints.clone());
    let preview: serde_json::Value = serde_json::from_str(
        &ws.execute(Arguments::preview_edit("source.txt", "target", "changed"))
            .await
            .unwrap(),
    )
    .unwrap();
    let before = preview["before_sha256"].as_str().unwrap();
    let after = preview["after_sha256"].as_str().unwrap();

    let changed_output =
        Arguments::edit_from_preview("source.txt", "target", "different", before, after);
    assert!(
        ws.execute(changed_output)
            .await
            .unwrap_err()
            .contains("expected_after_sha256")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(checkpoints.list().unwrap().is_empty());

    let mut no_input_hash = Arguments::edit("source.txt", "target", "changed");
    no_input_hash.expected_after_sha256 = Some(after.to_owned());
    assert!(
        ws.execute(no_input_hash)
            .await
            .unwrap_err()
            .contains("expected_sha256")
    );
    assert!(
        ws.execute(Arguments::edit_from_preview(
            "source.txt",
            "target",
            "changed",
            before,
            "short",
        ))
        .await
        .unwrap_err()
        .contains("64 位")
    );

    std::fs::write(&path, "external\ntarget\nthree\n").unwrap();
    assert!(
        ws.execute(Arguments::edit_from_preview(
            "source.txt",
            "target",
            "changed",
            before,
            after,
        ))
        .await
        .unwrap_err()
        .contains("expected_sha256")
    );
    assert!(checkpoints.list().unwrap().is_empty());

    std::fs::write(&path, original).unwrap();
    ws.execute(Arguments::edit_from_preview(
        "source.txt",
        "target",
        "changed",
        before.to_uppercase(),
        after.to_uppercase(),
    ))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), modified);
    assert_eq!(checkpoints.list().unwrap().len(), 1);
}

#[tokio::test]
async fn edit_preview_bounds_a_long_single_line_diff_and_rejects_other_operations() {
    let folder = tempfile::tempdir().unwrap();
    let original = "界".repeat(50_000);
    std::fs::write(folder.path().join("long.txt"), &original).unwrap();
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost)).unwrap();
    let output = ws
        .execute(Arguments::preview_edit(
            "long.txt",
            &original,
            "海".repeat(50_000),
        ))
        .await
        .unwrap();
    let preview: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(output.len() <= 16 * 1024);
    assert_eq!(preview["diff_truncated"], true);
    assert_eq!(
        std::fs::read_to_string(folder.path().join("long.txt")).unwrap(),
        original
    );

    let mut invalid = Arguments::read("long.txt");
    invalid.preview = Some(true);
    assert!(
        ws.execute(invalid)
            .await
            .unwrap_err()
            .contains("仅适用于 edit")
    );
}

#[tokio::test]
async fn edit_preview_keeps_a_concurrent_steering_message_in_valid_json() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("source.txt"), "target").unwrap();
    let ws = Workspace::new(folder.path(), Arc::new(SteeringHost)).unwrap();
    let output = ws
        .execute(Arguments::preview_edit("source.txt", "target", "changed"))
        .await
        .unwrap();
    let preview: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["user_steering_message"], "请先检查预览");
    assert_eq!(
        std::fs::read_to_string(folder.path().join("source.txt")).unwrap(),
        "target"
    );
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

#[tokio::test]
async fn aggregate_workspace_respects_explicit_exclusions() {
    use mio_coding_agent::ToolFilter;
    for (allow, exclude, operation, permitted) in [
        (None, Some("write"), "write", false),
        (Some("workspace"), Some("wr*"), "write", false),
        (Some("workspace"), None, "write", true),
        (Some("read"), None, "write", false),
        (None, Some("bash"), "shell", false),
        (None, Some("shell"), "bash", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path(), Arc::new(DummyHost))
            .unwrap()
            .with_tool_filter(ToolFilter::from_cli(allow, exclude, false).unwrap());
        let args = serde_json::from_value(serde_json::json!({
            "operation": operation, "path": "out", "value": "written", "command": "unused"
        }))
        .unwrap();
        let result = workspace.execute(args).await;
        assert_eq!(
            result.is_ok(),
            permitted,
            "{allow:?} {exclude:?} {operation}: {result:?}"
        );
        if !permitted {
            assert!(result.unwrap_err().contains("工具过滤策略禁止执行"));
        }
        assert_eq!(dir.path().join("out").exists(), permitted);
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path(), Arc::new(DummyHost))
        .unwrap()
        .with_tool_filter(ToolFilter::none());
    let args =
        serde_json::from_value(serde_json::json!({"operation":"write","path":"out","value":"x"}))
            .unwrap();
    assert!(workspace.execute(args).await.is_err());
    assert!(!dir.path().join("out").exists());
}

struct SyncDenyHost;
impl Host for SyncDenyHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
    fn approve_tool(&self, _operation: &str, _target: &str) -> Result<(), String> {
        Err("sync denial".into())
    }
}

type ApprovalReceiver =
    Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<Result<(), String>>>>>;

struct AsyncGateHost {
    gate_rx: ApprovalReceiver,
    started_tx: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}
impl Host for AsyncGateHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
    fn approve_tool_async(
        &self,
        _operation: &str,
        _target: &str,
    ) -> HostFuture<Result<(), String>> {
        let gate_rx = Arc::clone(&self.gate_rx);
        let started_tx = Arc::clone(&self.started_tx);
        Box::pin(async move {
            if let Some(tx) = started_tx.lock().await.take() {
                let _ = tx.send(());
            }
            let mut guard = gate_rx.lock().await;
            let rx = guard.take().expect("receiver already used");
            rx.await.map_err(|_| "channel canceled".to_string())?
        })
    }
}

#[tokio::test]
async fn test_sync_approve_tool_deny_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path(), Arc::new(SyncDenyHost)).unwrap();
    let file_path = dir.path().join("deny.txt");
    let args = serde_json::from_value(serde_json::json!({
        "operation": "write",
        "path": "deny.txt",
        "value": "should not be written"
    }))
    .unwrap();

    let res = workspace.execute(args).await;
    assert!(res.is_err());
    assert_eq!(res.unwrap_err(), "sync denial");
    assert!(!file_path.exists());

    std::fs::write(&file_path, "target").unwrap();
    let preview = workspace
        .execute(Arguments::preview_edit("deny.txt", "target", "changed"))
        .await;
    assert_eq!(preview.unwrap_err(), "sync denial");
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "target");
}

#[tokio::test]
async fn test_async_approve_tool_gate_pending_and_resolution() {
    // 1. Allow 流程：oneshot 在 pending 时文件未修改，发送 Ok(()) 后才写
    let dir = tempfile::tempdir().unwrap();
    let (tx_allow, rx_allow) = tokio::sync::oneshot::channel();
    let (started_tx_allow, started_rx_allow) = tokio::sync::oneshot::channel();
    let gate_host = Arc::new(AsyncGateHost {
        gate_rx: Arc::new(tokio::sync::Mutex::new(Some(rx_allow))),
        started_tx: Arc::new(tokio::sync::Mutex::new(Some(started_tx_allow))),
    });
    let workspace = Workspace::new(dir.path(), gate_host).unwrap();
    let target_file = dir.path().join("allow.txt");
    let args = serde_json::from_value(serde_json::json!({
        "operation": "write",
        "path": "allow.txt",
        "value": "hello async approval"
    }))
    .unwrap();

    let join_handle = tokio::spawn({
        let ws = workspace;
        async move { ws.execute(args).await }
    });

    // 等待明确通知到达 gate
    started_rx_allow.await.unwrap();
    assert!(
        !join_handle.is_finished(),
        "任务应在审批挂起期间处于 pending"
    );
    assert!(!target_file.exists(), "审批挂起期间文件绝对不能被修改");

    // 允许执行
    tx_allow.send(Ok(())).unwrap();
    let exec_res = join_handle.await.unwrap();
    assert!(exec_res.is_ok());
    assert!(target_file.exists());
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        "hello async approval"
    );

    // 2. Deny 流程：oneshot 发送 Err，审批前未写，拒绝后无副作用
    let dir_deny = tempfile::tempdir().unwrap();
    let (tx_deny, rx_deny) = tokio::sync::oneshot::channel();
    let (started_tx_deny, started_rx_deny) = tokio::sync::oneshot::channel();
    let gate_host_deny = Arc::new(AsyncGateHost {
        gate_rx: Arc::new(tokio::sync::Mutex::new(Some(rx_deny))),
        started_tx: Arc::new(tokio::sync::Mutex::new(Some(started_tx_deny))),
    });
    let workspace_deny = Workspace::new(dir_deny.path(), gate_host_deny).unwrap();
    let target_file_deny = dir_deny.path().join("denied.txt");
    let args_deny = serde_json::from_value(serde_json::json!({
        "operation": "write",
        "path": "denied.txt",
        "value": "should never be written"
    }))
    .unwrap();

    let join_deny = tokio::spawn({
        let ws = workspace_deny;
        async move { ws.execute(args_deny).await }
    });

    // 等待明确通知到达 gate
    started_rx_deny.await.unwrap();
    assert!(!join_deny.is_finished());
    assert!(!target_file_deny.exists());

    tx_deny.send(Err("user rejected".into())).unwrap();
    let exec_res_deny = join_deny.await.unwrap();
    assert!(exec_res_deny.is_err());
    assert_eq!(exec_res_deny.unwrap_err(), "user rejected");
    assert!(!target_file_deny.exists(), "审批拒绝后无写副作用");
}
