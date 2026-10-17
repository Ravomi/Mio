//! 工作区 I/O 的能力式（capability）文件系统封装。
//!
//! 目标是把「先按字符路径逐级校验、真正读写时再重新解析同一个可变绝对路径」收窄为
//! 「校验一次，之后只对最初授权的目录 fd 做相对操作」。父目录在固定之后被 `rename`
//! 走、并在原位置换成指向工作区外的符号链接时，读写仍只作用在最初授权的 inode 上。
//!
//! - **Unix**：从规范化的根开始逐组件 `openat(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`。
//!   [`Target`] 持有父目录 fd 与单个 leaf 名：读用 `openat(O_NOFOLLOW | O_NONBLOCK)`，
//!   写用同目录内 `O_CREAT | O_EXCL` 私有临时文件 + `renameat`，删除用 `unlinkat`，
//!   分类用父目录 fd 上的 `fstatat(AT_SYMLINK_NOFOLLOW)`；目录遍历用
//!   `fdopendir` / `readdir` 并逐项 `fstatat(AT_SYMLINK_NOFOLLOW)`，不再按绝对路径重开。
//!   非普通文件（目录、FIFO、设备等）一律拒绝，符号链接永不跟随。
//! - **非 Unix**：**诚实边界**。本模块仍按字符路径操作（`std::fs`），**不声称**能防住
//!   父目录被替换；调用方也不会把这里当成已验证的隔离。这里保持原有可移植行为，
//!   不做任何「已受保护」的虚假声明。
//!
//! 本模块只提供目录句柄语义，不消除非协作写入者造成的 CAS 窗口（先读内容、再原子替换
//! 之间的时间差）。它保证的是：一旦固定，后续操作绑定最初授权的那批 inode。

use std::{
    fs::{File, Permissions},
    io,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use std::{
    ffi::{CString, OsStr},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    },
};

/// 目录项 / 叶子的类型，永不跟随符号链接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// 一次「不跟随符号链接」的 `stat` 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    /// 条目不存在。
    Missing,
    File,
    Dir,
    Symlink,
    Other,
}

/// 固定父目录上的叶子分类与权限；不做任何内容 I/O。
pub struct TargetStat {
    pub kind: StatKind,
    /// 仅当 `kind == StatKind::File` 时携带真实权限位。
    pub permissions: Option<Permissions>,
}

/// 以真实目录句柄固定的工作区根。
///
/// 内部只持有 fd 与用于诊断的规范路径；所有子路径解析都从该 fd 出发，不重新读取字符
/// 路径（非 Unix 除外，见模块头部的诚实边界）。
pub struct Root {
    #[cfg(unix)]
    root: Arc<File>,
    #[cfg(not(unix))]
    root: PathBuf,
    path: PathBuf,
}

impl Root {
    /// 打开一个「绝对且已规范」的根路径，逐组件固定真实目录句柄。
    ///
    /// Unix 上每个组件都用 `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` 打开：路径中任意
    /// 组件是符号链接都会以 `ELOOP` 失败。非 Unix 仅记录路径。
    pub fn open(cwd: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            if cwd.as_os_str().is_empty() || !cwd.is_absolute() {
                return Err("workspace root must be an absolute, normalized path".into());
            }
            let mut current = openat_file(libc::AT_FDCWD, &cstring(OsStr::new("/"))?, DIR_FLAGS)
                .map_err(|error| error.to_string())?;
            for component in cwd.components() {
                match component {
                    Component::RootDir | Component::CurDir => {}
                    Component::ParentDir => {
                        return Err("workspace root must be a normalized path without `..`".into());
                    }
                    Component::Normal(name) => {
                        let name = cstring(name)?;
                        current = openat_file(current.as_raw_fd(), &name, DIR_FLAGS)
                            .map_err(|error| error.to_string())?;
                    }
                    _ => {
                        return Err("workspace root contains an unsupported path component".into());
                    }
                }
            }
            Ok(Self {
                root: Arc::new(current),
                path: cwd.to_path_buf(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                root: cwd.to_path_buf(),
                path: cwd.to_path_buf(),
            })
        }
    }

