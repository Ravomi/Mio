//! 基于目录句柄的工作区文件系统原语的真实文件系统测试。
//!
//! 该文件通过 `#[path]` 直接编译 `src/workspace_fs.rs`，因此在 `lib.rs` 导出该模块
//! 之前即可独立运行：`cargo test --locked --test workspace_fs_pinning`。
//!
//! 断言基于真实 inode 与真实 `rename` / `symlink` / `mkfifo`，而不是 mock：固定父目录
//! fd 之后再把父目录改名并在原位置放外部符号链接，读写与删除必须只作用在原 inode 上，
//! 外部 sentinel 不变且不产生临时产物。FIFO 用例不用线程挂住主测试，而是以有界子进程
//! 复跑本测试二进制来证明 `O_NONBLOCK` 生效。
#![cfg(unix)]

#[path = "../src/workspace_fs.rs"]
mod workspace_fs;

use std::{
    fs::{self, File, Permissions},
    io::Read,
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use workspace_fs::{TEMP_FILE_PREFIX, WorkspaceRoot};

/// 一个隔离的临时基目录，内部含 `root/` 与 `external/`，两者都已规范化。
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let base = tempfile::tempdir().unwrap();
    let base_path = fs::canonicalize(base.path()).unwrap();
    let root = base_path.join("root");
    let external = base_path.join("external");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&external).unwrap();
    (base, root, external)
}

fn contents(file: &mut File) -> String {
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    text
}

/// 目录中所有以私有临时前缀命名且已排序的条目。
fn temp_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(TEMP_FILE_PREFIX))
        .collect();
    names.sort();
    names
}

#[test]
fn pin_rejects_absolute_parent_and_empty_paths() {
    let (_base, root, _external) = fixture();
    let ws = WorkspaceRoot::open(&root).unwrap();
    assert_eq!(ws.root_path(), root.as_path());

    for relative in [
        "",
        ".",
        "..",
        "../escape",
        "a/../b",
        "/etc/passwd",
        "sub/../../..",
    ] {
        assert!(
            ws.pin(relative).is_err(),
            "pin({relative:?}) must be rejected"
        );
    }

    assert!(WorkspaceRoot::open(Path::new("relative/root")).is_err());
    assert!(WorkspaceRoot::open(Path::new("")).is_err());
    assert!(WorkspaceRoot::open(Path::new("/tmp/not/../normalized")).is_err());
}

