//! 基于目录句柄的工作区文件系统原语（Unix）。
//!
//! 本模块提供**能力式（capability）**的路径固定：把「校验路径」与「读写路径」之间的
//! 字符路径竞争，收窄为「已授权 inode」上的句柄操作。
//!
//! - [`WorkspaceRoot::open`] 从绝对、已规范的根开始，逐组件以
//!   `openat(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)` 打开，任一组件是符号链接即拒绝
//!   （`ELOOP`）。最终根以 `Arc<File>` 持有并在 `pin` 时共享。
//! - [`WorkspaceRoot::pin`] 拒绝绝对路径与 `..`，沿固定的根句柄逐组件解析**父目录**并
//!   持有其 fd，最后一个组件只作为单个 leaf 名保存，绝不回退为绝对字符串。
//! - [`PinnedPath`] 的读取 / 元数据 / 原子替换 / 删除都通过父目录 fd 与单个 leaf
//!   `CString` 发起 `openat` / `fstat` / `renameat` / `unlinkat`，不使用
//!   `File::open(absolute)` 或 `NamedTempFile::persist`。
//!
//! ## 语义边界（明确不承诺的内容）
//!
//! - 本模块**不消除**非协作写入者造成的 CAS 窗口：`ensure_edit_source_unchanged`
//!   之类的「先读内容再替换」仍然存在检查与写入之间的时间差。这里只保证：一旦固定，
//!   后续操作绑定的是**最初授权的那批 inode**。
//! - 父目录即使被外部 `rename` 走、并在原位置换成指向别处的符号链接，操作仍作用在原
//!   inode 上（父 fd 指向它），不会跟随新的符号链接目标。这是能力语义，不是「防住
//!   一切外部修改」的沙箱。
//! - 删除使用 `unlinkat`，只删除固定父目录中的 leaf 条目；叶子是符号链接时删除链接
//!   本身，不跟随其目标。
//! - 仅 Unix。其它平台未实现，也**不声称** Windows 已受保护。
//!
//! ## O_CLOEXEC
//!
//! 所有 `openat` 都带 `O_CLOEXEC`，避免目录 / 文件 / 临时文件描述符泄漏给被 `exec`
//! 的 shell 或子进程。

#![cfg(unix)]

use std::{
    ffi::{CString, OsStr},
    fs::{self, File, Permissions},
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::{AsRawFd, FromRawFd, RawFd},
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// 原子替换所用私有临时文件的名称前缀。
///
/// 测试以此确认失败或成功后父目录里没有残留临时文件，也不会误删同前缀的无关文件。
pub const TEMP_FILE_PREFIX: &str = ".mio-workspace-fs-";

/// 目录解析统一使用的打开标志：只读、必须是目录、不跟随符号链接、不泄漏到子进程。
const DIR_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// 叶子只读打开的标志：`O_NONBLOCK` 保证 FIFO 之类的对象不会阻塞在 `open` 上，
/// 随后用 `fstat` 拒绝非普通文件。
const FILE_READ_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

/// 私有临时文件的创建标志：独占创建（`O_EXCL`），绝不覆盖已有条目。
const TEMP_CREATE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// 临时文件创建时请求的权限；创建后还会用 `set_permissions` 强制为 0600，压过宽松 umask。
const TEMP_CREATE_MODE: libc::c_uint = 0o600;

/// 私有临时文件在写入阶段的权限。
const PRIVATE_FILE_MODE: u32 = 0o600;

/// 以真实目录句柄固定的工作区根。
///
/// 内部只持有 fd 与用于诊断的规范路径；所有子路径解析都从该 fd 出发，不重新读取字符
/// 路径。
#[derive(Debug)]
pub struct WorkspaceRoot {
    root: Arc<File>,
    path: PathBuf,
}

impl WorkspaceRoot {
    /// 打开一个「绝对且已规范」的根路径，逐组件固定真实目录句柄。
    ///
    /// 每个组件都用 `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` 打开：路径中任意组件是符号
    /// 链接都会以 `ELOOP` 失败。输入必须是绝对路径，且不能包含 `..`（`.` 会被跳过）；
    /// 调用方负责先做规范化（例如 `fs::canonicalize`）。
    pub fn open(root: &Path) -> io::Result<Self> {
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(invalid_input(
                "workspace root must be an absolute, normalized path",
            ));
        }
        let anchor = cstring(OsStr::new("/"))?;
        let mut current = openat_file(libc::AT_FDCWD, &anchor, DIR_OPEN_FLAGS, 0)?;
        for component in root.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(invalid_input(
                        "workspace root must be a normalized path without `..`",
                    ));
                }
                Component::Normal(name) => {
                    let name = cstring(name)?;
                    current = openat_file(current.as_raw_fd(), &name, DIR_OPEN_FLAGS, 0)?;
                }
                _ => {
                    return Err(invalid_input(
                        "workspace root contains an unsupported path component",
                    ));
                }
            }
        }
        Ok(Self {
            root: Arc::new(current),
            path: root.to_path_buf(),
        })
    }

    /// 构造时使用的规范根路径（仅用于诊断与展示）。
    pub fn root_path(&self) -> &Path {
        &self.path
    }

    /// 固定根目录的原始 fd；调用方可据此做 `fcntl` 等只读检查。
    pub fn root_fd(&self) -> RawFd {
        self.root.as_raw_fd()
    }

    /// 解析工作区内的**相对**路径，固定其父目录 fd 并返回 [`PinnedPath`]。
    ///
    /// 绝对路径、空路径、`..` 以及无法解析为单个 leaf 的路径都会被拒绝。中间组件逐级用
    /// `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` 打开；父目录必须已经存在。
    pub fn pin(&self, relative: &str) -> io::Result<PinnedPath> {
        let relative = relative.trim_end_matches('/');
        if relative.is_empty() {
            return Err(invalid_input(
                "path must name a file below the workspace root",
            ));
        }
        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(invalid_input("path must be relative to the workspace root"));
        }
        let mut components = Vec::new();
        for component in candidate.components() {
            match component {
                Component::Normal(name) => components.push(cstring(name)?),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(invalid_input("path must not contain `..`"));
                }
                _ => {
                    return Err(invalid_input("path contains an unsupported component"));
                }
            }
        }
        let leaf = components
            .pop()
            .ok_or_else(|| invalid_input("path must name a file below the workspace root"))?;
        let mut parent = Arc::clone(&self.root);
        for name in &components {
            let next = openat_file(parent.as_raw_fd(), name, DIR_OPEN_FLAGS, 0)?;
            parent = Arc::new(next);
        }
        Ok(PinnedPath {
            parent,
            leaf,
            path: self.path.join(relative),
        })
    }
}

