//! 工作区**根目录扫描别名**的回归测试。
//!
//! 真实用户 journal 里的缺陷是：`grep` 的 `path` 写成 `"."` 时被拒绝为
//! `path must name something below the workspace root`，而省略 `path` 却能正常扫描整个
//! 工作区；同一意图因为写法不同得到假拒绝。本测试用真实临时目录验证：
//!
//! - `grep` / `find` / `ls` 的空值、`"."`、`"./"`、`"./."` 与省略 `path` 结果一致；
//! - 嵌套相对目录（`src`、`./src`、单个文件）仍然正常收窄扫描；
//! - 拒绝边界不变：写/读/改的根目录别名、绝对路径、`..` 与符号链接逃逸仍被拒绝，
//!   且根目录扫描不会跟随符号链接读到工作区之外。
//!
//! 这里只断言公开 API（`Workspace::execute`）的可观察行为，不依赖内部实现。

#![cfg(unix)]

use mio_coding_agent::{Arguments, Host, HostFuture, Workspace};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::Arc,
};

struct QuietHost;

impl Host for QuietHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
}

/// 隔离 fixture：`root/` 是工作区，`external/` 在工作区之外，`root/link_out` 与
/// `root/link_file` 是指向它的符号链接（逃逸诱饵）。
struct Fixture {
    _base: tempfile::TempDir,
    root: PathBuf,
    external: PathBuf,
}

fn fixture() -> Fixture {
    let base = tempfile::tempdir().expect("temp base");
    let base_path = fs::canonicalize(base.path()).expect("canonical base");
    let root = base_path.join("root");
    let external = base_path.join("external");
    fs::create_dir(&root).expect("workspace root");
    fs::create_dir(&external).expect("external dir");

    fs::write(root.join("top.txt"), "needle at top\nsecond line\n").expect("top.txt");
    fs::write(root.join("other.txt"), "haystack only\n").expect("other.txt");
    fs::create_dir(root.join("src")).expect("src dir");
    fs::write(root.join("src/nested.txt"), "needle in src\n").expect("nested.txt");
    fs::create_dir(root.join("src/deep")).expect("deep dir");
    fs::write(root.join("src/deep/deeper.txt"), "needle deep\n").expect("deeper.txt");

    fs::write(external.join("secret.txt"), "needle outside workspace\n").expect("secret.txt");
    symlink(&external, root.join("link_out")).expect("symlink dir");
    symlink(external.join("secret.txt"), root.join("link_file")).expect("symlink file");

    Fixture {
        _base: base,
        root,
        external,
    }
}

fn workspace(fixture: &Fixture) -> Workspace {
    Workspace::new(&fixture.root, Arc::new(QuietHost)).expect("workspace")
}

/// 根目录别名写法：空值与纯空白按既有行为视作省略 `path`。
const ROOT_ALIASES: &[&str] = &["", ".", "./", "./.", ".///", "   "];

