use crate::read_observation::{ReadObservation, ReadRange, normalize_read_path};
use crate::tool_identity::ToolIdentity;
use crate::unknown_effect::{UnknownReason, UnknownSideEffect};
use crate::{AgentEvent, Host};
use mio_core::tool::{PortableTool, Tool, ToolContext, ToolExecutionError, ToolOutput};
use mio_core::wasm_compat::WasmCompatSend;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

// Pin directory handles for native workspace I/O; character paths are diagnostic only.
#[path = "workspace_capability.rs"]
pub(crate) mod workspace_capability;

use workspace_capability::{NodeKind, StatKind};

pub const MAX_READ_INPUT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB 硬上限
pub const MAX_READ_OUTPUT_BYTES: usize = 64 * 1024; // 64 KiB
/// read 每次输出预算的下限：足够放下截断头与至少一行短内容。
pub const MIN_READ_OUTPUT_BYTES: usize = 256;
pub const MAX_WRITE_BYTES: usize = 1024 * 1024; // 1 MiB
const MAX_EDIT_PREVIEW_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_GREP_FILE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
pub const MAX_GREP_TOTAL_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_GREP_LINE_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_SEARCH_VISITED_ENTRIES: usize = 5000;
pub const MAX_SEARCH_DEPTH: usize = 64;
pub const MAX_GREP_MATCHES: usize = 500;
/// `grep` 邻近上下文行上限；超过即明确拒绝，不静默截断到上限。
pub const MAX_GREP_CONTEXT_LINES: u64 = 20;
/// 从既有总输出上限中扣出的截断通知预算，不新增任何输出额度。
const GREP_NOTICE_BUDGET: usize = 256;
pub const MAX_FIND_MATCHES: usize = 500;
pub const MAX_LS_ENTRIES: usize = 1000;
pub const SHELL_TIMEOUT_SECS: u64 = 120;
/// Backstop grace for a custom [`Host::shell`] future that applies no timeout of
/// its own. The native default already reports its real post-spawn state at
/// [`SHELL_TIMEOUT_SECS`], so this outer backstop must not race ahead of it.
const SHELL_HOST_BACKSTOP_GRACE_SECS: u64 = 10;

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
    // 以真实目录句柄固定的工作区能力根：read/write/edit 与目录遍历都从它出发，不再在逐级
    // 校验之后重新解析可变绝对路径。
    capability: Arc<workspace_capability::Root>,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) read_only: bool,
    pub(crate) subagents: Option<Arc<crate::subagent::SubagentManager>>,
    pub(crate) tool_filter: crate::pattern::ToolFilter,
    pub(crate) shared_limits: Option<crate::shared_limits::SharedLimits>,
    // 运行级统一门禁：附加后审批只在 dispatch 边界发生一次，避免双重审批。
    policy: Option<crate::policy::ToolPolicy>,
    // 可选 pre-edit 文件检查点；默认关闭，未附加时 write/edit 行为与本字段引入前一致。
    checkpoints: Option<Arc<crate::checkpoint::CheckpointStore>>,
    // 每实例的 read 输出预算；默认 MAX_READ_OUTPUT_BYTES，直接调用者行为不变。
    // 运行时按已解析的上下文预算收紧，避免一次 read 工具结果就撑爆下一轮输入。
    read_output_bytes: usize,
    execution_policy: crate::execution_backend::ExecutionPolicy,
    access: Arc<tokio::sync::RwLock<()>>,
    // 生产 run 生命周期持有的项目外 shared workspace activity 租约。它在
    // `Workspace::new` 时**无条件**取得（与 `.mio/outputs` 是否已存在无关），并保留到该
    // run 的最后一个 `Workspace` 克隆被丢弃。持有期间离线清理的 exclusive activity 租约
    // 非阻塞失败（`Busy`），所以清理不会在活动 run 中删除一个该 run 还可能引用的旧输出，
    // 也不会在「run 构造之后才发生第一次 capture」时留下缺口。锁文件位于项目之外，因此
    // 只读问答不会在项目根或 `.mio` 新增任何文件。
    run_output_lease: Arc<crate::shell::workspace_activity::ActivityLease>,
}