    /// 构造时使用的规范根路径（仅用于诊断与展示）。
    pub fn display_path(&self) -> &Path {
        &self.path
    }

    /// 固定后的根目录句柄。
    pub fn root_dir(&self) -> Result<Dir, String> {
        #[cfg(unix)]
        {
            let dup = dup_cloexec(self.root.as_raw_fd()).map_err(|error| error.to_string())?;
            // SAFETY: `dup` is a freshly duplicated descriptor owned here.
            Ok(Dir {
                dir: Arc::new(unsafe { File::from_raw_fd(dup) }),
                display: self.path.clone(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Dir {
                display: self.path.clone(),
            })
        }
    }

    /// 解析工作区内的**相对**目录路径，固定其目录句柄。
    ///
    /// 绝对路径、空路径、`..` 以及无法解析的路径都会被拒绝；中间组件逐级用
    /// `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` 打开，必须已经存在。
    pub fn dir(&self, relative: &str) -> Result<Dir, String> {
        #[cfg(unix)]
        {
            let names = split_relative(relative)?;
            let mut current = Arc::clone(&self.root);
            for name in &names {
                current = Arc::new(
                    openat_file(current.as_raw_fd(), name, DIR_FLAGS)
                        .map_err(|error| error.to_string())?,
                );
            }
            Ok(Dir {
                dir: current,
                display: self.path.join(relative),
            })
        }
        #[cfg(not(unix))]
        {
            if split_relative_checked(relative).is_err() {
                return Err("path must name a directory below the workspace root".into());
            }
            Ok(Dir {
                display: self.path.join(relative),
            })
        }
    }

    /// 解析工作区内的**相对**路径，固定其父目录并返回 [`Target`]。
    ///
    /// 绝对路径、空路径、`..` 以及无法解析为单个 leaf 的路径都会被拒绝；中间组件逐级用
    /// `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` 打开，父目录必须已经存在。
    pub fn target(&self, relative: &str) -> Result<Target, String> {
        #[cfg(unix)]
        {
            let mut names = split_relative(relative)?;
            let leaf = names
                .pop()
                .ok_or_else(|| "path must name a file below the workspace root".to_string())?;
            let mut parent = Arc::clone(&self.root);
            for name in &names {
                parent = Arc::new(
                    openat_file(parent.as_raw_fd(), name, DIR_FLAGS)
                        .map_err(|error| error.to_string())?,
                );
            }
            Ok(Target {
                parent,
                leaf,
                display: self.path.join(relative),
            })
        }
        #[cfg(not(unix))]
        {
            if split_relative_checked(relative).is_err() {
                return Err("path must name a file below the workspace root".into());
            }
            Ok(Target {
                display: self.path.join(relative),
            })
        }
    }
}

/// 固定目录句柄；遍历只读取该 fd，不再按绝对路径重开。
pub struct Dir {
    #[cfg(unix)]
    dir: Arc<File>,
    display: PathBuf,
}

impl Dir {
    /// 在当前固定目录中定位单个 leaf，不重新解析父目录的展示路径。
    pub fn target(&self, name: &str) -> Result<Target, String> {
        let mut components = Path::new(name).components();
        let Some(Component::Normal(leaf)) = components.next() else {
            return Err("target must be a single file name".into());
        };
        if components.next().is_some() {
            return Err("target must be a single file name".into());
        }
        Ok(Target {
            #[cfg(unix)]
            parent: Arc::clone(&self.dir),
            #[cfg(unix)]
            leaf: cstring(leaf)?,
            display: self.display.join(leaf),
        })
    }

    /// 逐个产出目录条目的流式迭代器（不含 `.` 与 `..`）。
    ///
    /// Unix 上迭代器持有 `fdopendir` / `readdir` 的目录流：**每次只产出一个条目**，调用方
    /// 处理完即可立即释放，所以遍历很多目录时不会同时保留大量目录 fd。分类仍用
    /// `fstatat(AT_SYMLINK_NOFOLLOW)`，父目录被替换也不会改读别处。迭代器 `Drop` 时必定
    /// `closedir`。
    pub fn children(&self) -> Result<Children, String> {
        #[cfg(unix)]
        {
            // 重开 `.` 得到**独立**的目录描述符：`dup` 会共享文件偏移，前一次遍历会把偏移
            // 留在末尾，使下一次 `readdir` 直接读到 EOF。相对固定 fd 的 `openat(".")` 仍然
            // 是能力式操作，同时给出从 0 开始的新偏移。
            let dot = cstring(OsStr::new("."))?;
            let reopened = openat_file(self.dir.as_raw_fd(), &dot, DIR_FLAGS)
                .map_err(|error| error.to_string())?;
            // `fdopendir` 只在**成功**时接管 descriptor。先交出原始 fd，失败时再据其重建
            // `File` 关闭，保证错误路径不泄漏 descriptor。
            let fd = reopened.into_raw_fd();
            // SAFETY: `fd` is a live directory descriptor; on success `fdopendir` takes
            // ownership of it, and on failure we reclaim it below.
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: `fdopendir` failed and did not take ownership, so `fd` is still
                // live and owned here; rebuilding `File` closes it exactly once.
                drop(unsafe { File::from_raw_fd(fd) });
                return Err(error.to_string());
            }
            Ok(Children {
                stream,
                parent: Arc::clone(&self.dir),
                display: self.display.clone(),
            })
        }
        #[cfg(not(unix))]
        {
            let read_dir = std::fs::read_dir(&self.display).map_err(|error| error.to_string())?;
            Ok(Children { read_dir })
        }
    }
}

/// [`Dir::children`] 的流式迭代器。
///
/// Unix 上持有 `fdopendir` 目录流与固定父目录 fd，`Drop` 时一定 `closedir`；非 Unix 上
/// 直接包装 `std::fs::ReadDir`。
pub struct Children {
    #[cfg(unix)]
    stream: *mut libc::DIR,
    #[cfg(unix)]
    parent: Arc<File>,
    #[cfg(unix)]
    display: PathBuf,
    #[cfg(not(unix))]
    read_dir: std::fs::ReadDir,
}

impl Children {
    #[cfg(unix)]
    fn finish(&mut self) {
        if !self.stream.is_null() {
            // SAFETY: `stream` is a live directory stream owned here, closed exactly once.
            unsafe { libc::closedir(self.stream) };
            self.stream = std::ptr::null_mut();
        }
    }
}

#[cfg(unix)]
impl Iterator for Children {
    type Item = Result<Child, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stream.is_null() {
            return None;
        }
        loop {
            reset_errno();
            // SAFETY: `stream` is a live directory stream; `readdir` returns a borrowed
            // pointer valid until the next call.
            let entry = unsafe { libc::readdir(self.stream) };
            if entry.is_null() {
                let failure = readdir_failure();
                self.finish();
                return failure.map(|error| Err(error.to_string()));
            }
            // SAFETY: `entry` is non-null and points at a live `dirent`.
            let raw = unsafe { (*entry).d_name.as_ptr() };
            // SAFETY: `d_name` is a NUL-terminated name inside the live `dirent`.
            let bytes = unsafe { std::ffi::CStr::from_ptr(raw) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let leaf = match CString::new(bytes) {
                Ok(leaf) => leaf,
                Err(_) => {
                    return Some(Err(
                        "directory entry name contains an interior NUL byte".into()
                    ));
                }
            };
            let stat = match stat_name(self.parent.as_raw_fd(), &leaf) {
                Ok(stat) => stat,
                Err(error) => return Some(Err(error.to_string())),
            };
            return Some(Ok(Child {
                name: OsStr::from_bytes(bytes).to_string_lossy().into_owned(),
                display: self.display.join(OsStr::from_bytes(bytes)),
                kind: stat.0,
                parent: Arc::clone(&self.parent),
                leaf,
            }));
        }
    }
}

#[cfg(unix)]
impl Drop for Children {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(not(unix))]
impl Iterator for Children {
    type Item = Result<Child, String>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.read_dir.next()?;
        Some((|| {
            let entry = entry.map_err(|error| error.to_string())?;
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            let kind = if file_type.is_dir() {
                NodeKind::Dir
            } else if file_type.is_symlink() {
                NodeKind::Symlink
            } else if file_type.is_file() {
                NodeKind::File
            } else {
                NodeKind::Other
            };
            Ok(Child {
                display: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            })
        })())
    }
}