#[tokio::test]
async fn grep_find_ls_root_aliases_match_omitted_path() {
    let fixture = fixture();
    let ws = workspace(&fixture);

    // grep：省略 path 与所有根目录别名逐字一致。
    let grep_omitted = ws
        .execute(Arguments::grep("needle", None))
        .await
        .expect("grep with omitted path scans the workspace root");
    assert!(grep_omitted.contains("top.txt:1: needle at top"));
    assert!(grep_omitted.contains("src/nested.txt:1: needle in src"));
    assert!(grep_omitted.contains("src/deep/deeper.txt:1: needle deep"));
    assert!(
        !grep_omitted.contains("outside workspace"),
        "root scan must not follow symlinks out of the workspace: {grep_omitted}"
    );

    for alias in ROOT_ALIASES {
        let result = ws
            .execute(Arguments::grep("needle", Some((*alias).to_string())))
            .await
            .unwrap_or_else(|error| {
                panic!("grep path={alias:?} must behave like an omitted path, got error: {error}")
            });
        assert_eq!(
            result, grep_omitted,
            "grep path={alias:?} must equal the omitted-path result"
        );
    }

    // find：同样必须与省略 path 一致，且不跟随逃逸符号链接。
    let find_omitted = ws
        .execute(Arguments::find(".txt", None))
        .await
        .expect("find with omitted path scans the workspace root");
    assert!(find_omitted.contains("top.txt"));
    assert!(find_omitted.contains("src/deep/deeper.txt"));
    assert!(!find_omitted.contains("outside workspace"));

    for alias in ROOT_ALIASES {
        let result = ws
            .execute(Arguments::find(".txt", Some((*alias).to_string())))
            .await
            .unwrap_or_else(|error| {
                panic!("find path={alias:?} must behave like an omitted path, got error: {error}")
            });
        assert_eq!(
            result, find_omitted,
            "find path={alias:?} must equal the omitted-path result"
        );
    }

    // ls：列出的是工作区根，符号链接只作为条目名出现，绝不进入 external/。
    let ls_omitted = ws
        .execute(Arguments::ls(None))
        .await
        .expect("ls with omitted path lists the workspace root");
    assert!(ls_omitted.contains("top.txt"));
    assert!(ls_omitted.contains("src/"));
    assert!(ls_omitted.contains("link_out@"));
    assert!(!ls_omitted.contains("secret.txt"));

    for alias in ROOT_ALIASES {
        let result = ws
            .execute(Arguments::ls(Some((*alias).to_string())))
            .await
            .unwrap_or_else(|error| {
                panic!("ls path={alias:?} must behave like an omitted path, got error: {error}")
            });
        assert_eq!(
            result, ls_omitted,
            "ls path={alias:?} must equal the omitted-path result"
        );
    }
}

#[tokio::test]
async fn nested_relative_scans_remain_narrowed() {
    let fixture = fixture();
    let ws = workspace(&fixture);

    let grep_src = ws
        .execute(Arguments::grep("needle", Some("src".into())))
        .await
        .expect("grep in src");
    assert!(grep_src.contains("src/nested.txt:1: needle in src"));
    assert!(grep_src.contains("src/deep/deeper.txt:1: needle deep"));
    assert!(
        !grep_src.contains("top.txt"),
        "src scan must stay inside src: {grep_src}"
    );

    // `./src` 与 `src` 等价（既有行为），不是根目录别名。
    let grep_dotted = ws
        .execute(Arguments::grep("needle", Some("./src".into())))
        .await
        .expect("grep in ./src");
    assert_eq!(grep_dotted, grep_src);

    // 单个相对文件仍然可直接搜索。
    let grep_file = ws
        .execute(Arguments::grep("needle", Some("src/nested.txt".into())))
        .await
        .expect("grep a single file");
    assert_eq!(grep_file, "src/nested.txt:1: needle in src");

    let ls_deep = ws
        .execute(Arguments::ls(Some("src/deep".into())))
        .await
        .expect("ls src/deep");
    assert_eq!(ls_deep, "deeper.txt");
    assert_eq!(
        ws.execute(Arguments::ls(Some("./src/deep".into())))
            .await
            .expect("ls ./src/deep"),
        ls_deep
    );

    let find_src = ws
        .execute(Arguments::find(".txt", Some("src".into())))
        .await
        .expect("find in src");
    assert!(find_src.contains("src/nested.txt"));
    assert!(find_src.contains("src/deep/deeper.txt"));
    assert!(
        !find_src.contains("top.txt"),
        "find in src must stay inside src: {find_src}"
    );
}

