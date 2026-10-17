//! DES-17 共同 workspace activity 租约。
//!
//! 这是外置输出生命周期中**唯一**用于表达「这个工作区里是否有活动运行或引用写入者」
//! 的跨进程 `flock`。它由三方共同参与，缺一不可：
//!
//! 1. **生产 run 生命周期**：`src/workspace.rs` 的 `Workspace::new` **无论
//!    `.mio/outputs` 是否已经存在**都取得本模块的 shared 租约，并持有到该 run 的
//!    `Workspace` 最后一个克隆被丢弃。因此「run 构造之后才出现第一次 capture」同样在
//!    保护期内。
//! 2. **引用写入者**：`src/output_store.rs` 的 `guard_existing`（`Session::save` 与
//!    `journal::persist` 的落盘临界区）与捕获事务 `begin_capture` 都持有同一个 shared
//!    租约。
//! 3. **显式离线清理**：`begin_cleanup` 在扫描与删除的整个区间持有对应工作区的
//!    exclusive 租约。
//!
//! ## 锁文件的位置（关键修复）
//!
//! 锁文件**不放在项目目录里**，而是放在项目之外的运行目录：
//!
//! - 键由「规范化工作区的**原生路径字节** + 当前 euid」经 SHA-256 得到（见
//!   [`workspace_activity_key`]），因此每个工作区一把唯一锁，既不会因为小 stripe
//!   池让无关任务互相 `Busy`，也不会在纯问答时于项目根或 `.mio` 里新增任何文件。
//! - 运行目录**优先**使用 Linux 可信的 `/run/user/<euid>`；不可信或不存在时回退到
//!   系统 `/tmp` 下的私有目录 `mio-output-runtime-<euid>`（0700、owner 校验）。
//! - 路径**独立于** `MIO_CONFIG_DIR` 与 `TMPDIR`：这里使用硬编码的 `/run/user` 与
//!   `/tmp`，不读取 `std::env::temp_dir()`，因此「不同 profile / 不同 tmp 环境」解析出
//!   同一把锁。
//!
//! ## 安全校验
//!
//! 目录与锁文件都要求是 owner 为当前 euid、不可被 group/other 写入、且不是符号链接的
//! 真实路径；打开使用 `O_NOFOLLOW | O_CLOEXEC`（目录额外 `O_DIRECTORY`），锁文件以
//! `openat` 相对已校验的目录句柄创建，避免校验后被替换。租约获取一律 `LOCK_NB`
//! （`flock` 非阻塞），因此不会出现环状等待，也不会阻塞同步 Host 回调。
//!
//! **锁顺序（固定，避免死锁）**：所有参与者都按
//! `activity.lock` → `cleanup.lock`（`src/output_store.rs` 的产物租约）→ `index.lock`
//! 的顺序获取锁。写者与清理都用非阻塞 `LOCK_NB`：清理在存在 shared 租约时立即得到
//! `Busy`，而不是排队等待。`src/output_store.rs` 只通过本模块拿 activity 租约，因此
//! 顺序在这一处收敛。
//!
//! ## 宿主（SDK）契约
//!
//! 只提供内存历史、跨 run 复用的宿主，如果不经过 `Workspace`（也从不触发
//! `Session::save` / journal 落盘），就必须**显式**调用 [`shared_for_workspace`] 并持有
//! 返回的 [`ActivityLease`] 覆盖「可能引用外置输出的整个生命周期」。本模块只能证明
//! 「持租约者存在」，不能推断宿主私有的外部 memory refs；静态离线清理由此是协作式准入，
//! 不是对所有第三方进程的强制隔离。
//!
//! ## 非 Unix
//!
//! `flock` 语义不可用：[`exclusive_for_workspace`]（与显式清理）明确返回
//! [`ActivityError::Unsupported`]，[`shared_for_workspace`] 返回一个不含锁的守卫，
//! **不伪造互斥**（该平台上清理同样不可用）。