/// 固定父目录中的一个目录项 / leaf。
pub struct Child {
    name: String,
    display: PathBuf,
    kind: NodeKind,
    #[cfg(unix)]
    parent: Arc<File>,
    #[cfg(unix)]
    leaf: CString,
}

impl Child {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_path(&self) -> &Path {
        &self.display
    }

    pub fn kind(&self) -> NodeKind {
        self.kind
    }

    /// 以只读方式打开该条目，并保证它是**普通文件**。
    pub fn open_file(&self) -> Result<File, String> {
        #[cfg(unix)]
        {
            let file = openat_file(self.parent.as_raw_fd(), &self.leaf, FILE_FLAGS)
                .map_err(|error| error.to_string())?;
            if !file
                .metadata()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err(format!("{} is not a regular file", self.display.display()));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let metadata =
                std::fs::symlink_metadata(&self.display).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("symbolic links are not followed".into());
            }
            if !metadata.is_file() {
                return Err(format!("{} is not a regular file", self.display.display()));
            }
            File::open(&self.display).map_err(|error| error.to_string())
        }
    }

    /// 打开该条目指向的目录句柄（不跟随符号链接）。
    pub fn open_dir(&self) -> Result<Dir, String> {
        #[cfg(unix)]
        {
            let file = openat_file(self.parent.as_raw_fd(), &self.leaf, DIR_FLAGS)
                .map_err(|error| error.to_string())?;
            Ok(Dir {
                dir: Arc::new(file),
                display: self.display.clone(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Dir {
                display: self.display.clone(),
            })
        }
    }
}

/// 已固定父目录中的一个 leaf 目标。
pub struct Target {
    #[cfg(unix)]
    parent: Arc<File>,
    #[cfg(unix)]
    leaf: CString,
    display: PathBuf,
}

impl Target {
    /// `pin` 时的展示路径（仅用于诊断）。
    pub fn display_path(&self) -> &Path {
        &self.display
    }

    /// 在固定父目录 fd 上做一次不跟随符号链接的 `stat`。
    ///
    /// 不打开、不读取叶子内容，因此对不可读的普通文件也能拿到真实权限位。
    pub fn stat(&self) -> Result<TargetStat, String> {
        #[cfg(unix)]
        {
            let (kind, permissions) = match stat_name(self.parent.as_raw_fd(), &self.leaf) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(TargetStat {
                        kind: StatKind::Missing,
                        permissions: None,
                    });
                }
                Err(error) => return Err(error.to_string()),
            };
            Ok(TargetStat {
                kind: match kind {
                    NodeKind::File => StatKind::File,
                    NodeKind::Dir => StatKind::Dir,
                    NodeKind::Symlink => StatKind::Symlink,
                    NodeKind::Other => StatKind::Other,
                },
                permissions,
            })
        }
        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(&self.display) {
                Ok(metadata) => {
                    let kind = if metadata.file_type().is_symlink() {
                        StatKind::Symlink
                    } else if metadata.is_file() {
                        StatKind::File
                    } else if metadata.is_dir() {
                        StatKind::Dir
                    } else {
                        StatKind::Other
                    };
                    let permissions = (kind == StatKind::File).then(|| metadata.permissions());
                    Ok(TargetStat { kind, permissions })
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetStat {
                    kind: StatKind::Missing,
                    permissions: None,
                }),
                Err(error) => Err(error.to_string()),
            }
        }
    }

    /// 以只读方式打开 leaf，并保证它是**普通文件**。
    ///
    /// 打开使用 `O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK`：符号链接以 `ELOOP` 拒绝，
    /// FIFO / 目录等非普通文件在 `fstat` 之后被拒绝，因此不会阻塞在 FIFO 的 `open` 上。
    pub fn open_read(&self) -> Result<File, String> {
        #[cfg(unix)]
        {
            let file = openat_file(self.parent.as_raw_fd(), &self.leaf, FILE_FLAGS)
                .map_err(|error| error.to_string())?;
            if !file
                .metadata()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err(format!("{} is not a regular file", self.display.display()));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let metadata =
                std::fs::symlink_metadata(&self.display).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("symbolic links are not followed".into());
            }
            if !metadata.is_file() {
                return Err(format!("{} is not a regular file", self.display.display()));
            }
            File::open(&self.display).map_err(|error| error.to_string())
        }
    }

    /// 在固定的父目录中原子替换 leaf。
    ///
    /// 步骤：独占创建私有临时文件（`O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`，0600）
    /// → 写入 → `fsync` → 可选 `set_permissions` → `renameat` 原子替换。失败或中途
    /// `Drop` 都会用 `unlinkat` 清理**自己创建的那个**同 inode 临时条目，不触碰别的文件。
    /// 叶子是符号链接时替换的是链接条目本身，不会跟随到链接目标。
    pub fn write_atomic(
        &self,
        contents: &[u8],
        permissions: Option<Permissions>,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            let temp_name = format!("{TEMP_FILE_PREFIX}{}.tmp", uuid::Uuid::new_v4().simple());
            let temp = cstring(OsStr::new(&temp_name))?;
            let file = openat_create(
                self.parent.as_raw_fd(),
                &temp,
                TEMP_CREATE_FLAGS,
                TEMP_CREATE_MODE,
            )
            .map_err(|error| error.to_string())?;
            let mut guard = TempFileGuard {
                parent: Arc::clone(&self.parent),
                name: temp,
                file,
                armed: true,
            };
            guard
                .file
                .set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
                .map_err(|error| error.to_string())?;
            guard
                .file
                .write_all(contents)
                .map_err(|error| error.to_string())?;
            guard.file.sync_all().map_err(|error| error.to_string())?;
            if let Some(permissions) = permissions {
                guard
                    .file
                    .set_permissions(permissions)
                    .map_err(|error| error.to_string())?;
            }
            renameat_file(
                self.parent.as_raw_fd(),
                &guard.name,
                self.parent.as_raw_fd(),
                &self.leaf,
            )
            .map_err(|error| error.to_string())?;
            // rename 之后临时名字已不存在；解除守卫，避免误删外部此后可能创建的同名文件。
            guard.armed = false;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let parent = self.display.parent().ok_or("invalid parent directory")?;
            let temp = tempfile::NamedTempFile::new_in(parent)
                .map_err(|error| format!("failed to create a temporary file: {error}"))?;
            if let Some(permissions) = permissions {
                std::fs::set_permissions(temp.path(), permissions)
                    .map_err(|error| format!("failed to set permissions: {error}"))?;
            }
            std::fs::write(temp.path(), contents)
                .map_err(|error| format!("failed to write the temporary file: {error}"))?;
            temp.persist(&self.display)
                .map_err(|error| format!("atomic persist failed: {error}"))?;
            Ok(())
        }
    }
}