/// 已固定父目录的一个 leaf。
///
/// 父目录以打开的 fd 持有；leaf 只是父目录下的单个名称。所有操作都相对于该 fd，因此
/// 即使在 `pin` 之后父目录被改名或原位置被换成符号链接，操作也只影响最初授权的 inode。
#[derive(Debug)]
pub struct PinnedPath {
    parent: Arc<File>,
    leaf: CString,
    path: PathBuf,
}

impl PinnedPath {
    /// `pin` 时的工作区绝对展示路径（仅用于诊断）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 以只读方式打开 leaf，并保证它是**普通文件**。
    ///
    /// 打开使用 `O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK`：符号链接以 `ELOOP` 拒绝，FIFO /
    /// 目录等非普通文件在 `fstat` 之后被拒绝，因此不会阻塞在 FIFO 的 `open` 上。
    pub fn open_read(&self) -> io::Result<File> {
        let file = openat_file(self.parent.as_raw_fd(), &self.leaf, FILE_READ_FLAGS, 0)?;
        if !file.metadata()?.is_file() {
            return Err(invalid_input(format!(
                "{} is not a regular file",
                self.path.display()
            )));
        }
        Ok(file)
    }

    /// 在显式字节上限内完整读取 leaf；不存在时返回 `Ok(None)`。
    ///
    /// 只有「不存在」被当作缺失。符号链接、非普通文件以及其它错误照常返回 `Err`。
    pub fn read_optional(&self, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        let file = match self.open_read() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeds the read byte limit",
            ));
        }
        Ok(Some(bytes))
    }

    /// 从真实打开的 fd 读取 leaf 的元数据。
    ///
    /// 与 [`Self::open_read`] 一样要求 leaf 是普通文件：元数据来自 `fstat`，而不是再次
    /// 依赖字符路径。
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        self.open_read()?.metadata()
    }

    /// 在固定的父目录 fd 中原子替换 leaf。
    ///
    /// 步骤：独占创建私有临时文件（`O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`，
    /// 0600）→ 写入 → `fsync` → 可选 `set_permissions` → `renameat` 原子替换。失败或
    /// 中途 `Drop` 都会用 `unlinkat` 清理**自己创建的那个**临时文件，不触碰其它文件。
    /// 叶子是符号链接时替换的是链接条目本身，不会跟随到链接目标。
    pub fn write_atomic(
        &self,
        contents: &[u8],
        permissions: Option<Permissions>,
    ) -> io::Result<()> {
        let temp_name = format!("{TEMP_FILE_PREFIX}{}.tmp", uuid::Uuid::new_v4().simple());
        let temp = cstring(OsStr::new(&temp_name))?;
        let file = openat_file(
            self.parent.as_raw_fd(),
            &temp,
            TEMP_CREATE_FLAGS,
            TEMP_CREATE_MODE,
        )?;
        // 一旦独占创建成功，临时文件就归本次调用所有；后续任何失败都由守卫负责清理。
        let mut guard = TempFileGuard {
            parent: Arc::clone(&self.parent),
            name: temp,
            file,
            armed: true,
        };
        guard
            .file
            .set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
        guard.file.write_all(contents)?;
        guard.file.sync_all()?;
        if let Some(permissions) = permissions {
            guard.file.set_permissions(permissions)?;
        }
        renameat_file(
            self.parent.as_raw_fd(),
            &guard.name,
            self.parent.as_raw_fd(),
            &self.leaf,
        )?;
        // rename 之后临时名字已不存在；解除守卫，避免误删外部此后可能创建的同名文件。
        guard.armed = false;
        Ok(())
    }

    /// 用 `unlinkat` 删除固定父目录中的 leaf 条目。
    ///
    /// leaf 是符号链接时删除链接本身，不跟随目标。返回 `Ok(true)` 表示确实删除了条目，
    /// `Ok(false)` 表示 leaf 本就不存在。
    pub fn remove(&self) -> io::Result<bool> {
        unlinkat_file(self.parent.as_raw_fd(), &self.leaf)
    }
}