#![allow(dead_code)] // 阻塞变体供清理/写者的互斥测试与工具使用，生产路径只用非阻塞。

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// 旧式「状态目录内 activity 租约」文件名。
///
/// 生产路径已改为按工作区定位的项目外锁（见 [`shared_for_workspace`] /
/// [`exclusive_for_workspace`]）；该常量与 [`try_shared`] / [`exclusive_blocking`] 等
/// 显式路径 API 一起保留，供仍直接操作某个锁文件的调用方与互斥测试使用。
pub const ACTIVITY_LOCK_FILE: &str = "activity.lock";

/// 项目外运行目录的基名；回退路径形如 `/tmp/mio-output-runtime-<euid>`。
pub const RUNTIME_DIR_PREFIX: &str = "mio-output-runtime";

/// activity 租约失败的原因。全部是保守拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityError {
    /// 底层文件系统错误。
    Io(String),
    /// 锁文件/运行目录是符号链接、非普通文件、owner 或权限不符合要求。
    Invalid(String),
    /// 已存在冲突的租约（由非阻塞获取路径返回 `None`，这里保留给显式路径工具）。
    Busy(String),
    /// 当前平台不支持该操作（非 Unix 的 exclusive 租约）。
    Unsupported(String),
}

impl std::fmt::Display for ActivityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message)
            | Self::Invalid(message)
            | Self::Busy(message)
            | Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ActivityError {}

/// 持有 shared 或 exclusive activity 租约的守卫；`Drop`（关闭文件描述符）即释放。
///
/// `file` 在不支持 `flock` 的平台上是 `None`：那时返回守卫只是沿用调用方语义，并不
/// 表示已经取得互斥。
#[must_use = "dropping the guard releases the workspace activity lease"]
pub struct ActivityLease {
    path: PathBuf,
    file: Option<File>,
}