/// 私有临时文件的名称前缀。
#[cfg(unix)]
const TEMP_FILE_PREFIX: &str = ".mio-workspace-capability-";

/// 目录解析统一使用的打开标志：只读、必须是目录、不跟随符号链接、不泄漏到子进程。
#[cfg(unix)]
const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// 叶子只读打开的标志：`O_NONBLOCK` 保证 FIFO 之类的对象不会阻塞在 `open` 上。
#[cfg(unix)]
const FILE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

/// 私有临时文件的创建标志：独占创建（`O_EXCL`），绝不覆盖已有条目。
#[cfg(unix)]
const TEMP_CREATE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// 临时文件创建时请求的权限；创建后还会强制为 0600，压过宽松 umask。
#[cfg(unix)]
const TEMP_CREATE_MODE: libc::c_uint = 0o600;

/// 私有临时文件在写入阶段的权限。
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// 把单个路径组件转换成 NUL 结尾的 `CString`。
#[cfg(unix)]
fn cstring(value: &OsStr) -> Result<CString, String> {
    CString::new(value.as_bytes())
        .map_err(|_| "path component contains an interior NUL byte".into())
}

/// 把相对路径切成组件序列；拒绝空路径、绝对路径与 `..`。
#[cfg(unix)]
fn split_relative(relative: &str) -> Result<Vec<CString>, String> {
    let candidate = std::path::Path::new(relative.trim_end_matches('/'));
    if candidate.as_os_str().is_empty() {
        return Err("path must name something below the workspace root".into());
    }
    if candidate.is_absolute() {
        return Err("path must be relative to the workspace root".into());
    }
    let mut out = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(name) => out.push(cstring(name)?),
            Component::CurDir => {}
            Component::ParentDir => return Err("path must not contain `..`".into()),
            _ => return Err("path contains an unsupported component".into()),
        }
    }
    if out.is_empty() {
        return Err("path must name something below the workspace root".into());
    }
    Ok(out)
}