/// 把 read 输出预算收敛到可用区间：不小于一个截断头加一行短内容，不超过 64 KiB 硬上限。
pub fn clamp_read_output_bytes(bytes: usize) -> usize {
    bytes.clamp(MIN_READ_OUTPUT_BYTES, MAX_READ_OUTPUT_BYTES)
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
        // 生产 run 生命周期参与共同 activity 租约（DES-17）：无条件取得按工作区定位的
        // 项目外 shared 租约并持有到该 Workspace 被丢弃，即使 `.mio/outputs` 还不存在。
        // 锁文件不落在项目目录里，因此只读问答保持零项目污染；与 Session/journal 写入、
        // 捕获事务使用完全相同的锁顺序（activity → cleanup）。
        let run_output_lease = match crate::shell::workspace_activity::shared_for_workspace(
            &canonical_cwd,
        ) {
            Ok(Some(lease)) => Arc::new(lease),
            Ok(None) => {
                return Err(
                        "workspace output activity lease is unavailable (an offline output cleanup may be running)"
                            .into(),
                    );
            }
            Err(error) => {
                return Err(format!(
                    "workspace output activity lease is unavailable (an offline output cleanup may be running): {error}"
                ));
            }
        };
        let capability = workspace_capability::Root::open(&canonical_cwd)
            .map_err(|error| format!("工作目录能力句柄不可用: {error}"))?;
        Ok(Self {
            cwd: canonical_cwd,
            capability: Arc::new(capability),
            execution_policy: host.execution_policy(),
            host,
            read_only: false,
            subagents: None,
            tool_filter: crate::pattern::ToolFilter::all(),
            shared_limits: None,
            policy: None,
            checkpoints: None,
            read_output_bytes: MAX_READ_OUTPUT_BYTES,
            access: Arc::new(tokio::sync::RwLock::new(())),
            run_output_lease,
        })
    }

    /// Use the immutable policy snapshot selected by the enclosing run.
    pub(crate) fn with_execution_policy(
        mut self,
        policy: crate::execution_backend::ExecutionPolicy,
    ) -> Self {
        self.execution_policy = policy;
        self
    }

    /// Attach the cooperative cross-process workspace quota.
    pub fn with_shared_limits(mut self, limits: crate::shared_limits::SharedLimits) -> Self {
        self.shared_limits = Some(limits);
        self
    }

    /// Attach a parent delegation lease for this same workspace.
    pub fn with_subagents(
        mut self,
        manager: Arc<crate::subagent::SubagentManager>,
    ) -> Result<Self, String> {
        if manager.root_cwd() != self.cwd {
            return Err("subagent manager belongs to a different workspace".into());
        }
        self.subagents = Some(manager);
        Ok(self)
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

    /// Attach the run's unified dispatch gate.
    ///
    /// With a gate attached, approval happens once at the dispatch boundary and
    /// this tool must not ask again; without one (a direct caller), the legacy
    /// per-operation approval still applies.
    pub fn with_tool_policy(mut self, policy: crate::policy::ToolPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// 附加可选的文件检查点存储。
    ///
    /// 启用后 `write` / `edit` 会在替换文件之前持久化 pre-edit 快照；快照失败时拒绝
    /// 写入并返回错误。传入禁用存储等同于不启用。快照仍然只在通过工具过滤、只读检查、
    /// 工作区租约与统一审批之后发生，不会为模型绕过任何准入。
    pub fn with_checkpoints(
        mut self,
        checkpoints: Arc<crate::checkpoint::CheckpointStore>,
    ) -> Self {
        self.checkpoints = checkpoints.is_enabled().then_some(checkpoints);
        self
    }

    /// 当前实例附加的文件检查点存储（未启用时为 `None`）。
    pub fn checkpoints(&self) -> Option<&Arc<crate::checkpoint::CheckpointStore>> {
        self.checkpoints.as_ref()
    }

    /// 通过工作区租约回退某个检查点（宿主维护入口，不是模型工具）。
    ///
    /// 它复用与 `write` / `edit` 完全相同的准入链：只读检查、`SharedLimits` 跨进程写
    /// 租约、进程内 `workspace_admission` 协作准入、本实例读写锁、父级委派租约，以及
    /// 宿主审批；通过后才在阻塞 worker 中调用
    /// [`crate::checkpoint::CheckpointStore::rewind`]。因此合作进程的写与 rewind 会
    /// 互相排队，不会交错覆盖；非协作的外部写入者不在此保证范围内，`rewind` 仍只按内容
    /// 指纹做乐观拒绝。
    ///
    /// 返回 `Err` 表示检查点未启用、工作区只读、租约/审批被拒，或检查点状态不允许回退。
    pub async fn rewind_checkpoint(
        &self,
        selector: impl AsRef<str>,
    ) -> Result<crate::checkpoint::RewindOutcome, String> {
        let Some(store) = self.checkpoints.clone() else {
            return Err("文件检查点未启用，无法 rewind".into());
        };
        if self.read_only {
            return Err("只读工作区禁止执行 'rewind' 操作".into());
        }
        if !self.tool_filter.allows_workspace_operation("write") {
            return Err("工具过滤策略禁止文件回退".into());
        }
        let selector = selector.as_ref();
        let shared = match &self.shared_limits {
            Some(shared) => Some(
                shared
                    .acquire_workspace(&self.cwd, true)
                    .await
                    .map_err(|error| format!("Workspace admission: {error}"))?,
            ),
            None => None,
        };
        let global = workspace_admission()
            .acquire_task(crate::coordinator::TaskRequest {
                dedup_key: None,
                path: workspace_scope(&self.cwd),
                write: true,
            })
            .await
            .map_err(|error| format!("Workspace admission: {error}"))?;
        let access = WorkspaceAccess::Write {
            _guard: self.access.clone().write_owned().await,
        };
        let parent = self
            .subagents
            .as_ref()
            .map(|manager| manager.parent_operation(true))
            .transpose()?;
        let lease = WorkspaceLease {
            _parent: parent,
            _access: access,
            _global: Some(global),
            _shared: shared,
            _output: Some(self.run_output_lease.clone()),
            _worker: None,
        };
        // rewind 是宿主发起的维护操作，不经过工具 dispatch 网关，因此这里总是要求一次
        // 宿主审批，再在租约内执行。
        self.host.approve_tool_async("rewind", selector).await?;
        let selector = selector.to_owned();
        spawn_leased(lease, move || store.rewind(&selector))
            .await
            .map_err(|error| error.to_string())?
    }

    /// 设置本实例 `read` / workspace `read` 的单次输出预算（字节）。
    /// 入参会被 clamp 到 `[MIN_READ_OUTPUT_BYTES, MAX_READ_OUTPUT_BYTES]`，
    /// 即只影响 read 结果截断，不改变文件大小上限或其它工具行为。
    pub fn with_read_output_limit_bytes(mut self, bytes: usize) -> Self {
        self.read_output_bytes = clamp_read_output_bytes(bytes);
        self
    }

    /// 当前实例生效的 `read` 输出预算（字节）。
    pub fn read_output_limit_bytes(&self) -> usize {
        self.read_output_bytes
    }

    /// 是否为只读模式
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 该 run 持有的项目外共享 workspace activity 租约。
    ///
    /// 每个 `Workspace` 都在构造时无条件取得它（与 `.mio/outputs` 是否已存在无关），并
    /// 保留到最后一个克隆被丢弃；只读句柄，用于观测（例如宿主确认为什么离线清理报告
    /// `Busy`），不提供任何解锁操作：丢弃 `Workspace` 即释放。
    pub fn output_activity_lease(
        &self,
    ) -> Option<&crate::shell::workspace_activity::ActivityLease> {
        Some(self.run_output_lease.as_ref())
    }

    /// 获取绑定的工作目录绝对路径。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 执行 Workspace 工具操作。
    pub async fn execute(&self, args: Arguments) -> Result<String, String> {
        let op = args.operation.as_str();
        if args.expected_sha256.is_some() && op != "edit" {
            return Err("expected_sha256 仅适用于 edit 操作".into());
        }
        if args.expected_after_sha256.is_some() && op != "edit" {
            return Err("expected_after_sha256 仅适用于 edit 操作".into());
        }
        if args.include_sha256 == Some(true) && op != "read" {
            return Err("include_sha256 仅适用于 read 操作".into());
        }
        if args.preview == Some(true) && op != "edit" {
            return Err("preview 仅适用于 edit 操作".into());
        }
        let structured_edit_preview = op == "edit" && args.preview == Some(true);
        let is_write = matches!(op, "write" | "edit" | "shell" | "bash");
        if !self.tool_filter.allows_workspace_operation(op) {
            return Err(format!("工具过滤策略禁止执行 '{op}' 操作"));
        }
        if self.read_only && is_write {
            return Err(format!("只读工作区禁止执行 '{op}' 操作"));
        }
        let shared = match &self.shared_limits {
            Some(shared) => Some(
                shared
                    .acquire_workspace(&self.cwd, is_write)
                    .await
                    .map_err(|error| format!("Workspace admission: {error}"))?,
            ),
            None => None,
        };
        // Independent Workspace instances (including a cancelled child whose
        // blocking worker is still running) must share overlapping path leases.
        let global = workspace_admission()
            .acquire_task(crate::coordinator::TaskRequest {
                dedup_key: None,
                path: workspace_scope(&self.cwd),
                write: is_write,
            })
            .await
            .map_err(|error| format!("Workspace admission: {error}"))?;
        let access = if is_write {
            WorkspaceAccess::Write {
                _guard: self.access.clone().write_owned().await,
            }
        } else {
            WorkspaceAccess::Read {
                _guard: self.access.clone().read_owned().await,
            }
        };
        let parent = self
            .subagents
            .as_ref()
            .map(|manager| manager.parent_operation(is_write))
            .transpose()?;
        // Count this operation as a real worker on the bound task. The guard is
        // moved with the lease into any blocking closure, so it is released only
        // when the worker actually ends; a released task never opens a new one.
        // Every refusal must stop the operation rather than being swallowed: a
        // task that never admitted the worker could otherwise settle with
        // `workers == 0` while a blocking disk operation is created right after.
        let worker = match crate::task_lifecycle::current_task_handle() {
            Some(handle) => Some(handle.begin_worker().map_err(|error| match error {
                crate::task_lifecycle::TaskLifecycleError::CancelRequested { .. } => {
                    "task cancellation requested; refusing a new workspace operation".to_string()
                }
                other => format!(
                    "task worker admission failed; refusing a new workspace operation: {other}"
                ),
            })?),
            None => None,
        };
        let lease = WorkspaceLease {
            _parent: parent,
            _access: access,
            _global: Some(global),
            _shared: shared,
            _output: Some(self.run_output_lease.clone()),
            _worker: worker,
        };
        if is_write && self.policy.is_none() {
            // Approval is performed exactly once, at the dispatch boundary,
            // when this workspace belongs to an engine run. A directly
            // constructed workspace (no gate) keeps the historical
            // per-operation approval.
            self.host.approve_tool_async(op, &args.target()).await?;
        }

        let mut output = match op {
            "read" => self.execute_read(args, lease).await?,
            "write" => self.execute_write(args, lease).await?,
            "edit" => self.execute_edit(args, lease).await?,
            "grep" => self.execute_grep(args, lease).await?,
            "find" => self.execute_find(args, lease).await?,
            "ls" => self.execute_ls(args, lease).await?,
            "shell" | "bash" => {
                let _lease = lease;
                self.execute_shell(args).await?
            }
            _ => return Err("未知工具操作".into()),
        };

        if let Some(steering) = self.host.poll_steering() {
            let trimmed = steering.trim();
            if !trimmed.is_empty() {
                if structured_edit_preview {
                    let mut value: Value =
                        serde_json::from_str(&output).map_err(|error| error.to_string())?;
                    value["user_steering_message"] = json!(trimmed);
                    output = serde_json::to_string(&value).map_err(|error| error.to_string())?;
                } else {
                    output.push_str("\n\n[User Steering Message]: ");
                    output.push_str(trimmed);
                }
            }
        }

        Ok(output)
    }

    async fn execute_read(&self, args: Arguments, lease: WorkspaceLease) -> Result<String, String> {
        let path_str = args.path.ok_or("read 操作缺少 path 参数")?;
        let capability = self.capability.clone();
        // 真实派发身份只绑定在当前异步任务的 task-local 上；阻塞读线程没有该
        // scope，因此在任何 spawn_blocking 之前先取出并复制字段值。引擎未提供
        // 身份时保持 None，不用 run/tool id 冒充。
        let identity = crate::tool_identity::current();

        if args.byte_offset.is_some() || args.byte_limit.is_some() {
            if args.offset.is_some() || args.limit.is_some() {
                return Err("byte paging cannot be combined with line offset/limit".into());
            }
            let output_bytes = self.read_output_bytes;
            let budget = output_bytes
                .saturating_sub(256)
                .clamp(4, MAX_READ_OUTPUT_BYTES);
            let limit = args.byte_limit.unwrap_or(budget as u64).min(budget as u64) as usize;
            let offset = args.byte_offset.unwrap_or(0);
            let identity = identity.clone();
            let (output, observation) = spawn_leased(lease, move || {
                read_byte_page_blocking(
                    &capability,
                    &path_str,
                    offset,
                    limit,
                    ReadFormat {
                        output_bytes,
                        include_sha256: args.include_sha256.unwrap_or(false),
                    },
                    identity.as_ref(),
                )
            })
            .await
            .map_err(|error| error.to_string())??;
            if let Some(observation) = observation {
                self.host.on_read_observation(&observation);
            }
            return Ok(output);
        }

        if args.offset == Some(0) {
            return Err("offset 须从 1 开始".into());
        }
        if args.limit == Some(0) {
            return Err("limit 须大于 0".into());
        }

        let offset = args.offset.unwrap_or(1);
        let limit = args.limit;
        let output_bytes = self.read_output_bytes;
        let search_available = self.tool_filter.allows_workspace_operation("grep");
        let include_sha256 = args.include_sha256.unwrap_or(false);
        let (output, observation) = spawn_leased(lease, move || {
            read_lines_blocking(
                &capability,
                &path_str,
                offset,
                limit,
                ReadFormat {
                    output_bytes,
                    include_sha256,
                },
                identity.as_ref(),
                search_available,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        if let Some(observation) = observation {
            self.host.on_read_observation(&observation);
        }
        Ok(output)
    }

    async fn execute_write(
        &self,
        args: Arguments,
        lease: WorkspaceLease,
    ) -> Result<String, String> {
        let path_str = args.path.ok_or("write 操作缺少 path 参数")?;
        let value = args
            .value
            .or(args.content)
            .ok_or("write 操作缺少 value 或 content 参数")?;
        if value.len() > MAX_WRITE_BYTES {
            return Err("写入内容超过 1 MiB".into());
        }

        let capability = self.capability.clone();
        let checkpoints = self.checkpoints.clone();
        spawn_leased(lease, move || {
            // 先用与既有语义一致的校验保留原有拒绝消息（相对路径、无 `..`、无符号链接
            // 组件、位于工作目录内），随后所有真实 I/O 都走固定父目录 fd。
            checked_path(capability.display_path(), &path_str, true)?;
            let target = capability.target(&path_str)?;
            let stat = target.stat()?;
            if stat.kind == StatKind::Symlink {
                return Err("写入目标不能是符号链接".into());
            }

            // 可选检查点：必须在替换文件之前持久化 pre-edit 快照，快照失败则拒绝写入。
            let before = match (&checkpoints, stat.kind) {
                (None, _) => None,
                (Some(_), StatKind::Missing) => None,
                (Some(_), StatKind::File) => Some(read_write_source(&target)?),
                (Some(_), _) => return Err("检查点无法快照非普通文件目标".into()),
            };
            let checkpoint = match &checkpoints {
                Some(store) => store.record_pre_edit(
                    "write",
                    &path_str,
                    before.as_deref(),
                    value.as_bytes(),
                )?,
                None => None,
            };

            if let Err(error) = target.write_atomic(value.as_bytes(), stat.permissions) {
                if let (Some(store), Some(id)) = (&checkpoints, &checkpoint) {
                    let _ = store.mark_failed(id);
                }
                return Err(error);
            }
            if let (Some(store), Some(id)) = (&checkpoints, &checkpoint) {
                store.confirm(id)?;
            }

            Ok("文件已写入".into())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_edit(&self, args: Arguments, lease: WorkspaceLease) -> Result<String, String> {
        let path_str = args.path.ok_or("edit 操作缺少 path 参数")?;
        let old_text = args.old_text.ok_or("edit 操作缺少 old_text 参数")?;
        let new_text = args.new_text.ok_or("edit 操作缺少 new_text 参数")?;
        let expected_sha256 = args.expected_sha256;
        let expected_after_sha256 = args.expected_after_sha256;
        let preview = args.preview.unwrap_or(false);

        if let Some(hash) = &expected_sha256
            && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("expected_sha256 必须是完整的 64 位十六进制 SHA-256".into());
        }
        if let Some(hash) = &expected_after_sha256 {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("expected_after_sha256 必须是完整的 64 位十六进制 SHA-256".into());
            }
            if expected_sha256.is_none() || preview {
                return Err(
                    "expected_after_sha256 仅适用于携带 expected_sha256 的正式 edit".into(),
                );
            }
        }

        if old_text.is_empty() {
            return Err("old_text 不能为空".into());
        }
        if new_text.len() > MAX_WRITE_BYTES {
            return Err("替换内容超过 1 MiB 上限".into());
        }

        let capability = self.capability.clone();
        let checkpoints = self.checkpoints.clone();
        spawn_leased(lease, move || {
            // 先用与既有语义一致的校验保留原有拒绝消息，随后所有真实 I/O 都走固定父目录 fd。
            checked_path(capability.display_path(), &path_str, false)?;
            let target = capability.target(&path_str)?;
            let stat = target.stat()?;
            if stat.kind == StatKind::Symlink {
                return Err("不可编辑符号链接".into());
            }
            if stat.kind != StatKind::File {
                return Err("目标不是常规文件".into());
            }

            let permissions = stat.permissions.ok_or("目标不是常规文件")?;
            let file = target.open_read()?;
            // 原 inode 的元数据来自已打开 fd 的 `fstat`，后续冲突复核用同一固定父目录重开。
            let metadata = file.metadata().map_err(|e| e.to_string())?;
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

            if let Some(expected) = &expected_sha256 {
                let actual = format!("{:x}", Sha256::digest(&original_bytes));
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err("编辑冲突：文件内容 SHA-256 与 expected_sha256 不一致；请重新读取并生成新锚点".into());
                }
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
            if let Some(expected) = &expected_after_sha256 {
                let actual = format!("{:x}", Sha256::digest(modified.as_bytes()));
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err("编辑冲突：修改后内容 SHA-256 与 expected_after_sha256 不一致；请重新预览".into());
                }
            }

            // Catch edits saved by an editor or another process while we prepared
            // the replacement. This is optimistic conflict detection, not an OS
            // lock: arbitrary external writers can still race the final rename.
            ensure_edit_source_unchanged(&target, &metadata, &original_text)?;
            if preview {
                return render_edit_preview(
                    &path_str,
                    start_line,
                    &original_text,
                    &modified,
                    &old_text,
                    &new_text,
                );
            }
            // 可选检查点：紧邻原子替换之前持久化 pre-edit 快照，快照失败则拒绝写入。
            let checkpoint = match &checkpoints {
                Some(store) => store.record_pre_edit(
                    "edit",
                    &path_str,
                    Some(original_text.as_bytes()),
                    modified.as_bytes(),
                )?,
                None => None,
            };
            if let Err(error) = target.write_atomic(modified.as_bytes(), Some(permissions)) {
                if let (Some(store), Some(id)) = (&checkpoints, &checkpoint) {
                    let _ = store.mark_failed(id);
                }
                return Err(error);
            }
            if let (Some(store), Some(id)) = (&checkpoints, &checkpoint) {
                store.confirm(id)?;
            }

            let diff = crate::pattern::unified_diff(&path_str, start_line, &old_text, &new_text);
            Ok(format!("文件已成功编辑\n{diff}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn execute_grep(&self, args: Arguments, lease: WorkspaceLease) -> Result<String, String> {
        let pattern = args
            .pattern
            .or(args.value)
            .ok_or("grep 操作缺少 pattern 或 value 参数")?;
        let use_regex = args.regex.unwrap_or(false);
        let case_insensitive = args.case_insensitive.unwrap_or(false);
        let context_lines = args.context_lines.unwrap_or(0);
        if context_lines > MAX_GREP_CONTEXT_LINES {
            return Err(format!(
                "context_lines 必须在 0 到 {MAX_GREP_CONTEXT_LINES} 之间"
            ));
        }
        let context_lines = context_lines as usize;
        let matcher = crate::pattern::LineMatcher::new(&pattern, use_regex, case_insensitive)?;

        let capability = self.capability.clone();
        let subpath = args.path.unwrap_or_default();
        let effective_budget = MAX_GREP_TOTAL_BYTES.saturating_sub(GREP_NOTICE_BUDGET);

        spawn_leased(lease, move || {
            let cwd = capability.display_path().to_path_buf();
            let start = resolve_scan_start(&capability, &subpath)?;

            let mut acc = GrepAccumulator::default();
            let mut scanned_entries = 0;
            let mut truncated_traversal = false;

            match start {
                ScanStart::Node { target, kind } => match kind {
                    StatKind::Symlink => return Err("不支持搜索符号链接".into()),
                    StatKind::File => {
                        let display = target.display_path().to_path_buf();
                        grep_accumulate_file(
                            &mut acc,
                            &matcher,
                            &cwd,
                            &display,
                            target.open_read(),
                            context_lines,
                            effective_budget,
                        );
                    }
                    _ => return Err("目标不是常规文件或目录".into()),
                },
                ScanStart::Dir(dir) => {
                    let mut ctx = TraversalContext {
                        visited: 0,
                        max_entries: MAX_SEARCH_VISITED_ENTRIES,
                        max_depth: MAX_SEARCH_DEPTH,
                        truncated: false,
                    };
                    // 流式遍历：每读到一个普通文件就立即通过固定父目录 `openat` 打开、读完
                    // 并释放，不在扫描阶段累积持有父目录 fd 的条目。
                    walk_entries_pinned(&dir, 0, &mut ctx, &mut |child| {
                        if child.kind() != NodeKind::File {
                            return Ok(false);
                        }
                        Ok(grep_accumulate_file(
                            &mut acc,
                            &matcher,
                            &cwd,
                            child.display_path(),
                            child.open_file(),
                            context_lines,
                            effective_budget,
                        ))
                    })?;
                    scanned_entries = ctx.visited;
                    truncated_traversal = ctx.truncated;
                }
            }

            // 命中优先：真实命中行已按既有预算累积并保持旧输出格式，上下文只在剩余预算
            // 内按 (路径, 行号) 合并，绝不挤掉任何命中行。
            let mut entries = std::mem::take(&mut acc.matches);
            if context_lines > 0 {
                let mut contexts = std::mem::take(&mut acc.contexts);
                contexts.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
                let mut remaining = effective_budget.saturating_sub(acc.total_output_bytes);
                for (path, line, formatted) in contexts {
                    let needed = formatted.len() + 1;
                    if needed > remaining {
                        acc.hit_byte_limit = true;
                        break;
                    }
                    remaining -= needed;
                    entries.push((path, line, formatted));
                }
            }

            let mut notices = Vec::new();
            if acc.hit_match_limit {
                notices.push(format!("达到最大匹配数限制 {MAX_GREP_MATCHES}"));
            }
            if acc.hit_byte_limit || acc.context_saturated {
                notices.push(format!("达到最大输出字节数限制 {MAX_GREP_TOTAL_BYTES}"));
            }
            if truncated_traversal {
                notices.push(format!(
                    "目录遍历达到上限 (已访问 {scanned_entries} 项/深度 {MAX_SEARCH_DEPTH})"
                ));
            }
            if acc.read_errors > 0 {
                notices.push(format!("{} 个文件读取失败/权限不足", acc.read_errors));
            }

            if entries.is_empty() {
                // 无命中时不返回任何孤立上下文，保持既有文案。
                if acc.read_errors > 0 {
                    Ok(format!(
                        "未找到匹配内容 (警告: {} 个文件因权限不足等原因未能读取)",
                        acc.read_errors
                    ))
                } else {
                    Ok("未找到匹配内容".into())
                }
            } else {
                // 输出顺序与修复前一致：按相对路径、再按行号排序（原来对文件列表整体排序）。
                entries.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
                let mut matches_str = entries
                    .iter()
                    .map(|(_, _, formatted)| formatted.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
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

    async fn execute_find(&self, args: Arguments, lease: WorkspaceLease) -> Result<String, String> {
        let pattern = args
            .pattern
            .or(args.value)
            .ok_or("find 操作缺少 pattern 或 value 参数")?;

        let capability = self.capability.clone();
        let subpath = args.path.unwrap_or_default();
        let is_glob = crate::pattern::has_glob_meta(&pattern);

        spawn_leased(lease, move || {
            let cwd = capability.display_path().to_path_buf();
            let start = resolve_scan_start(&capability, &subpath)?;

            let mut ctx = TraversalContext {
                visited: 0,
                max_entries: MAX_SEARCH_VISITED_ENTRIES,
                max_depth: MAX_SEARCH_DEPTH,
                truncated: false,
            };
            let mut matched_entries: Vec<String> = Vec::new();
            let mut hit_limit = false;

            match start {
                ScanStart::Dir(dir) => {
                    // 流式遍历：只保留命中的相对路径字符串，不保留任何条目句柄；下降到
                    // 子目录时也只在递归期间持有对应目录 fd。
                    walk_entries_pinned(&dir, 0, &mut ctx, &mut |child| {
                        if find_matches_entry(
                            &pattern,
                            is_glob,
                            &cwd,
                            child.display_path(),
                            &mut matched_entries,
                        ) {
                            hit_limit = true;
                            return Ok(true);
                        }
                        Ok(false)
                    })?;
                }
                ScanStart::Node { target, kind } => match kind {
                    StatKind::Symlink => return Err("不支持搜索符号链接".into()),
                    _ => {
                        if find_matches_entry(
                            &pattern,
                            is_glob,
                            &cwd,
                            target.display_path(),
                            &mut matched_entries,
                        ) {
                            hit_limit = true;
                        }
                    }
                },
            }

            matched_entries.sort();

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

    async fn execute_ls(&self, args: Arguments, lease: WorkspaceLease) -> Result<String, String> {
        let capability = self.capability.clone();
        let subpath = args.path.unwrap_or_default();

        spawn_leased(lease, move || {
            let dir = match resolve_scan_start(&capability, &subpath)? {
                ScanStart::Dir(dir) => dir,
                // 目录列举不接受符号链接：`resolve_scan_start` 已经用固定父目录的
                // `fstatat(AT_SYMLINK_NOFOLLOW)` 分类，叶子是链接时在这里被拒绝。
                ScanStart::Node {
                    kind: StatKind::Symlink,
                    ..
                } => return Err("不可列出符号链接目录".into()),
                ScanStart::Node { .. } => return Err("目标不是目录".into()),
            };

            let mut entries = Vec::new();
            let mut truncated = false;

            // 流式列举：逐个读取并立即释放条目，只保留有界的展示字符串，绝不先无界收集
            // 整个目录再截断。
            for (total_scanned, child) in dir.children()?.enumerate() {
                let child = child?;
                if total_scanned >= MAX_SEARCH_VISITED_ENTRIES {
                    truncated = true;
                    break;
                }

                if is_ignored_name(child.name()) {
                    continue;
                }

                let suffix = match child.kind() {
                    NodeKind::Dir => "/",
                    NodeKind::Symlink => "@",
                    _ => "",
                };

                entries.push(format!("{}{suffix}", child.name()));
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
        // Route through the policy-aware entry point so the host's requested
        // isolation reaches the shell path. The default host implementation
        // fails closed for a restricted policy instead of calling `shell`.
        let shell_fut = self
            .host
            .shell_with_policy(&command, &self.cwd, &self.execution_policy);
        match tokio::time::timeout(
            Duration::from_secs(SHELL_TIMEOUT_SECS + SHELL_HOST_BACKSTOP_GRACE_SECS),
            shell_fut,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // This outer timeout wraps an arbitrary [`Host`] future, so the
                // engine cannot know whether that host already spawned a
                // process: it is conservatively unknown. The native default
                // path reports its own real post-spawn state before this
                // fires, so this marker is the custom-host backstop, never a
                // second classification of the same call.
                let effect = UnknownSideEffect::new(
                    "shell",
                    crate::shell::native_shell_target(&command),
                    UnknownReason::HostSupervision,
                    crate::tool_identity::current_tool_call_id(),
                );
                crate::unknown_effect::publish_supervision_unknown(effect);
                Err("命令超过 120 秒，已取消命令协程；宿主须自行回收子进程".to_string())
            }
        }
    }
}

// All built-in aliases share this lock. Reads may overlap, while an edit,
// replacement or shell excludes other operations in the same Workspace.
// Keep the guard with the worker across cancellation, just like the parent lease.
enum WorkspaceAccess {
    Read {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Write {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}
struct WorkspaceLease {
    _parent: Option<crate::subagent::ParentLease>,
    _access: WorkspaceAccess,
    _global: Option<crate::coordinator::TaskPermit>,
    _shared: Option<crate::shared_limits::WorkspacePermit>,
    // Keep the run's shared workspace activity lease (located outside the
    // project directory) alive for the whole worker, including blocking workers
    // that outlive a cancelled run future. Without it, dropping the run could
    // release the lease while a disk worker still reads a published output,
    // letting an offline cleanup delete it mid-read.
    _output: Option<Arc<crate::shell::workspace_activity::ActivityLease>>,
    // Count this real worker against the bound task until the worker ends. It is
    // moved into any blocking closure together with the lease, so a single
    // lease drop (e.g. a cancelled async caller) never settles the whole turn.
    _worker: Option<crate::task_lifecycle::TaskWorkerGuard>,
}

fn workspace_admission() -> &'static crate::coordinator::Coordinator {
    static ADMISSION: OnceLock<crate::coordinator::Coordinator> = OnceLock::new();
    ADMISSION.get_or_init(|| {
        crate::coordinator::Coordinator::new(crate::coordinator::Limits {
            agent: 1024,
            queue_capacity: 65_536,
            ..Default::default()
        })
        .expect("workspace admission limits are valid")
    })
}

// Encode components separately so ancestors remain ancestors without exposing
// raw paths or confusing a literal filename with navigation syntax.
fn workspace_scope(cwd: &Path) -> String {
    use std::fmt::Write;
    let mut scope = String::from("filesystem");
    for component in cwd.components() {
        scope.push('/');
        #[cfg(windows)]
        let folded = component.as_os_str().to_string_lossy().to_lowercase();
        #[cfg(windows)]
        let bytes = folded.as_bytes();
        #[cfg(not(windows))]
        let bytes = component.as_os_str().as_encoded_bytes();
        for byte in bytes {
            write!(scope, "{byte:02x}").expect("writing to a String succeeds");
        }
    }
    scope
}

// A cancelled async caller cannot cancel an already running blocking operation.
// Move the lease onto that worker so delegation stays blocked until it really ends.
fn spawn_leased<T: Send + 'static>(
    lease: WorkspaceLease,
    operation: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        operation()
    })
}

fn render_edit_preview(
    path: &str,
    start_line: usize,
    before: &str,
    after: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, String> {
    let mut diff = crate::pattern::unified_diff(path, start_line, old_text, new_text);
    // unified_diff already summarizes after 40 lines; report that loss as well
    // as any further byte clipping performed for this structured result.
    let mut diff_truncated = old_text.lines().count() > 40 || new_text.lines().count() > 40;
    if diff.len() > MAX_EDIT_PREVIEW_OUTPUT_BYTES {
        let mut end = MAX_EDIT_PREVIEW_OUTPUT_BYTES;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff_truncated = true;
    }
    let before_sha256 = format!("{:x}", Sha256::digest(before.as_bytes()));
    let after_sha256 = format!("{:x}", Sha256::digest(after.as_bytes()));
    loop {
        let result = serde_json::to_string(&json!({
            "status": "preview",
            "path": path,
            "start_line": start_line,
            "before_sha256": before_sha256,
            "after_sha256": after_sha256,
            "before_bytes": before.len(),
            "after_bytes": after.len(),
            "diff": diff,
            "diff_truncated": diff_truncated,
        }))
        .map_err(|error| error.to_string())?;
        if result.len() <= MAX_EDIT_PREVIEW_OUTPUT_BYTES {
            return Ok(result);
        }
        if diff.is_empty() {
            return Err("编辑预览元数据超过输出上限".into());
        }
        let mut end = diff.len() / 2;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff_truncated = true;
    }
}

/// 能力句柄版本的字节分页。
///
/// 先用与既有语义一致的 `checked_path` 校验相对路径（相对路径、无 `..`、无符号链接组件、
/// 位于工作目录内），随后对固定父目录 fd 做 `openat`，之后所有读取都来自该 fd，不再重新
/// 解析可变绝对路径。
fn read_byte_page_pinned(
    root: &workspace_capability::Root,
    path_str: &str,
    offset: u64,
    limit: usize,
    include_sha256: bool,
) -> Result<(crate::file_page::BytePage, Option<String>), String> {
    if !(4..=MAX_READ_OUTPUT_BYTES).contains(&limit) {
        return Err("byte_limit must be between 4 and 65536 bytes".into());
    }
    checked_path(root.display_path(), path_str, false)?;
    let target = root.target(path_str)?;
    let mut file = target.open_read()?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("byte paging requires a regular file".into());
    }
    if metadata.len() > MAX_READ_INPUT_BYTES as u64 {
        return Err("file exceeds the 8 MiB byte paging limit".into());
    }
    if offset > metadata.len() {
        return Err("byte_offset is beyond the end of the file".into());
    }
    let (mut bytes, total_bytes, sha256) = if include_sha256 {
        // Hash and page come from the same pinned read. Reading a separate page
        // before hashing could return a hash for different content if another
        // process changes the file between the two reads.
        let mut all = Vec::new();
        file.take((MAX_READ_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut all)
            .map_err(|error| error.to_string())?;
        if all.len() > MAX_READ_INPUT_BYTES {
            return Err("file exceeds the 8 MiB byte paging limit".into());
        }
        if offset > all.len() as u64 {
            return Err("byte_offset is beyond the end of the file".into());
        }
        let hash = format!("{:x}", Sha256::digest(&all));
        let start = offset as usize;
        let end = start.saturating_add(limit).min(all.len());
        (all[start..end].to_vec(), all.len() as u64, Some(hash))
    } else {
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        (bytes, metadata.len(), None)
    };
    let usable = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error)
            if error.error_len().is_none()
                && error.valid_up_to() > 0
                && offset + (bytes.len() as u64) < total_bytes =>
        {
            error.valid_up_to()
        }
        Err(_) => {
            return Err("file is not UTF-8 or byte_offset is not at a character boundary".into());
        }
    };
    if usable == 0 && offset < total_bytes {
        return Err("file changed while reading the byte page".into());
    }
    bytes.truncate(usable);
    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    let next = offset + usable as u64;
    Ok((
        crate::file_page::BytePage {
            text,
            byte_offset: offset,
            next_byte_offset: (next < total_bytes).then_some(next),
            total_bytes,
        },
        sha256,
    ))
}

/// Blocking body of one bounded byte page plus its content-free observation.
///
/// It runs inside `spawn_blocking`, where no task-local scope exists, so the
/// caller passes the identity it already copied off the async task. The
/// reported range is the bytes actually returned, never the request.
fn read_byte_page_blocking(
    root: &workspace_capability::Root,
    path_str: &str,
    offset: u64,
    limit: usize,
    format: ReadFormat,
    identity: Option<&ToolIdentity>,
) -> Result<(String, Option<ReadObservation>), String> {
    let (mut page, sha256) =
        read_byte_page_pinned(root, path_str, offset, limit, format.include_sha256)?;
    // Measure the real JSON page inside the canonical model-facing tool
    // output. Ordinary UTF-8 need not pay the worst-case control-character
    // escaping penalty on every page. Search only the already-read buffer.
    let encode = |end: usize| -> Result<(String, usize), String> {
        let next = page.byte_offset + end as u64;
        let candidate = crate::file_page::BytePage {
            text: page.text[..end].to_owned(),
            byte_offset: page.byte_offset,
            next_byte_offset: (next < page.total_bytes).then_some(next),
            total_bytes: page.total_bytes,
        };
        let encoded = if let Some(hash) = &sha256 {
            let mut value = serde_json::to_value(&candidate).map_err(|error| error.to_string())?;
            value["sha256"] = json!(hash);
            serde_json::to_string(&value).map_err(|error| error.to_string())?
        } else {
            serde_json::to_string(&candidate).map_err(|error| error.to_string())?
        };
        let model_output = mio_core::tool::ToolOutput::json(json!({"output": &encoded}));
        let size = serde_json::to_vec(&model_output)
            .map_err(|error| error.to_string())?
            .len();
        Ok((encoded, size))
    };
    let (mut output, size) = encode(page.text.len())?;
    if size > format.output_bytes {
        let mut low = 0;
        let mut high = page.text.len();
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let mut end = middle;
            while !page.text.is_char_boundary(end) {
                end -= 1;
            }
            if encode(end)?.1 <= format.output_bytes {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        while !page.text.is_char_boundary(low) {
            low -= 1;
        }
        let (encoded, size) = encode(low)?;
        if size > format.output_bytes || (low == 0 && !page.text.is_empty()) {
            return Err(
                "read output budget cannot fit a byte page; increase the read output budget".into(),
            );
        }
        output = encoded;
        page.text.truncate(low);
        let next = page.byte_offset + low as u64;
        page.next_byte_offset = (next < page.total_bytes).then_some(next);
    }
    // 真实返回字节数来自分页结果本身，不是请求参数。
    let returned = page.text.len();
    let range = if returned == 0 {
        // 零长度页没有可表达的 limit，显式记为未知范围。
        ReadRange::Unknown
    } else {
        ReadRange::Bytes {
            offset: page.byte_offset,
            limit: returned as u64,
        }
    };
    let observation = normalize_read_path(path_str).map(|path| {
        ReadObservation::new(
            identity,
            path,
            range,
            Some(page.total_bytes),
            page.next_byte_offset.is_some(),
        )
    });
    Ok((output, observation))
}

/// 字节上限截断时追加的通用检索指引。
///
/// 工具描述中已有同类建议，但返回 header 若只给 `Next offset`，容易让模型连续顺序
/// 扫描。这里在同一个 metadata header 内补一句通用指导；`Next offset` 仍位于
/// header 末尾，原有解析不受影响。该文本的全部 UTF-8 字节计入原有输出上限，
/// 不放宽 `read` cap。
const TRUNCATED_READ_GUIDANCE: &str = "Use grep to locate relevant sections before paging further.";

/// 渲染一个截断页的 metadata header。
///
/// 预算计算与最终输出共用此函数，避免两者漂移；`last_line` 是实际展示的最后一行
/// 行号，`next_offset` 是第一行未展示的行号。`byte_capped` 仅在本次截断确由字节
/// 上限触发（而非调用方显式 `limit`）时为 `true`，此时才插入
/// [`TRUNCATED_READ_GUIDANCE`]。
fn render_truncated_read_header(
    offset: u64,
    last_line: u64,
    total_lines: usize,
    next_offset: u64,
    byte_capped: bool,
) -> String {
    if byte_capped {
        format!(
            "[TRUNCATED: Showing lines {offset}..{last_line} of {total_lines}. {} Next offset: {next_offset}]\n",
            TRUNCATED_READ_GUIDANCE
        )
    } else {
        format!(
            "[TRUNCATED: Showing lines {offset}..{last_line} of {total_lines}. Next offset: {next_offset}]\n"
        )
    }
}

/// 按行区间重建一页正文（末行是文件末行时不补换行）。
///
/// 仅用于字节上限截断后为了容纳指引而从尾部整行回退，保证分页拼接既不丢行也
/// 不丢换行。
fn joined_read_page(
    lines: &[&str],
    start_idx: usize,
    end_idx: usize,
    total_lines: usize,
) -> String {
    let mut page = String::new();
    for (index, line) in lines[start_idx..end_idx].iter().enumerate() {
        page.push_str(line);
        if start_idx + index + 1 < total_lines {
            page.push('\n');
        }
    }
    page
}

/// Blocking body of one bounded line read plus its content-free observation.
///
/// The reported range is the lines actually returned, taken from the same
/// pagination loop that produced the output, never the requested parameters.
/// An empty file reports an explicit zero total with an unknown range, and a
/// failed or rejected read returns `Err` without any observation.
struct ReadFormat {
    output_bytes: usize,
    include_sha256: bool,
}

fn read_lines_blocking(
    root: &workspace_capability::Root,
    path_str: &str,
    offset: u64,
    limit: Option<u64>,
    format: ReadFormat,
    identity: Option<&ToolIdentity>,
    search_available: bool,
) -> Result<(String, Option<ReadObservation>), String> {
    checked_path(root.display_path(), path_str, false)?;
    let target = root.target(path_str)?;
    let file = target.open_read()?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("不支持直接读取符号链接文件".into());
    }
    if !metadata.is_file() {
        return Err("目标不是常规文件".into());
    }

    let mut handle = file.take((MAX_READ_INPUT_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    handle.read_to_end(&mut bytes).map_err(|e| e.to_string())?;

    if bytes.len() > MAX_READ_INPUT_BYTES {
        return Err(format!(
            "文件超过最大读取上限 ({} 字节 / 8 MiB)，请使用 shell 命令分段处理",
            MAX_READ_INPUT_BYTES
        ));
    }

    let hash_prefix = if format.include_sha256 {
        format!("[sha256: {:x}]\n", Sha256::digest(&bytes))
    } else {
        String::new()
    };
    let output_bytes = format.output_bytes.saturating_sub(hash_prefix.len());
    let text = String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 编码文本")?;

    // 空文件没有可表达的行范围：显式记为 0 总量 + 未知范围。
    if text.is_empty() {
        let observation = normalize_read_path(path_str)
            .map(|path| ReadObservation::new(identity, path, ReadRange::Unknown, Some(0), false));
        return Ok((hash_prefix, observation));
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();

    let start_line_idx = (offset - 1) as usize;
    if start_line_idx >= total_lines {
        return Err(format!("offset {offset} 超出文件总行数 {total_lines}"));
    }

    let max_lines = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let mut out = String::new();
    let mut current_idx = start_line_idx;
    let mut truncated = false;
    let mut hit_limit = false;

    while current_idx < total_lines && (current_idx - start_line_idx) < max_lines {
        let line = lines[current_idx];
        let line_len_with_nl = line.len() + if current_idx + 1 < total_lines { 1 } else { 0 };

        // 预算按最宽的 header 计算：行号取上界，且始终不把指引算进来，
        // 以保留固定的旧页面边界与错误阈值；确由字节上限截断时，下面再
        // 为指引让出字节。
        let header_len = render_truncated_read_header(
            offset,
            total_lines as u64,
            total_lines,
            total_lines as u64 + 1,
            false,
        )
        .len();
        let budget = output_bytes.saturating_sub(header_len);

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

    let was_truncated = truncated || hit_limit;

    // 仅字节上限触发的截断需要把 header 内指引的字节也让出来：显式 `limit`
    // 分页与完整读取保持旧页面大小与阈值。为容纳指引，从尾部整行回退，
    // 至少保留一行；这样 `header + out` 始终不超过原始输出上限。
    let header = if !was_truncated {
        None
    } else if !truncated || !search_available {
        Some(render_truncated_read_header(
            offset,
            current_idx as u64,
            total_lines,
            current_idx as u64 + 1,
            false,
        ))
    } else {
        let mut candidate = render_truncated_read_header(
            offset,
            current_idx as u64,
            total_lines,
            current_idx as u64 + 1,
            true,
        );
        while current_idx > start_line_idx + 1 && candidate.len() + out.len() > output_bytes {
            current_idx -= 1;
            out = joined_read_page(&lines, start_line_idx, current_idx, total_lines);
            candidate = render_truncated_read_header(
                offset,
                current_idx as u64,
                total_lines,
                current_idx as u64 + 1,
                true,
            );
        }
        if candidate.len() + out.len() > output_bytes {
            // 单行加指引仍放不下时退回无指引 header：旧错误/页面行为不变。
            candidate = render_truncated_read_header(
                offset,
                current_idx as u64,
                total_lines,
                current_idx as u64 + 1,
                false,
            );
        }
        Some(candidate)
    };

    let returned_lines = (current_idx - start_line_idx) as u64;
    let range = if returned_lines == 0 {
        ReadRange::Unknown
    } else {
        ReadRange::Lines {
            offset,
            limit: returned_lines,
        }
    };
    let observation = normalize_read_path(path_str).map(|path| {
        ReadObservation::new(
            identity,
            path,
            range,
            Some(total_lines as u64),
            was_truncated,
        )
    });

    match header {
        Some(header) => Ok((format!("{hash_prefix}{header}{out}"), observation)),
        None => Ok((format!("{hash_prefix}{out}"), observation)),
    }
}

/// 读取 `write` 覆盖前的原文件内容，并强制检查点快照上限。
/// 仅在启用检查点时调用；超出上限时拒绝覆盖而不是产生不完整快照。
/// 读取只经过固定父目录 fd 上的 `openat`，不重新解析可变绝对路径。
fn read_write_source(target: &workspace_capability::Target) -> Result<Vec<u8>, String> {
    let file = target.open_read()?;
    let mut bytes = Vec::new();
    file.take((MAX_WRITE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取原文件失败: {e}"))?;
    if bytes.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "文件超过检查点快照上限 ({} 字节)，已拒绝在启用检查点时覆盖",
            MAX_WRITE_BYTES
        ));
    }
    Ok(bytes)
}

/// 编辑前的原 inode / 内容复核。
///
/// 用同一固定父目录 fd 重新 `openat` 叶子，比对新旧 `len` / `modified` / dev / ino 与
/// 完整字节内容。这是乐观冲突检测，不是 OS 锁：任意外部写入仍可抢在最终 `renameat`
/// 之前，但至少不会把「已被替换成另一个 inode 或另一份内容」的目录项当成原文件。
fn ensure_edit_source_unchanged(
    target: &workspace_capability::Target,
    original_metadata: &std::fs::Metadata,
    original: &str,
) -> Result<(), String> {
    let conflict = || "编辑冲突：文件在读取后被修改或替换，请重新读取后重试".to_string();
    let file = target.open_read().map_err(|_| conflict())?;
    let metadata = file.metadata().map_err(|_| conflict())?;
    if !metadata.is_file()
        || metadata.len() != original_metadata.len()
        || metadata.modified().ok() != original_metadata.modified().ok()
    {
        return Err(conflict());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != original_metadata.dev() || metadata.ino() != original_metadata.ino() {
            return Err(conflict());
        }
    }
    let mut current = Vec::new();
    file.take((MAX_WRITE_BYTES + 1) as u64)
        .read_to_end(&mut current)
        .map_err(|_| conflict())?;
    if current != original.as_bytes() {
        return Err(conflict());
    }
    Ok(())
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

/// `grep` / `find` / `ls` 的起点：已固定的目录句柄，或分类已确定的单个叶子。
enum ScanStart {
    Dir(workspace_capability::Dir),
    Node {
        target: workspace_capability::Target,
        kind: StatKind,
    },
}

/// `grep` / `find` / `ls` 的 `path` 是否只是工作区根目录的别名。
///
/// 省略 `path`（空值 / 纯空白）、`"."`、`"./"`、`"./."` 都指向同一个根目录，必须与
/// 省略 `path` 走完全相同的路径，否则同一意图会因为写法不同得到「必须在工作区根目录
/// 之下」这类假拒绝。绝对路径、`..` 与非 `.` 组件一律返回 `false`，继续交给
/// [`checked_path`] 与能力层给出原有的拒绝消息：这里只收窄「根目录别名」一处，不放开任何
/// 子路径、父级遍历或符号链接逃逸。
fn is_scan_root_alias(subpath: &str) -> bool {
    if Path::new(subpath).is_absolute() {
        return false;
    }
    if subpath.trim().is_empty() {
        return true;
    }
    subpath
        .split('/')
        .all(|segment| segment.is_empty() || segment == ".")
}

/// 把相对 `subpath` 解析成一个已固定的起点。
///
/// 先用与既有语义一致的 `checked_path` 校验并保留原有拒绝消息，再用固定父目录 fd 上的
/// `fstatat(AT_SYMLINK_NOFOLLOW)` 分类；目录会被真正打开成 [`workspace_capability::Dir`]，
/// 其余分类交回给各自调用方决定拒绝还是接受。
fn resolve_scan_start(
    root: &workspace_capability::Root,
    subpath: &str,
) -> Result<ScanStart, String> {
    if is_scan_root_alias(subpath) {
        return Ok(ScanStart::Dir(root.root_dir()?));
    }
    checked_path(root.display_path(), subpath, false)?;
    let target = root.target(subpath)?;
    let kind = target.stat()?.kind;
    if kind == StatKind::Dir {
        Ok(ScanStart::Dir(root.dir(subpath)?))
    } else {
        Ok(ScanStart::Node { target, kind })
    }
}

/// `grep` 读取阶段的累加状态。
///
/// 只保留有界的匹配结果与计数（匹配行数上限 `MAX_GREP_MATCHES`、输出字节上限
/// `MAX_GREP_TOTAL_BYTES`），不保留任何目录句柄，因此遍历很多目录也不会累积 fd。
/// 邻近上下文只在 `context_lines > 0` 时填入有界候选缓冲，上限同样是原输出字节预算。
#[derive(Default)]
struct GrepAccumulator {
    /// (相对路径, 行号, 已格式化的输出行)
    matches: Vec<(String, usize, String)>,
    /// 邻近上下文候选行；仅在请求了上下文时填充。
    contexts: Vec<(String, usize, String)>,
    context_bytes: usize,
    /// 上下文候选缓冲已达输出预算，输出中的上下文被截断且必须明确标记。
    context_saturated: bool,
    total_output_bytes: usize,
    hit_match_limit: bool,
    hit_byte_limit: bool,
    read_errors: usize,
}

/// 使用既有单行上限与总输出预算渲染一行 `rel_path{sep}line{sep} ...`。
///
/// 命中行用 `:` 分隔（保持旧输出），上下文行用 `-` 分隔以便区分；返回 `None`
/// 表示剩余总预算已放不下这一行的前缀。按 UTF-8 边界截断，不新增任何额度。
fn grep_render_line(
    rel_path: &str,
    line_no: usize,
    separator: char,
    line: &str,
    used_bytes: usize,
    effective_budget: usize,
) -> Option<String> {
    let prefix = format!("{rel_path}{separator}{line_no}{separator} ");
    let remaining_total = effective_budget.saturating_sub(used_bytes + prefix.len() + 1);
    if remaining_total == 0 {
        return None;
    }
    let line_trunc_tag = " ... [TRUNCATED LINE]";
    let max_line_payload = MAX_GREP_LINE_BYTES.saturating_sub(line_trunc_tag.len());
    let allowed_line_bytes = max_line_payload.min(remaining_total);
    let formatted = if line.len() > allowed_line_bytes {
        let mut end = allowed_line_bytes.saturating_sub(line_trunc_tag.len());
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{prefix}{}{line_trunc_tag}", &line[..end])
    } else {
        format!("{prefix}{line}")
    };
    Some(formatted)
}

/// 读取一个已按固定父目录打开的普通文件并累积匹配。
///
/// 第一次遍历只累积真实命中（行号、格式与预算行为与原实现一致），并按需记录命中行
/// 下标（上限 `MAX_GREP_MATCHES`）；`context_lines > 0` 时在同一份已读入的有界文本上
/// 第二次遍历补齐上下文。返回 `true` 表示已达匹配数或输出字节上限，调用方应停止遍历。
fn grep_accumulate_file(
    acc: &mut GrepAccumulator,
    matcher: &crate::pattern::LineMatcher,
    cwd: &Path,
    display: &Path,
    file: Result<File, String>,
    context_lines: usize,
    effective_budget: usize,
) -> bool {
    let file = match file {
        Ok(file) => file,
        Err(_) => {
            acc.read_errors += 1;
            return false;
        }
    };
    let mut handle = file.take((MAX_GREP_FILE_BYTES + 1) as u64);
    let mut content = Vec::new();
    if handle.read_to_end(&mut content).is_err() {
        acc.read_errors += 1;
        return false;
    }
    if content.len() > MAX_GREP_FILE_BYTES {
        return false; // 超过 2 MiB 跳过
    }
    let text = match String::from_utf8(content) {
        Ok(text) => text,
        Err(_) => return false, // 非 UTF8 跳过
    };

    let rel_path = display
        .strip_prefix(cwd)
        .unwrap_or(display)
        .to_string_lossy()
        .to_string();

    let mut match_indices: Vec<usize> = Vec::new();
    let mut limit_reached = false;
    for (idx, line) in text.lines().enumerate() {
        if !matcher.is_match(line) {
            continue;
        }
        let formatted = match grep_render_line(
            &rel_path,
            idx + 1,
            ':',
            line,
            acc.total_output_bytes,
            effective_budget,
        ) {
            Some(formatted) => formatted,
            None => {
                acc.hit_byte_limit = true;
                limit_reached = true;
                break;
            }
        };

        let needed_bytes = formatted.len() + 1;
        if acc.total_output_bytes + needed_bytes > effective_budget {
            acc.hit_byte_limit = true;
            limit_reached = true;
            break;
        }

        acc.total_output_bytes += needed_bytes;
        if context_lines > 0 {
            match_indices.push(idx);
        }
        acc.matches.push((rel_path.clone(), idx + 1, formatted));
        if acc.matches.len() >= MAX_GREP_MATCHES {
            acc.hit_match_limit = true;
            limit_reached = true;
            break;
        }
    }

    // 命中已全部累积后才补上下文：即使命中已经触达上限，本文件仍能得到上下文，
    // 但预算已被命中占满时不再浪费一次上下文遍历。
    if context_lines > 0 && !acc.hit_byte_limit && !acc.context_saturated {
        grep_accumulate_context(
            acc,
            &text,
            &match_indices,
            context_lines,
            &rel_path,
            effective_budget,
        );
    }
    limit_reached
}

/// 在已读入的有界 UTF-8 文本上第二次遍历，为已累积的命中行补齐邻近上下文。
///
/// 窗口按行号相交或相邻合并去重；命中行本身不重复输出；上下文只用 `path-line-`
/// 分隔并限定在原输出字节预算内，超出即标记截断，绝不由上下文反向决定命中取舍。
fn grep_accumulate_context(
    acc: &mut GrepAccumulator,
    text: &str,
    match_indices: &[usize],
    context_lines: usize,
    rel_path: &str,
    effective_budget: usize,
) {
    // 命中行下标按行号递增，相交或相邻窗口合并为连续区间，保证同一行只输出一次。
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &index in match_indices {
        let start = index.saturating_sub(context_lines);
        let end = index + context_lines;
        match merged.last_mut() {
            Some((_, last_end)) if start <= last_end.saturating_add(1) => {
                if end > *last_end {
                    *last_end = end;
                }
            }
            _ => merged.push((start, end)),
        }
    }
    if merged.is_empty() {
        return;
    }

    let mut windows = merged.into_iter();
    let mut window = windows.next();
    let mut match_cursor = 0usize;
    for (idx, line) in text.lines().enumerate() {
        while let Some((_, end)) = window {
            if idx > end {
                window = windows.next();
            } else {
                break;
            }
        }
        let Some((start, _)) = window else {
            break;
        };
        if idx < start {
            continue;
        }
        if match_cursor < match_indices.len() && match_indices[match_cursor] == idx {
            match_cursor += 1;
            continue;
        }
        let Some(formatted) = grep_render_line(
            rel_path,
            idx + 1,
            '-',
            line,
            acc.total_output_bytes + acc.context_bytes,
            effective_budget,
        ) else {
            acc.context_saturated = true;
            return;
        };
        let needed = formatted.len() + 1;
        if acc.context_bytes + needed > effective_budget {
            acc.context_saturated = true;
            return;
        }
        acc.context_bytes += needed;
        acc.contexts.push((rel_path.to_owned(), idx + 1, formatted));
    }
}

/// 判断 `find` 的一条路径是否命中；命中时压入结果，返回 `true` 表示已达匹配数上限。
fn find_matches_entry(
    pattern: &str,
    is_glob: bool,
    cwd: &Path,
    display: &Path,
    matched_entries: &mut Vec<String>,
) -> bool {
    let rel_path = display
        .strip_prefix(cwd)
        .unwrap_or(display)
        .to_string_lossy()
        .to_string();
    let file_name = display
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();

    let matched = if is_glob {
        crate::pattern::matches_glob(pattern, &rel_path)
            || (!pattern.contains('/') && crate::pattern::matches_glob(pattern, &file_name))
    } else {
        file_name.contains(pattern) || rel_path.contains(pattern)
    };

    if matched {
        matched_entries.push(rel_path);
        if matched_entries.len() >= MAX_FIND_MATCHES {
            return true;
        }
    }
    false
}

/// 以固定目录 fd 做深度优先的流式遍历，逐个把条目交给 `visit`。
///
/// 每个层级只在遍历本层条目时持有一个 `fdopendir` 目录流；子目录句柄在真正下降时才
/// 打开、递归返回即释放；`visit` 拿到的条目在回调返回后立即释放。因此同时打开的目录 fd
/// 数量只与遍历深度相关，而不是与工作区内目录总数相关，遍历很多目录时不会耗尽 fd。
///
/// `visited` / `max_entries` / `max_depth` / `truncated` 语义与原递归实现一致：到达上限
/// 立即停止并标记截断，绝不先无界扫描再截断。`visit` 返回 `true` 时整个遍历提前结束。
fn walk_entries_pinned<F>(
    dir: &workspace_capability::Dir,
    depth: usize,
    ctx: &mut TraversalContext,
    visit: &mut F,
) -> Result<bool, String>
where
    F: FnMut(&workspace_capability::Child) -> Result<bool, String>,
{
    if depth > ctx.max_depth || ctx.visited >= ctx.max_entries {
        ctx.truncated = true;
        return Ok(false);
    }

    // 只保留子目录的 `Child`（与当前 `dir` 共享同一个父 fd），下降时再逐个 `open_dir`，
    // 避免同一层一次性打开大量子目录 fd。
    let mut subdirs = Vec::new();
    for child in dir.children()? {
        let child = child?;
        if ctx.visited >= ctx.max_entries {
            ctx.truncated = true;
            break;
        }
        ctx.visited += 1;

        match child.kind() {
            NodeKind::Symlink => continue,
            NodeKind::Dir => {
                if is_ignored_name(child.name()) {
                    continue;
                }
                if visit(&child)? {
                    return Ok(true);
                }
                subdirs.push(child);
            }
            NodeKind::File | NodeKind::Other => {
                if visit(&child)? {
                    return Ok(true);
                }
            }
        }
    }

    for child in subdirs {
        let subdir = child.open_dir()?;
        if walk_entries_pinned(&subdir, depth + 1, ctx, visit)? {
            return Ok(true);
        }
    }

    Ok(false)
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
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub expected_after_sha256: Option<String>,
    #[serde(default)]
    pub preview: Option<bool>,
    #[serde(default)]
    pub include_sha256: Option<bool>,
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
    #[serde(default)]
    pub byte_offset: Option<u64>,
    #[serde(default)]
    pub byte_limit: Option<u64>,
    #[serde(default)]
    pub context_lines: Option<u64>,
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

    /// 构造绑定完整原文件内容哈希的 edit 操作参数。
    pub fn edit_with_hash(
        path: impl Into<String>,
        old_text: impl Into<String>,
        new_text: impl Into<String>,
        expected_sha256: impl Into<String>,
    ) -> Self {
        let mut args = Self::edit(path, old_text, new_text);
        args.expected_sha256 = Some(expected_sha256.into());
        args
    }

    /// 将预览返回的前后整文件哈希同时绑定到正式 edit。
    pub fn edit_from_preview(
        path: impl Into<String>,
        old_text: impl Into<String>,
        new_text: impl Into<String>,
        before_sha256: impl Into<String>,
        after_sha256: impl Into<String>,
    ) -> Self {
        let mut args = Self::edit_with_hash(path, old_text, new_text, before_sha256);
        args.expected_after_sha256 = Some(after_sha256.into());
        args
    }

    /// 构造不写入磁盘的 edit 预览参数，仍使用 edit 的准入与授权。
    pub fn preview_edit(
        path: impl Into<String>,
        old_text: impl Into<String>,
        new_text: impl Into<String>,
    ) -> Self {
        let mut args = Self::edit(path, old_text, new_text);
        args.preview = Some(true);
        args
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
        "Workspace file tools and noninteractive shell (120s timeout). Paths must be relative; save outputs as e.g. mindmap.mmd, never /tmp/out.mmd. For grep/find/ls omit path or use '.' for root. For large files grep first (context_lines 0-20, default 0), then read relevant offset/limit ranges; truncated reads give the next offset.".into()
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
                    "description": "Workspace-relative file or directory path; absolute paths are rejected. For grep/find/ls omit it or use \".\" for the workspace root"
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
                "expected_sha256": {
                    "type": "string",
                    "pattern": "^[0-9a-fA-F]{64}$",
                    "description": "Optional SHA-256 of the entire original file bytes; edit refuses stale contents before writing"
                },
                "expected_after_sha256": {
                    "type": "string",
                    "pattern": "^[0-9a-fA-F]{64}$",
                    "description": "edit: with expected_sha256, require the output to match preview.after_sha256"
                },
                "preview": {
                    "type": "boolean",
                    "description": "edit: return a bounded structured diff and before/after hashes without writing; still requires edit approval"
                },
                "include_sha256": {
                    "type": "boolean",
                    "description": "read: include SHA-256 of the entire file bytes; byte pages include a sha256 JSON field"
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
                },
                "byte_offset": {"type":"integer","minimum":0,"description":"0-based UTF-8 byte offset; use for long single-line tool output; cannot combine with line paging"},
                "byte_limit": {"type":"integer","minimum":4,"maximum":65536,"description":"Maximum bytes for byte paging; result includes next_byte_offset"},
                "context_lines": {"type":"integer","minimum":0,"maximum":20,"description":"grep: neighboring lines to include around each match (0-20, default 0), so surrounding context needs no separate read"}
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
    #[serde(default)]
    pub byte_offset: Option<u64>,
    #[serde(default)]
    pub byte_limit: Option<u64>,
    #[serde(default)]
    pub include_sha256: Option<bool>,
}

#[derive(Clone)]
pub struct ReadTool(pub Workspace);
impl PortableTool for ReadTool {
    const NAME: &'static str = "read";
    type Args = ReadArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Read a workspace-relative UTF-8 file. For large files grep first, then read relevant 1-based offset/limit ranges. Follow Next offset on truncation. Optional include_sha256 reports the hash of the entire file for a later expected_sha256 edit. Long single-line files support byte_offset/byte_limit and next_byte_offset (JSON).".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path; absolute paths are rejected"},
                "offset": {"type": "integer", "minimum": 1, "description": "1-based starting line number"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to read"},
                "byte_offset": {"type":"integer","minimum":0,"description":"0-based UTF-8 byte offset; cannot combine with line paging"},
                "byte_limit": {"type":"integer","minimum":4,"maximum":65536,"description":"Maximum bytes; result includes next_byte_offset"},
                "include_sha256": {"type":"boolean","description":"Include SHA-256 of the entire file bytes; byte pages include a sha256 JSON field"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: ReadArgs) -> Result<Value, Self::Error> {
        PortableTool::call(
            &self.0,
            Arguments {
                operation: "read".into(),
                path: Some(args.path),
                offset: args.offset,
                limit: args.limit,
                byte_offset: args.byte_offset,
                byte_limit: args.byte_limit,
                include_sha256: args.include_sha256,
                ..Arguments::default()
            },
        )
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
        "Atomically write UTF-8 (max 1 MiB, parent must exist). Use a workspace-relative filename such as mindmap.mmd; absolute paths including /tmp/out.mmd are rejected.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path; absolute paths are rejected"},
                "content": {"type": "string", "description": "File content to write"},
                "value": {"type": "string", "description": "Alias for content"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: WriteArgs) -> Result<Value, Self::Error> {
        PortableTool::call(
            &self.0,
            Arguments {
                operation: "write".into(),
                path: Some(args.path),
                content: args.content,
                value: args.value,
                ..Arguments::default()
            },
        )
        .await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub expected_after_sha256: Option<String>,
    #[serde(default)]
    pub preview: Option<bool>,
}

#[derive(Clone)]
pub struct EditTool(pub Workspace);
impl PortableTool for EditTool {
    const NAME: &'static str = "edit";
    type Args = EditArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Edit a file by replacing an exact, uniquely matching old_text with new_text. Optional preview returns a bounded structured diff and hashes without writing; it still requires edit approval. Bind a later edit to the preview with expected_sha256 and expected_after_sha256. `path` must be a workspace-relative file path; absolute paths are rejected.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path; absolute paths are rejected"},
                "old_text": {"type": "string", "description": "Exact existing text to replace (must match uniquely)"},
                "new_text": {"type": "string", "description": "Replacement text"},
                "expected_sha256": {"type": "string", "pattern": "^[0-9a-fA-F]{64}$", "description": "Optional SHA-256 of the entire original file bytes; stale content is rejected"},
                "expected_after_sha256": {"type": "string", "pattern": "^[0-9a-fA-F]{64}$", "description": "With expected_sha256, require output to match preview.after_sha256"},
                "preview": {"type": "boolean", "description": "Return bounded structured diff and before/after hashes without writing; still requires edit approval"}
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: EditArgs) -> Result<Value, Self::Error> {
        let mut edit = Arguments::edit(args.path, args.old_text, args.new_text);
        edit.expected_sha256 = args.expected_sha256;
        edit.expected_after_sha256 = args.expected_after_sha256;
        edit.preview = args.preview;
        PortableTool::call(&self.0, edit).await
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
        PortableTool::call(
            &self.0,
            Arguments {
                operation: "shell".into(),
                command: args.command,
                value: args.value,
                ..Arguments::default()
            },
        )
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
    #[serde(default)]
    pub context_lines: Option<u64>,
}

#[derive(Clone)]
pub struct GrepTool(pub Workspace);
impl PortableTool for GrepTool {
    const NAME: &'static str = "grep";
    type Args = GrepArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Search file contents by literal or regex. Use relative path; omit it or use '.' for workspace root. Results have relative paths. context_lines (0-20, default 0) includes neighboring lines without another read.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (literal or regex)"},
                "path": {"type": "string", "description": "Optional workspace-relative file or directory path; omit it or pass \".\" for the whole workspace root"},
                "regex": {"type": "boolean", "description": "Treat pattern as a regular expression"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search"},
                "context_lines": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Neighboring lines to include around each match (0-20, default 0), so surrounding context needs no separate read"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: GrepArgs) -> Result<Value, Self::Error> {
        PortableTool::call(
            &self.0,
            Arguments {
                operation: "grep".into(),
                pattern: Some(args.pattern),
                path: args.path,
                regex: args.regex,
                case_insensitive: args.case_insensitive,
                context_lines: args.context_lines,
                ..Arguments::default()
            },
        )
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
        "Find workspace files/directories by glob (e.g. '**/*.rs') or substring. Paths are relative; omit path or use '.' for root.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern or substring to match"},
                "path": {"type": "string", "description": "Optional workspace-relative starting directory; omit it or pass \".\" to search from the workspace root"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn call(&self, args: FindArgs) -> Result<Value, Self::Error> {
        PortableTool::call(&self.0, Arguments::find(args.pattern, args.path)).await
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
        "List a workspace-relative directory (sorted, bounded). Omit path or use '.' for root. Absolute paths, '..' and symlinks are rejected.".into()
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Optional workspace-relative directory path; omit it or pass \".\" to list the workspace root"}
            },
            "additionalProperties": false
        })
    }
    async fn call(&self, args: LsArgs) -> Result<Value, Self::Error> {
        PortableTool::call(&self.0, Arguments::ls(args.path)).await
    }
}

/// 底部工具适配器：把内置 `PortableTool` 的失败归一化为引擎级错误。
///
/// 内置工具用 `Error = Infallible`，因此把真实失败包装成 `Ok({"error": ...})` JSON。
/// 如果这层不归一化，引擎会把失败当成成功（假 `Executed`）。本适配器只对**内置**
/// 工具生效：它把顶层 `error` 字符串视为该内置工具自己的错误通道，转成
/// `ToolExecutionError`，并用 `with_model_output` 保留原 JSON，使模型看到完全相同的
/// 反馈。它不解析、也不猜测插件/MCP/自定义工具 JSON 的业务含义，也不改动
/// `Workspace::execute` 的原始接口。
///
/// 适用范围：只有「顶层 `error` 字符串就是唯一失败通道」的无状态内置工具可以用本
/// 适配器。若某个工具的成功结果里也可能合法地带顶层 `error`（例如 `subagent` 的
/// `status` / `snapshot` / `kill` 返回的子任务业务结果），必须改用专用 typed 适配器
/// （[`crate::subagent::SubagentToolAdapter`]），否则会把业务结果误判为本次调用失败。
#[derive(Clone)]
pub struct BuiltinTool<T>(pub T);

impl<T> BuiltinTool<T> {
    /// 被包装的内置工具。
    pub fn inner(&self) -> &T {
        &self.0
    }
}

/// 内置工具失败 JSON 的判据：顶层是对象且带字符串 `error`。
fn builtin_error_message(value: &Value) -> Option<String> {
    value
        .as_object()
        .and_then(|object| object.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl<T> Tool for BuiltinTool<T>
where
    T: PortableTool<Output = Value, Error = std::convert::Infallible>,
{
    const NAME: &'static str = T::NAME;
    type Args = T::Args;
    type Output = ToolOutput;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        PortableTool::description(&self.0)
    }

    fn parameters(&self) -> Value {
        PortableTool::parameters(&self.0)
    }

    fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> impl Future<Output = Result<ToolOutput, ToolExecutionError>> + WasmCompatSend {
        let inner = &self.0;
        async move {
            match PortableTool::call(inner, args).await {
                Ok(value) => match builtin_error_message(&value) {
                    Some(message) => Err(ToolExecutionError::other(message)
                        .with_model_output(ToolOutput::json(value))),
                    None => Ok(ToolOutput::json(value)),
                },
                // `Error = Infallible`: a genuine typed failure is impossible.
                Err(never) => match never {},
            }
        }
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::{Config, HostFuture, Message, RunOptions, subagent};

    struct Offline;
    impl Host for Offline {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(async { Err("not used".into()) })
        }
        fn on_event(&self, _: AgentEvent) {}
    }
    impl subagent::Executor for Offline {
        fn execute(&self, task: subagent::Task) -> HostFuture<Result<Vec<Message>, String>> {
            Box::pin(async move { Ok(task.history) })
        }
    }

    #[test]
    fn edit_conflict_detection_preserves_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let path = cwd.join("source.rs");
        std::fs::write(&path, "original").unwrap();
        let original_metadata = std::fs::metadata(&path).unwrap();
        let root = workspace_capability::Root::open(&cwd).unwrap();
        let target = root.target("source.rs").unwrap();
        assert!(ensure_edit_source_unchanged(&target, &original_metadata, "original").is_ok());
        // Same-length changes must not pass a size-only check.
        std::fs::write(&path, "external").unwrap();
        assert!(ensure_edit_source_unchanged(&target, &original_metadata, "original").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
        std::fs::remove_file(&path).unwrap();
        assert!(ensure_edit_source_unchanged(&target, &original_metadata, "original").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn edit_conflict_detection_rejects_replaced_identity_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let path = cwd.join("source.rs");
        std::fs::write(&path, "original").unwrap();
        let original_metadata = std::fs::metadata(&path).unwrap();
        let root = workspace_capability::Root::open(&cwd).unwrap();
        let target = root.target("source.rs").unwrap();
        let replacement = tempfile::NamedTempFile::new_in(&cwd).unwrap();
        std::fs::write(replacement.path(), "original").unwrap();
        replacement.persist(&path).unwrap();
        assert!(ensure_edit_source_unchanged(&target, &original_metadata, "original").is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(ensure_edit_source_unchanged(&target, &original_metadata, "original").is_err());
    }

    #[tokio::test]
    async fn cancellation_keeps_workspace_reserved_until_blocking_worker_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            subagent::SubagentManager::with_executor(
                dir.path(),
                Config {
                    model: "offline".into(),
                    ..Config::default()
                },
                RunOptions::default(),
                4,
                Duration::from_secs(10),
                Arc::new(Offline),
            )
            .unwrap(),
        );
        let access = Arc::new(tokio::sync::RwLock::new(()));
        let shared_dir = tempfile::tempdir().unwrap();
        let shared =
            crate::shared_limits::SharedLimits::new(shared_dir.path(), Default::default()).unwrap();
        let lease = WorkspaceLease {
            _parent: Some(manager.parent_operation(true).unwrap()),
            _shared: Some(shared.acquire_workspace(dir.path(), true).await.unwrap()),
            _global: Some(
                workspace_admission()
                    .acquire_task(crate::coordinator::TaskRequest::write(workspace_scope(
                        &dir.path().canonicalize().unwrap(),
                    )))
                    .await
                    .unwrap(),
            ),
            _access: WorkspaceAccess::Write {
                _guard: access.clone().write_owned().await,
            },
            _output: None,
            _worker: None,
        };
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            spawn_leased(lease, move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            })
            .await
            .unwrap();
        });
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let request = subagent::Delegate {
            session: "probe".into(),
            goal: "offline probe".into(),
            prompt: "check".into(),
            path: None,
            read_only: true,
        };
        let queued = manager
            .delegate(request.clone(), Arc::new(Offline))
            .unwrap();
        assert_eq!(queued.status, subagent::Status::Queued);
        let write_still_reserved = access.clone().try_write_owned().is_err();
        let mut nested_reader = Box::pin(workspace_admission().acquire_task(
            crate::coordinator::TaskRequest::read(workspace_scope(&dir.path().join("nested"))),
        ));
        assert!(
            futures_util::poll!(&mut nested_reader).is_pending(),
            "an overlapping Workspace must wait for the cancelled worker"
        );
        let other_process_handle =
            crate::shared_limits::SharedLimits::new(shared_dir.path(), Default::default()).unwrap();
        let mut shared_reader = Box::pin(other_process_handle.acquire_workspace(dir.path(), false));
        assert!(
            futures_util::poll!(&mut shared_reader).is_pending(),
            "shared liveness lease must outlive cancellation too"
        );
        release_tx.send(()).unwrap();
        assert!(write_still_reserved);
        let nested_lease = tokio::time::timeout(Duration::from_secs(5), nested_reader)
            .await
            .unwrap()
            .unwrap();
        drop(nested_lease);
        let shared_lease = tokio::time::timeout(Duration::from_secs(5), shared_reader)
            .await
            .unwrap()
            .unwrap();
        drop(shared_lease);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = manager
                    .status(queued.run_id, Duration::from_secs(1))
                    .await
                    .unwrap();
                if !status.status.is_active() {
                    assert_eq!(status.status, subagent::Status::Completed);
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    fn checkpoint_store(cwd: &Path) -> Arc<crate::checkpoint::CheckpointStore> {
        Arc::new(crate::checkpoint::CheckpointStore::open(cwd).unwrap())
    }

    #[tokio::test]
    async fn checkpoints_are_off_by_default_and_leave_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let tools = Workspace::new(cwd.clone(), Arc::new(Offline)).unwrap();
        assert!(tools.checkpoints().is_none());

        tools
            .execute(Arguments::write("a.txt", "one"))
            .await
            .unwrap();
        tools
            .execute(Arguments::edit("a.txt", "one", "two"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "two");
        assert!(
            !cwd.join(".mio").exists(),
            "a default run must not create checkpoint files"
        );
    }

    #[tokio::test]
    async fn write_and_edit_snapshot_then_rewind_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = checkpoint_store(&cwd);
        let tools = Workspace::new(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store.clone());
        assert!(tools.checkpoints().is_some());

        tools
            .execute(Arguments::write("a.txt", "one"))
            .await
            .unwrap();
        tools
            .execute(Arguments::edit("a.txt", "one", "two"))
            .await
            .unwrap();
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].operation.as_str(), entries[0].existed),
            ("write", false)
        );
        assert!(entries[0].before_len.is_none());
        assert_eq!(
            (entries[1].operation.as_str(), entries[1].existed),
            ("edit", true)
        );
        assert_eq!(
            entries[1].before_sha256.as_deref(),
            Some(crate::checkpoint::content_fingerprint(b"one").as_str())
        );
        assert_eq!(
            entries[1].after_sha256,
            crate::checkpoint::content_fingerprint(b"two")
        );

        // 用户后续手工修改必须被保护：rewind 拒绝覆盖。
        std::fs::write(cwd.join("a.txt"), "manual").unwrap();
        let error = store.rewind(&entries[1].id).unwrap_err();
        assert!(error.contains("拒绝覆盖"), "{error}");
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "manual"
        );

        // 内容回到检查点记录的操作后状态后，edit 可以回退，write 创建的文件可以删除。
        std::fs::write(cwd.join("a.txt"), "two").unwrap();
        let outcome = store.rewind(&entries[1].id).unwrap();
        assert_eq!(outcome.action, crate::checkpoint::RewindAction::Restored);
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "one");
        let outcome = store.rewind(&entries[0].id).unwrap();
        assert_eq!(outcome.action, crate::checkpoint::RewindAction::Deleted);
        assert!(!cwd.join("a.txt").exists());
    }

    #[tokio::test]
    async fn failed_snapshot_blocks_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let checkpoint_dir = cwd.join(crate::checkpoint::CHECKPOINT_DIR);
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("index.json"), "{ broken").unwrap();
        let store = checkpoint_store(&cwd);
        let tools = Workspace::new(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store.clone());

        let error = tools
            .execute(Arguments::write("a.txt", "content"))
            .await
            .unwrap_err();
        assert!(error.contains("检查点"), "{error}");
        assert!(
            !cwd.join("a.txt").exists(),
            "a failed snapshot must block the write"
        );
        assert!(store.list().is_err());
        assert_eq!(
            std::fs::read_to_string(checkpoint_dir.join("index.json")).unwrap(),
            "{ broken"
        );
    }

    #[tokio::test]
    async fn oversized_existing_file_is_not_overwritten_when_checkpoints_are_on() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = checkpoint_store(&cwd);
        let tools = Workspace::new(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store.clone());
        let original = vec![b'x'; MAX_WRITE_BYTES + 1];
        std::fs::write(cwd.join("big.bin"), &original).unwrap();

        let error = tools
            .execute(Arguments::write("big.bin", "small"))
            .await
            .unwrap_err();
        assert!(error.contains("检查点快照上限"), "{error}");
        assert_eq!(std::fs::read(cwd.join("big.bin")).unwrap(), original);
        assert!(store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn checkpoints_reject_escapes_and_symlinked_targets() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = checkpoint_store(&cwd);
        let tools = Workspace::new(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store.clone());

        assert!(
            tools
                .execute(Arguments::write("../escape.txt", "no"))
                .await
                .is_err()
        );
        assert!(store.list().unwrap().is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", cwd.join("link.txt")).unwrap();
            assert!(
                tools
                    .execute(Arguments::write("link.txt", "no"))
                    .await
                    .is_err()
            );
            assert!(store.list().unwrap().is_empty());
        }
    }

    #[derive(Default)]
    struct ApprovalHost {
        approvals: std::sync::Mutex<Vec<(String, String)>>,
        deny: bool,
    }

    impl Host for ApprovalHost {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(async { Err("not used".into()) })
        }
        fn on_event(&self, _: AgentEvent) {}
        fn approve_tool(&self, operation: &str, target: &str) -> Result<(), String> {
            if self.deny {
                return Err(format!("approval denied for {operation} {target}"));
            }
            self.approvals
                .lock()
                .unwrap()
                .push((operation.to_owned(), target.to_owned()));
            Ok(())
        }
    }

    async fn two_step_checkpoint(store: &Arc<crate::checkpoint::CheckpointStore>) -> String {
        let host = Arc::new(ApprovalHost::default());
        let tools = Workspace::new(store.root().unwrap(), host)
            .unwrap()
            .with_checkpoints(store.clone());
        tools
            .execute(Arguments::write("a.txt", "before"))
            .await
            .unwrap();
        tools
            .execute(Arguments::edit("a.txt", "before", "after"))
            .await
            .unwrap();
        store.list().unwrap()[1].id.clone()
    }

    #[tokio::test]
    async fn rewind_checkpoint_requires_enabled_store_and_writable_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let plain = Workspace::new(cwd.clone(), Arc::new(Offline)).unwrap();
        assert!(plain.rewind_checkpoint("abcd").await.is_err());

        let store = checkpoint_store(&cwd);
        let read_only = Workspace::new_read_only(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store);
        let error = read_only.rewind_checkpoint("abcd").await.unwrap_err();
        assert!(error.contains("只读"), "{error}");
    }

    #[tokio::test]
    async fn rewind_checkpoint_uses_host_approval_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = checkpoint_store(&cwd);
        let host = Arc::new(ApprovalHost::default());
        let tools = Workspace::new(cwd.clone(), host.clone())
            .unwrap()
            .with_checkpoints(store.clone());
        tools
            .execute(Arguments::write("a.txt", "before"))
            .await
            .unwrap();
        tools
            .execute(Arguments::edit("a.txt", "before", "after"))
            .await
            .unwrap();
        let edit_id = store.list().unwrap()[1].id.clone();

        let outcome = tools.rewind_checkpoint(&edit_id).await.unwrap();
        assert_eq!(outcome.action, crate::checkpoint::RewindAction::Restored);
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "before"
        );
        assert_eq!(
            store.list().unwrap()[1].state,
            crate::checkpoint::CheckpointState::Rewound
        );
        let approvals = host.approvals.lock().unwrap();
        assert!(
            approvals
                .iter()
                .any(|(operation, target)| operation == "rewind" && target == &edit_id),
            "rewind must request host approval: {approvals:?}"
        );
    }

    #[tokio::test]
    async fn rewind_checkpoint_is_blocked_by_host_denial() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = checkpoint_store(&cwd);
        let edit_id = two_step_checkpoint(&store).await;

        let denying = Arc::new(ApprovalHost {
            deny: true,
            ..Default::default()
        });
        let tools = Workspace::new(cwd.clone(), denying)
            .unwrap()
            .with_checkpoints(store.clone());
        let error = tools.rewind_checkpoint(&edit_id).await.unwrap_err();
        assert!(error.contains("denied"), "{error}");
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "after");
        assert_eq!(
            store.list().unwrap()[1].state,
            crate::checkpoint::CheckpointState::Active
        );
    }

    #[tokio::test]
    async fn rewind_checkpoint_waits_for_cooperative_write_lease() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let other_cwd = other_dir.path().canonicalize().unwrap();
        let shared_dir = tempfile::tempdir().unwrap();
        let shared =
            crate::shared_limits::SharedLimits::new(shared_dir.path(), Default::default()).unwrap();

        let store = checkpoint_store(&cwd);
        let mut tools = Workspace::new(cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(store.clone());
        tools.shared_limits = Some(shared.clone());
        tools
            .execute(Arguments::write("a.txt", "before"))
            .await
            .unwrap();
        tools
            .execute(Arguments::edit("a.txt", "before", "after"))
            .await
            .unwrap();
        let edit_id = store.list().unwrap()[1].id.clone();

        // 另一合作进程（独立 SharedLimits 句柄）持有同一路径的写租约。
        let competitor =
            crate::shared_limits::SharedLimits::new(shared_dir.path(), Default::default()).unwrap();
        let held = competitor.acquire_workspace(&cwd, true).await.unwrap();

        let mut rewind = Box::pin(tools.rewind_checkpoint(&edit_id));
        assert!(
            futures_util::poll!(&mut rewind).is_pending(),
            "rewind must queue behind a cooperative write lease"
        );

        // 无关路径不受影响：另一个工作区的写立即完成。
        let mut other = Workspace::new(other_cwd.clone(), Arc::new(Offline))
            .unwrap()
            .with_checkpoints(checkpoint_store(&other_cwd));
        other.shared_limits = Some(shared.clone());
        tokio::time::timeout(
            Duration::from_secs(3),
            other.execute(Arguments::write("b.txt", "x")),
        )
        .await
        .expect("an unrelated workspace must not block behind the lease")
        .unwrap();

        drop(held);
        let outcome = tokio::time::timeout(Duration::from_secs(5), rewind)
            .await
            .expect("rewind must proceed after the lease is released")
            .unwrap();
        assert_eq!(outcome.action, crate::checkpoint::RewindAction::Restored);
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "before"
        );
        assert_eq!(
            store.list().unwrap()[1].state,
            crate::checkpoint::CheckpointState::Rewound
        );
    }
}