#[tokio::test]
async fn root_alias_and_escape_rejections_are_unchanged() {
    let fixture = fixture();
    let ws = workspace(&fixture);

    // 读写改不因这次放宽而接受根目录别名：`.` 仍然不是可读/可写/可改的文件。
    for path in [".", "./", ".//"] {
        let write = ws
            .execute(Arguments::write(path, "x"))
            .await
            .expect_err("write to the workspace root alias must be rejected");
        assert!(!write.is_empty(), "write path={path:?} needs a reason");
        assert!(
            ws.execute(Arguments::read(path)).await.is_err(),
            "read path={path:?} must be rejected"
        );
        assert!(
            ws.execute(Arguments::edit(path, "old", "new"))
                .await
                .is_err(),
            "edit path={path:?} must be rejected"
        );
    }

    // 绝对路径仍然一律拒绝，包含指向工作区内部的绝对路径与 /tmp 输出路径。
    let absolute_workspace_path = fixture.root.join("top.txt");
    let outside_output = "/tmp/mio_mindmap.mmd";
    let value_error = ws
        .execute(Arguments::write(outside_output, "graph TD;"))
        .await
        .expect_err("absolute /tmp output must stay rejected");
    assert!(
        value_error.contains("必须使用相对路径"),
        "write to an absolute path needs the relative-path message, got: {value_error}"
    );
    for path in [
        "/".to_string(),
        "//".to_string(),
        "/./".to_string(),
        absolute_workspace_path.to_string_lossy().into_owned(),
        outside_output.to_string(),
    ] {
        for args in [
            Arguments::grep("needle", Some(path.clone())),
            Arguments::find(".txt", Some(path.clone())),
            Arguments::ls(Some(path.clone())),
        ]
        .into_iter()
        .chain([
            Arguments::read(path.clone()),
            Arguments::edit(path.clone(), "old", "new"),
        ]) {
            let error = ws
                .execute(args)
                .await
                .expect_err("absolute paths must be rejected everywhere");
            assert!(
                error.contains("必须使用相对路径"),
                "path={path:?} needs the relative-path message, got: {error}"
            );
        }
    }

    // `..` 父级遍历仍然拒绝。
    for path in ["..", "../", "../escape.txt", "src/../../escape.txt"] {
        let error = ws
            .execute(Arguments::ls(Some(path.into())))
            .await
            .expect_err("parent traversal must be rejected");
        assert!(
            error.contains("父目录"),
            "path={path:?} needs the parent-navigation message, got: {error}"
        );
        assert!(
            ws.execute(Arguments::grep("needle", Some(path.into())))
                .await
                .is_err()
        );
        assert!(
            ws.execute(Arguments::find(".txt", Some(path.into())))
                .await
                .is_err()
        );
        assert!(
            ws.execute(Arguments::write(path, "x")).await.is_err(),
            "write path={path:?} must be rejected"
        );
    }

    // 符号链接逃逸仍然拒绝：既包括扫描起点就是链接，也包括相对路径里带链接组件。
    assert!(
        ws.execute(Arguments::grep("needle", Some("link_out".into())))
            .await
            .is_err()
    );
    assert!(
        ws.execute(Arguments::find(".txt", Some("link_out".into())))
            .await
            .is_err()
    );
    assert!(
        ws.execute(Arguments::ls(Some("link_out".into())))
            .await
            .is_err()
    );
    assert!(
        ws.execute(Arguments::read("link_file")).await.is_err(),
        "reading a symlink must be rejected"
    );
    assert!(
        ws.execute(Arguments::read("link_out/secret.txt"))
            .await
            .is_err(),
        "reading through a symlinked directory must be rejected"
    );
    assert!(
        ws.execute(Arguments::write("link_out/new.txt", "x"))
            .await
            .is_err(),
        "writing through a symlinked directory must be rejected"
    );
    let escape_error = ws
        .execute(Arguments::grep(
            "needle",
            Some("link_out/secret.txt".into()),
        ))
        .await
        .expect_err("grep through a symlinked directory must be rejected");
    assert!(
        !escape_error.is_empty(),
        "symlink escape needs a reason, got an empty message"
    );

    // 外部 sentinel 未被读写污染。
    assert_eq!(
        fs::read_to_string(fixture.external.join("secret.txt")).unwrap(),
        "needle outside workspace\n"
    );
    assert!(!fixture.external.join("new.txt").exists());
}