#[test]
fn operations_bind_to_the_original_parent_inode() {
    let (_base, root, external) = fixture();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/target.txt"), b"original").unwrap();
    fs::write(external.join("target.txt"), b"external").unwrap();

    let ws = WorkspaceRoot::open(&root).unwrap();
    let pinned = ws.pin("sub/target.txt").unwrap();
    assert!(pinned.path().ends_with("sub/target.txt"));

    let original_ino = pinned.metadata().unwrap().ino();
    assert_eq!(
        original_ino,
        fs::metadata(root.join("sub/target.txt")).unwrap().ino()
    );
    assert_eq!(contents(&mut pinned.open_read().unwrap()), "original");

    // 父目录被改名，原位置换成指向外部的符号链接。
    let moved_dir = root.join("sub_moved");
    let moved_file = moved_dir.join("target.txt");
    fs::rename(root.join("sub"), &moved_dir).unwrap();
    symlink(&external, root.join("sub")).unwrap();

    assert_eq!(contents(&mut pinned.open_read().unwrap()), "original");
    assert_eq!(pinned.metadata().unwrap().ino(), original_ino);
    assert_eq!(fs::read(external.join("target.txt")).unwrap(), b"external");

    pinned
        .write_atomic(b"updated", Some(Permissions::from_mode(0o640)))
        .unwrap();
    assert_eq!(contents(&mut pinned.open_read().unwrap()), "updated");
    assert_eq!(fs::read(&moved_file).unwrap(), b"updated");
    assert_eq!(
        fs::metadata(&moved_file).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_ne!(pinned.metadata().unwrap().ino(), original_ino);
    assert_eq!(fs::read(external.join("target.txt")).unwrap(), b"external");
    assert!(temp_entries(&external).is_empty());

    assert!(pinned.remove().unwrap());
    assert!(!moved_file.exists());
    assert!(external.join("target.txt").exists());
    assert!(!pinned.remove().unwrap());
    assert!(temp_entries(&external).is_empty());
}

#[test]
fn root_path_swap_still_binds_original_inode() {
    let (_base, root, external) = fixture();
    fs::write(root.join("file.txt"), b"original").unwrap();
    fs::write(external.join("file.txt"), b"external").unwrap();

    let ws = WorkspaceRoot::open(&root).unwrap();
    let pinned = ws.pin("file.txt").unwrap();
    let original_ino = pinned.metadata().unwrap().ino();

    // 根目录被改名，原 root 路径换成指向外部的符号链接。
    let moved_root = root.with_file_name("root_moved");
    let moved_file = moved_root.join("file.txt");
    fs::rename(&root, &moved_root).unwrap();
    symlink(&external, &root).unwrap();

    assert_eq!(contents(&mut pinned.open_read().unwrap()), "original");
    assert_eq!(pinned.metadata().unwrap().ino(), original_ino);

    pinned.write_atomic(b"updated", None).unwrap();
    assert_eq!(fs::read(&moved_file).unwrap(), b"updated");
    // 未显式给权限时保持私有 0600。
    assert_eq!(
        fs::metadata(&moved_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(external.join("file.txt")).unwrap(), b"external");

    // 新的 root 路径现在是符号链接：逐组件 `O_NOFOLLOW` 必须拒绝。
    assert!(WorkspaceRoot::open(&root).is_err());

    assert!(pinned.remove().unwrap());
    assert!(!moved_file.exists());
    assert!(external.join("file.txt").exists());
    assert!(temp_entries(&external).is_empty());
}

#[test]
fn leaf_symlinks_are_never_followed() {
    let (_base, root, external) = fixture();
    fs::write(external.join("sentinel.txt"), b"sentinel").unwrap();
    symlink(external.join("sentinel.txt"), root.join("link.txt")).unwrap();

    let ws = WorkspaceRoot::open(&root).unwrap();
    let pinned = ws.pin("link.txt").unwrap();
    assert!(pinned.open_read().is_err());
    assert!(pinned.read_optional(1024).is_err());
    assert!(pinned.metadata().is_err());
    assert_eq!(
        fs::read(external.join("sentinel.txt")).unwrap(),
        b"sentinel"
    );

    // 原子替换替换的是符号链接条目本身，绝不跟随到外部目标。
    pinned
        .write_atomic(b"replaced", Some(Permissions::from_mode(0o644)))
        .unwrap();
    assert_eq!(contents(&mut pinned.open_read().unwrap()), "replaced");
    assert!(
        !fs::symlink_metadata(root.join("link.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(external.join("sentinel.txt")).unwrap(),
        b"sentinel"
    );

    // 重新放回符号链接后删除：只删除链接，不动外部文件。
    fs::remove_file(root.join("link.txt")).unwrap();
    symlink(external.join("sentinel.txt"), root.join("link.txt")).unwrap();
    assert!(pinned.remove().unwrap());
    assert!(fs::symlink_metadata(root.join("link.txt")).is_err());
    assert!(external.join("sentinel.txt").exists());
    assert!(!pinned.remove().unwrap());

    // 目录也不是常规文件。
    fs::create_dir(root.join("dir")).unwrap();
    let dir_pin = ws.pin("dir").unwrap();
    assert!(dir_pin.open_read().is_err());
    assert!(dir_pin.metadata().is_err());
}

#[test]
fn read_optional_only_treats_absence_as_missing() {
    let (_base, root, _external) = fixture();
    let ws = WorkspaceRoot::open(&root).unwrap();
    let pinned = ws.pin("missing.txt").unwrap();
    assert!(pinned.open_read().is_err());
    assert_eq!(pinned.read_optional(1024).unwrap(), None);
    assert!(pinned.metadata().is_err());
    assert!(!pinned.remove().unwrap());

    fs::write(root.join("missing.txt"), b"here").unwrap();
    assert_eq!(pinned.read_optional(4).unwrap(), Some(b"here".to_vec()));
    assert_eq!(
        pinned.read_optional(3).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(pinned.remove().unwrap());
}

#[test]
fn write_atomic_failure_cleans_its_temp_file() {
    let (_base, root, _external) = fixture();
    let keep = format!("{TEMP_FILE_PREFIX}keep.tmp");
    fs::write(root.join(&keep), b"keep").unwrap();
    fs::create_dir(root.join("blocked")).unwrap();

    let ws = WorkspaceRoot::open(&root).unwrap();
    let pinned = ws.pin("blocked").unwrap();
    // 用普通文件 rename 覆盖目录在 Linux 上以 `EISDIR` 失败，触发守卫清理临时文件。
    assert!(pinned.write_atomic(b"nope", None).is_err());

    assert!(root.join("blocked").is_dir());
    assert_eq!(fs::read(root.join(&keep)).unwrap(), b"keep");
    assert_eq!(temp_entries(&root), vec![keep.clone()]);

    // 成功路径也不残留临时文件，也不会误删同前缀的无关文件。
    let ok = ws.pin("ok.txt").unwrap();
    ok.write_atomic(b"ok", None).unwrap();
    assert_eq!(contents(&mut ok.open_read().unwrap()), "ok");
    assert_eq!(temp_entries(&root), vec![keep]);
}

#[test]
fn descriptors_are_close_on_exec() {
    let (_base, root, _external) = fixture();
    fs::write(root.join("file.txt"), b"payload").unwrap();

    let ws = WorkspaceRoot::open(&root).unwrap();
    // SAFETY: `root_fd` is a live descriptor owned by `ws`.
    assert_ne!(
        unsafe { libc::fcntl(ws.root_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );

    let pinned = ws.pin("file.txt").unwrap();
    let file = pinned.open_read().unwrap();
    // SAFETY: `file` holds a live descriptor for the duration of the check.
    assert_ne!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
}

#[test]
fn fifo_leaf_is_rejected_without_blocking() {
    if let Some(path) = std::env::var_os("MIO_WORKSPACE_FS_FIFO_CHILD") {
        probe_fifo_child(PathBuf::from(path));
    }

    let (_base, root, _external) = fixture();
    let fifo = root.join("pipe");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `fifo_c` is a valid NUL-terminated path and the mode is a constant.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

    let executable = std::env::current_exe().unwrap();
    let mut child = Command::new(executable)
        .args([
            "--exact",
            "fifo_leaf_is_rejected_without_blocking",
            "--nocapture",
        ])
        .env("MIO_WORKSPACE_FS_FIFO_CHILD", &fifo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert!(status.success(), "FIFO child reported {status:?}");
                return;
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("open_read blocked on a FIFO leaf");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 子进程分支：固定 FIFO 的父目录并确认只读打开被拒绝而不是阻塞。
fn probe_fifo_child(fifo: PathBuf) -> ! {
    let parent = fifo.parent().expect("fifo path has a parent").to_path_buf();
    let leaf = fifo.file_name().expect("fifo path has a file name");
    let leaf = leaf.to_str().expect("fifo leaf is valid UTF-8");
    let root = WorkspaceRoot::open(&parent).expect("open fixed root in child");
    let pinned = root.pin(leaf).expect("pin fifo in child");
    let rejected_open = pinned.open_read().is_err();
    let rejected_optional = pinned.read_optional(1024).is_err();
    std::process::exit(if rejected_open && rejected_optional {
        0
    } else {
        3
    });
}