/// 非 Unix 下用同样的相对路径规则做一次拒绝性校验。
#[cfg(not(unix))]
fn split_relative_checked(relative: &str) -> Result<(), String> {
    let candidate = std::path::Path::new(relative.trim_end_matches('/'));
    if candidate.as_os_str().is_empty() || candidate.is_absolute() {
        return Err("path must be relative to the workspace root".into());
    }
    let mut has_component = false;
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => has_component = true,
            _ => return Err("path must not contain `..`".into()),
        }
    }
    if !has_component {
        return Err("path must name something below the workspace root".into());
    }
    Ok(())
}

/// `dup` 一个 close-on-exec 的目录 fd。
#[cfg(unix)]
fn dup_cloexec(fd: RawFd) -> io::Result<RawFd> {
    loop {
        // SAFETY: `fd` is a live descriptor and `F_DUPFD_CLOEXEC` only duplicates it.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup >= 0 {
            return Ok(dup);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

/// 相对 `dirfd` 用单个 leaf 名打开一个已存在的条目。
#[cfg(unix)]
fn openat_file(dirfd: RawFd, name: &CString, flags: libc::c_int) -> io::Result<File> {
    openat_create(dirfd, name, flags, 0)
}

/// 相对 `dirfd` 用单个 leaf 名打开（`mode` 只在创建时生效），`EINTR` 自动重试。
#[cfg(unix)]
fn openat_create(
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
#[cfg(unix)]
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

/// 在父目录 fd 上对单个 leaf 做不跟随符号链接的 `stat`。
#[cfg(unix)]
fn stat_name(dirfd: RawFd, name: &CString) -> io::Result<(NodeKind, Option<Permissions>)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    loop {
        // SAFETY: `dirfd` is a live directory descriptor, `name` is NUL-terminated, and
        // `fstatat` initializes `stat` on success without following symbolic links.
        let result = unsafe {
            libc::fstatat(
                dirfd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
    // SAFETY: `fstatat` succeeded above, so `stat` is initialized.
    let stat = unsafe { stat.assume_init() };
    let kind = node_kind(stat.st_mode);
    let permissions =
        (kind == NodeKind::File).then(|| Permissions::from_mode(mode_bits(stat.st_mode)));
    Ok((kind, permissions))
}

/// 把权限位归一成 `Permissions::from_mode` 需要的 `u32`。
///
/// `mode_t` 在 Linux 是 `u32`、在 Apple 是 `u16`；经 `u64` 中转可以避免在任一平台上
/// 出现「同类型转换」这种 clippy 会拒绝的冗余写法。
#[cfg(unix)]
fn mode_bits(mode: libc::mode_t) -> u32 {
    (mode as u64 & 0o7777) as u32
}

/// 从 `st_mode` 判断不跟随符号链接的类型。
#[cfg(unix)]
fn node_kind(mode: libc::mode_t) -> NodeKind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => NodeKind::File,
        libc::S_IFDIR => NodeKind::Dir,
        libc::S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

/// 把当前线程的 `errno` 归零，好让 `readdir` 返回 `NULL` 时能区分「目录结束」与错误。
#[cfg(unix)]
fn reset_errno() {
    let pointer = errno_pointer();
    if !pointer.is_null() {
        // SAFETY: `errno_pointer` returns this thread's errno slot.
        unsafe { *pointer = 0 };
    }
}

/// `readdir` 返回 `NULL` 之后判断是否真的出错。
#[cfg(unix)]
fn readdir_failure() -> Option<io::Error> {
    if errno_pointer().is_null() {
        return None;
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code != 0 => Some(error),
        _ => None,
    }
}

/// 本线程 `errno` 地址。覆盖 Rust 官方支持的主要 Unix；其余平台返回空指针，退化为
/// 「`readdir` 返回 `NULL` 即视为目录结束」。
#[cfg(unix)]
fn errno_pointer() -> *mut libc::c_int {
    #[cfg(any(
        target_os = "linux",
        target_os = "hurd",
        target_os = "redox",
        target_os = "emscripten",
        target_os = "dragonfly",
        target_os = "teeos",
        target_os = "qurt",
    ))]
    {
        // SAFETY: the platform exposes a thread-local errno slot.
        unsafe { libc::__errno_location() }
    }
    #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd",))]
    {
        // SAFETY: the platform exposes a thread-local errno slot.
        unsafe { libc::__errno() }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
    ))]
    {
        // SAFETY: the platform exposes a thread-local errno slot.
        unsafe { libc::__error() }
    }
    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    {
        // SAFETY: the platform exposes a thread-local errno slot.
        unsafe { libc::___errno() }
    }
    #[cfg(target_os = "haiku")]
    {
        // SAFETY: the platform exposes a thread-local errno slot.
        unsafe { libc::_errnop() }
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "hurd",
        target_os = "redox",
        target_os = "emscripten",
        target_os = "dragonfly",
        target_os = "teeos",
        target_os = "qurt",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "haiku",
    )))]
    {
        std::ptr::null_mut()
    }
}

/// 私有临时文件的清理守卫。
///
/// 在 `renameat` 成功前保持 `armed`：任何提前返回都由 `Drop` 用 `unlinkat` 删除自己
/// `O_EXCL` 创建的临时条目，但先用 `fstatat` 核对 dev/ino，绝不删除被替换成别的 inode
/// 的同名文件。
#[cfg(unix)]
struct TempFileGuard {
    parent: Arc<File>,
    name: CString,
    file: File,
    armed: bool,
}

#[cfg(unix)]
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
            // SAFETY: the live parent descriptor and C string are valid, and `fstatat`
            // initializes `current` on success without following links.
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
            // SAFETY: `fstatat` succeeded above.
            let current = unsafe { current.assume_init() };
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