impl ActivityLease {
    /// 该租约对应的锁文件路径，便于调用方核对。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 当前进程的有效 UID；非 Unix 平台固定为 0（该平台没有 `flock`）。
fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// 由「规范化工作区的原生路径字节 + euid」得到的稳定十六进制键。
///
/// 这是纯函数：不读 `MIO_CONFIG_DIR` / `TMPDIR` / 当前工作目录，也不触碰文件系统。
/// 同一个规范化路径在任何 profile 或 tmp 环境下都得到同一个键，不同工作区得到不同键。
pub fn workspace_activity_key(canonical_workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_workspace.as_os_str().as_encoded_bytes());
    hasher.update([0u8]);
    hasher.update(effective_uid().to_le_bytes());
    let digest = hasher.finalize();
    let mut key = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        write!(key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    key
}

/// 该工作区唯一锁文件的文件名（`<key>.lock`）。
pub fn workspace_lock_file_name(canonical_workspace: &Path) -> String {
    format!("{}.lock", workspace_activity_key(canonical_workspace))
}

/// 解析（必要时创建）当前 euid 的运行目录。
///
/// 优先使用 Linux 可信的 `/run/user/<euid>`；不可信、不存在或非 Linux 时使用系统
/// `/tmp` 下的私有目录 `mio-output-runtime-<euid>`（0700、owner 校验）。路径不读取
/// `MIO_CONFIG_DIR` / `TMPDIR`。
pub fn runtime_directory() -> Result<PathBuf, ActivityError> {
    #[cfg(not(unix))]
    {
        Err(ActivityError::Unsupported(
            "workspace activity leases require Unix flock; this platform is not supported".into(),
        ))
    }
    #[cfg(unix)]
    {
        let uid = effective_uid();
        let preferred = Path::new("/run/user").join(uid.to_string());
        if trusted_shared_runtime_dir(&preferred) {
            return Ok(preferred);
        }
        let fallback = Path::new("/tmp").join(format!("{RUNTIME_DIR_PREFIX}-{uid}"));
        ensure_private_runtime_dir(&fallback)?;
        Ok(fallback)
    }
}

/// 某个工作区唯一锁文件的绝对路径。
///
/// 会先规范化工作区根；工作区不存在时返回 [`ActivityError::Io`]。
pub fn lock_path_for_workspace(workspace_root: &Path) -> Result<PathBuf, ActivityError> {
    let canonical = canonicalize_workspace(workspace_root)?;
    Ok(runtime_directory()?.join(workspace_lock_file_name(&canonical)))
}

/// 尝试取得某个工作区唯一的 shared activity 租约；存在 exclusive 租约时返回 `Ok(None)`。
///
/// 这是生产 run、捕获事务与引用写入者共同使用的入口。非 Unix 平台返回一个不含锁的
/// 守卫（该平台清理本身不可用，不伪造互斥）。
pub fn shared_for_workspace(workspace_root: &Path) -> Result<Option<ActivityLease>, ActivityError> {
    #[cfg(not(unix))]
    {
        Ok(Some(ActivityLease {
            path: workspace_root.to_path_buf(),
            file: None,
        }))
    }
    #[cfg(unix)]
    {
        let canonical = canonicalize_workspace(workspace_root)?;
        let name = workspace_lock_file_name(&canonical);
        let dir = runtime_directory()?;
        let path = dir.join(&name);
        let file = open_runtime_lock(&dir, &name)?;
        if !flock(&file, &path, libc::LOCK_SH | libc::LOCK_NB)? {
            return Ok(None);
        }
        Ok(Some(ActivityLease {
            path,
            file: Some(file),
        }))
    }
}

/// 尝试取得某个工作区唯一的 exclusive activity 租约；存在任何其它租约时返回 `Ok(None)`。
///
/// 离线清理由此取得独占保护。非 Unix 平台明确返回 [`ActivityError::Unsupported`]。
pub fn exclusive_for_workspace(
    workspace_root: &Path,
) -> Result<Option<ActivityLease>, ActivityError> {
    #[cfg(not(unix))]
    {
        let _ = workspace_root;
        Err(ActivityError::Unsupported(
            "offline output cleanup requires Unix flock; this platform is not supported".into(),
        ))
    }
    #[cfg(unix)]
    {
        let canonical = canonicalize_workspace(workspace_root)?;
        let name = workspace_lock_file_name(&canonical);
        let dir = runtime_directory()?;
        let path = dir.join(&name);
        let file = open_runtime_lock(&dir, &name)?;
        if !flock(&file, &path, libc::LOCK_EX | libc::LOCK_NB)? {
            return Ok(None);
        }
        Ok(Some(ActivityLease {
            path,
            file: Some(file),
        }))
    }
}

/// 尝试取得 shared activity 租约（显式路径）；存在 exclusive 租约时返回 `Ok(None)`。
///
/// 非 Unix 平台返回一个不含锁的守卫而不是伪造互斥。
pub fn try_shared(path: &Path) -> Result<Option<ActivityLease>, ActivityError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(Some(ActivityLease {
            path: path.to_path_buf(),
            file: None,
        }))
    }
    #[cfg(unix)]
    {
        let file = open_lock_file(path)?;
        if !flock(&file, path, libc::LOCK_SH | libc::LOCK_NB)? {
            return Ok(None);
        }
        Ok(Some(ActivityLease {
            path: path.to_path_buf(),
            file: Some(file),
        }))
    }
}

/// 尝试取得 exclusive activity 租约（显式路径）；存在任何其它租约时返回 `Ok(None)`。
///
/// 非 Unix 平台明确返回 [`ActivityError::Unsupported`]，不静默降级。
pub fn try_exclusive(path: &Path) -> Result<Option<ActivityLease>, ActivityError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ActivityError::Unsupported(
            "offline output cleanup requires Unix flock; this platform is not supported".into(),
        ))
    }
    #[cfg(unix)]
    {
        let file = open_lock_file(path)?;
        if !flock(&file, path, libc::LOCK_EX | libc::LOCK_NB)? {
            return Ok(None);
        }
        Ok(Some(ActivityLease {
            path: path.to_path_buf(),
            file: Some(file),
        }))
    }
}