/// 私有临时文件的清理守卫。
///
/// 在 `renameat` 成功前保持 `armed`：任何提前返回（写入失败、`fsync` 失败、权限设置
/// 失败、`renameat` 失败）都会由 `Drop` 用 `unlinkat` 删除自己 `O_EXCL` 创建的临时
/// 条目。`renameat` 成功后解除，保证不会删除别人之后创建的文件。
struct TempFileGuard {
    parent: Arc<File>,
    name: CString,
    file: File,
    armed: bool,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(owned) = self.file.metadata() else {
            return;
        };
        loop {
            let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the live parent descriptor and C string are valid, and
            // fstatat initializes `current` on success without following links.
            let result = unsafe {
                libc::fstatat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    current.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return;
            }
            // SAFETY: fstatat succeeded above.
            let current = unsafe { current.assume_init() };
            // libc's identifiers may be signed or unsigned across Unix targets;
            // match MetadataExt's u64 representation through a wider integer.
            let current_dev = (current.st_dev as i128) as u64;
            let current_ino = (current.st_ino as i128) as u64;
            if current_dev != owned.dev() || current_ino != owned.ino() {
                // A replaced name is not permission to delete another inode.
                return;
            }
            // SAFETY: `parent` keeps a valid directory descriptor alive through the drop
            // and `name` is a NUL-terminated single component.
            let result = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
            if result == 0 {
                return;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
    }
}

/// 把单个路径组件转换成 NUL 结尾的 `CString`；含 NUL 表示调用方给了非法路径。
fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_encoded_bytes())
        .map_err(|_| invalid_input("path component contains an interior NUL byte"))
}

/// 相对 `dirfd` 用单个 leaf 名打开文件，`EINTR` 自动重试。
fn openat_file(
    dirfd: RawFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<File> {
    loop {
        // SAFETY: `dirfd` is a valid descriptor and `name` is NUL-terminated.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, mode) };
        if fd >= 0 {
            // SAFETY: `fd` is a freshly opened, owned descriptor.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

/// 在同一目录 fd 内原子重命名，`EINTR` 自动重试。
fn renameat_file(dirfd: RawFd, old: &CString, new_dirfd: RawFd, new: &CString) -> io::Result<()> {
    loop {
        // SAFETY: both descriptors are valid directories and both names are NUL-terminated.
        let result = unsafe { libc::renameat(dirfd, old.as_ptr(), new_dirfd, new.as_ptr()) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

/// 相对 `dirfd` 用单个 leaf 名删除条目；不存在返回 `Ok(false)`，`EINTR` 自动重试。
fn unlinkat_file(dirfd: RawFd, name: &CString) -> io::Result<bool> {
    loop {
        // SAFETY: `dirfd` is a valid directory descriptor and `name` is NUL-terminated.
        let result = unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error);
    }
}

/// 构造一个保守的「拒绝」输入错误。
fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn cleanup_preserves_a_replaced_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::open(&dir.path().canonicalize().unwrap()).unwrap();
        let name = cstring(OsStr::new("owned.tmp")).unwrap();
        let file = openat_file(
            root.root.as_raw_fd(),
            &name,
            TEMP_CREATE_FLAGS,
            TEMP_CREATE_MODE,
        )
        .unwrap();
        let guard = TempFileGuard {
            parent: Arc::clone(&root.root),
            name,
            file,
            armed: true,
        };
        fs::rename(
            dir.path().join("owned.tmp"),
            dir.path().join("original.tmp"),
        )
        .unwrap();
        fs::write(dir.path().join("owned.tmp"), b"replacement").unwrap();
        drop(guard);
        assert_eq!(
            fs::read(dir.path().join("owned.tmp")).unwrap(),
            b"replacement"
        );
        assert!(dir.path().join("original.tmp").is_file());
    }

    #[test]
    fn cleanup_can_remove_its_own_unreadable_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::open(&dir.path().canonicalize().unwrap()).unwrap();
        let name = cstring(OsStr::new("owned.tmp")).unwrap();
        let file = openat_file(
            root.root.as_raw_fd(),
            &name,
            TEMP_CREATE_FLAGS,
            TEMP_CREATE_MODE,
        )
        .unwrap();
        file.set_permissions(Permissions::from_mode(0o000)).unwrap();
        let guard = TempFileGuard {
            parent: Arc::clone(&root.root),
            name,
            file,
            armed: true,
        };
        drop(guard);
        assert!(!dir.path().join("owned.tmp").exists());
    }
}