/// 阻塞取得 shared activity 租约（显式路径），直到能够取得为止。
///
/// 生产写者不使用阻塞获取（同步 Host 回调不得阻塞）；它用于工具与互斥测试，证明租约
/// 是真正的串行化而不是靠 `sleep` 观察。
pub fn shared_blocking(path: &Path) -> Result<ActivityLease, ActivityError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(ActivityLease {
            path: path.to_path_buf(),
            file: None,
        })
    }
    #[cfg(unix)]
    {
        let file = open_lock_file(path)?;
        flock(&file, path, libc::LOCK_SH)?;
        Ok(ActivityLease {
            path: path.to_path_buf(),
            file: Some(file),
        })
    }
}

/// 阻塞取得 exclusive activity 租约（显式路径），直到所有 shared/exclusive 租约都释放为止。
///
/// 同样只用于工具与互斥测试；清理走非阻塞的 [`try_exclusive`]。
pub fn exclusive_blocking(path: &Path) -> Result<ActivityLease, ActivityError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(ActivityError::Unsupported(
            "offline output cleanup requires Unix flock; this platform is not supported".into(),
        ))
    }
    #[cfg(unix)]
    {
        let file = open_lock_file(path)?;
        flock(&file, path, libc::LOCK_EX)?;
        Ok(ActivityLease {
            path: path.to_path_buf(),
            file: Some(file),
        })
    }
}

fn canonicalize_workspace(workspace_root: &Path) -> Result<PathBuf, ActivityError> {
    workspace_root.canonicalize().map_err(|error| {
        ActivityError::Io(format!(
            "cannot canonicalize workspace root {}: {error}",
            workspace_root.display()
        ))
    })
}

/// 以 0600、`O_NOFOLLOW | O_CLOEXEC` 打开（必要时创建）锁文件，拒绝符号链接与非普通文件。
fn open_lock_file(path: &Path) -> Result<File, ActivityError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(ActivityError::Invalid(format!(
            "{} must not be a symlink",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|error| {
        ActivityError::Io(format!(
            "cannot open workspace activity lock {}: {error}",
            path.display()
        ))
    })?;
    if !file
        .metadata()
        .map_err(|error| ActivityError::Io(error.to_string()))?
        .is_file()
    {
        return Err(ActivityError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// `/run/user/<euid>` 是否可信：真实目录、非符号链接、owner 为当前 euid、group/other 不可访问。
#[cfg(unix)]
fn trusted_shared_runtime_dir(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == effective_uid()
                && metadata.permissions().mode() & 0o077 == 0
        }
        Err(_) => false,
    }
}

/// 确保回退运行目录是 0700、owner 为当前 euid 的真实目录；任何不符合都 fail-closed。
#[cfg(unix)]
fn ensure_private_runtime_dir(dir: &Path) -> Result<(), ActivityError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ActivityError::Invalid(format!(
                    "workspace activity runtime {} must be a real directory, not a symlink",
                    dir.display()
                )));
            }
            if metadata.uid() != effective_uid() {
                return Err(ActivityError::Invalid(format!(
                    "workspace activity runtime {} is not owned by uid {}; refusing a shared lock directory",
                    dir.display(),
                    effective_uid()
                )));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(ActivityError::Io(format!(
                        "cannot create workspace activity runtime {}: {error}",
                        dir.display()
                    )));
                }
            }
            let metadata = fs::symlink_metadata(dir).map_err(io_error)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ActivityError::Invalid(format!(
                    "workspace activity runtime {} must be a real directory, not a symlink",
                    dir.display()
                )));
            }
            if metadata.uid() != effective_uid() {
                return Err(ActivityError::Invalid(format!(
                    "workspace activity runtime {} is not owned by uid {}; refusing a shared lock directory",
                    dir.display(),
                    effective_uid()
                )));
            }
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
            Ok(())
        }
        Err(error) => Err(io_error(error)),
    }
}

/// 打开运行目录句柄（`O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`）并校验 owner/权限。
#[cfg(unix)]
fn open_runtime_dir(dir: &Path) -> Result<File, ActivityError> {
    use std::os::unix::io::FromRawFd;
    let dir_c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
        ActivityError::Invalid(format!(
            "workspace activity runtime {} contains a NUL byte",
            dir.display()
        ))
    })?;
    // SAFETY: `dir_c` is a valid NUL-terminated path and the flags are constants.
    let fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            ActivityError::Invalid(format!(
                "workspace activity runtime {} must not be a symlink",
                dir.display()
            ))
        } else {
            ActivityError::Io(format!(
                "cannot open workspace activity runtime {}: {error}",
                dir.display()
            ))
        });
    }
    // SAFETY: `fd` is a freshly opened, owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    validate_runtime_dir(&file, dir)?;
    Ok(file)
}

/// 校验已打开的运行目录句柄：真实目录、owner 为当前 euid、group/other 不可访问。
#[cfg(unix)]
fn validate_runtime_dir(file: &File, dir: &Path) -> Result<(), ActivityError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_dir() {
        return Err(ActivityError::Invalid(format!(
            "workspace activity runtime {} must be a directory",
            dir.display()
        )));
    }
    if metadata.uid() != effective_uid() {
        return Err(ActivityError::Invalid(format!(
            "workspace activity runtime {} is not owned by uid {}; refusing a shared lock directory",
            dir.display(),
            effective_uid()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ActivityError::Invalid(format!(
            "workspace activity runtime {} must not be group/other accessible",
            dir.display()
        )));
    }
    Ok(())
}

/// 相对已校验的目录句柄创建/打开 0600 锁文件：`O_NOFOLLOW | O_CLOEXEC`，owner 校验。
#[cfg(unix)]
fn open_runtime_lock(dir: &Path, name: &str) -> Result<File, ActivityError> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let path = dir.join(name);
    let dir_file = open_runtime_dir(dir)?;
    let name_c = std::ffi::CString::new(name).map_err(|_| {
        ActivityError::Invalid("workspace activity lock name contains a NUL byte".into())
    })?;
    // SAFETY: `dir_file` is a valid directory descriptor, `name_c` is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            dir_file.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            ActivityError::Invalid(format!("{} must not be a symlink", path.display()))
        } else {
            ActivityError::Io(format!(
                "cannot open workspace activity lock {}: {error}",
                path.display()
            ))
        });
    }
    // SAFETY: `fd` is a freshly opened, owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    // fchmod defeats a permissive umask so the lock is always private.
    // SAFETY: `file` keeps a valid descriptor alive for the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600 as libc::mode_t) } != 0 {
        return Err(ActivityError::Io(format!(
            "cannot set 0600 on workspace activity lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(ActivityError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != effective_uid() {
            return Err(ActivityError::Invalid(format!(
                "{} is not owned by uid {}",
                path.display(),
                effective_uid()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ActivityError::Invalid(format!(
                "{} must not be group/other accessible",
                path.display()
            )));
        }
    }
    Ok(file)
}

fn io_error(error: std::io::Error) -> ActivityError {
    ActivityError::Io(error.to_string())
}

#[cfg(all(test, unix))]
mod runtime_directory_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn shared_user_runtime_requires_private_permissions_without_changing_them() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o755, 0o750, 0o770] {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
            assert!(!trusted_shared_runtime_dir(dir.path()));
            let handle = File::open(dir.path()).unwrap();
            assert!(matches!(
                validate_runtime_dir(&handle, dir.path()),
                Err(ActivityError::Invalid(_))
            ));
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(trusted_shared_runtime_dir(dir.path()));
    }
}

#[cfg(unix)]
fn flock(file: &File, path: &Path, operation: libc::c_int) -> Result<bool, ActivityError> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `file` keeps a valid descriptor alive for the whole call.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        let code = error.raw_os_error();
        if code == Some(libc::EAGAIN) || code == Some(libc::EWOULDBLOCK) {
            return Ok(false);
        }
        return Err(ActivityError::Io(format!(
            "cannot lock workspace activity file {}: {error}",
            path.display()
        )));
    }
}
