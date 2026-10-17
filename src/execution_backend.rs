//! Pluggable OS execution sandbox backends.
//!
//! An [`ExecutionBackend`] turns an [`ExecutionRequest`] into a
//! [`SandboxCommand`]: the concrete program, argument vector, working
//! directory, environment and resource-limit policy for one command. A
//! supervisor (shell, plugin, MCP tool, subagent) applies that plan to the
//! `Command` it already owns, so stdio, timeouts, cancellation and
//! process-group cleanup stay with the supervisor while the isolation policy
//! stays here. The plan is plain data and can be serialized, compared or
//! reused across entry points.
//!
//! Two backends ship today:
//!
//! - [`UnrestrictedBackend`] runs the program directly with the caller's
//!   privileges. It exists so "no sandbox" is an explicit, queryable choice
//!   instead of an implicit default.
//! - [`BubblewrapBackend`] uses Linux [bubblewrap] namespaces. It is
//!   **fail-closed**: when the binary is missing, the platform is
//!   unsupported, or the requested policy cannot be honoured, [`wrap`]
//!   returns [`BackendError`] and never falls back to an unsandboxed command.
//!
//! [`ExecutionPolicy`] bundles a backend with its policy so a host can pick
//! `Unrestricted` or `Bubblewrap` and call [`ExecutionPolicy::resolve`]
//! without knowing about either implementation.
//!
//! ## What a read-only host root guarantees, and what it does not
//!
//! [`FilesystemPolicy::HostRootReadOnly`] mounts the host root read-only and
//! makes only the declared workspace writable. That bounds **writes**; it is
//! not a read boundary. Every host file the caller's user can read stays
//! readable inside the sandbox (for example `/etc/passwd`, an SSH key or a
//! `.env` file under the user's home), so this policy must not be described
//! as hiding host secrets.
//!
//! Full read confinement needs [`FilesystemPolicy::ExplicitMounts`]: the
//! sandbox starts from an empty root and the caller lists every runtime path
//! the program needs. [`linux_runtime_mounts`] returns the usual Linux
//! runtime paths (dynamic loader, `/usr`, `/lib`, ...). Callers must add
//! locale, TLS bundle, toolchain or data directories actually required, and
//! must not add the whole host root back. This backend rejects an explicit
//! policy whose read-only set contains `/`.
//!
//! ## Boundaries shared by both policies
//!
//! Namespaces share the host kernel: this is a mount/namespace boundary, not
//! a virtual machine and not a seccomp filter. [`BubblewrapPolicy::limits`]
//! adds per-process Linux `rlimit` ceilings that children inherit, while
//! memory pressure, kernel bugs or a host-level compromise stay outside the
//! model.
//!
//! ## Who is responsible for the policy
//!
//! The policy covers the child processes the engine itself starts: the `shell`
//! tool (through [`Host::shell_with_policy`]), declarative plugin tools, stdio
//! MCP servers and delegated subagents. When a run requests a
//! non-`Unrestricted` policy, the engine refuses arbitrary
//! `RunTools::extra_tools` before registration and accepts plugin/MCP tools
//! only through [`PolicyBoundTools`](crate::mcp::PolicyBoundTools), whose load-time policy fingerprint
//! is re-checked against the run. Two things are deliberately outside this
//! boundary and must not be described as sandboxed: unknown host tools and
//! host Rust callbacks, and the host's own Provider/model network client, which
//! runs in the host process rather than inside a sandboxed child.
//! [`BackendStatus::available`] is only a helper and platform check; use
//! [`ExecutionPolicy::probe`] when a host needs an actual observed result
//! before it claims the sandbox works.
//!
//! [`Host`]: crate::Host
//! [`Host::execution_policy`]: crate::Host::execution_policy
//! [`Host::shell_with_policy`]: crate::Host::shell_with_policy
//! [`Host::shell_with_policy`]: crate::Host::shell_with_policy
//! [`PolicyBoundTools`](crate::mcp::PolicyBoundTools): crate::mcp::PolicyBoundTools
//! [bubblewrap]: https://github.com/containers/bubblewrap
//! [`wrap`]: ExecutionBackend::wrap

use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Default bubblewrap binary on Linux distributions.
pub const BWRAP_PROGRAM: &str = "/usr/bin/bwrap";

/// Minimal `PATH` used by [`EnvironmentPolicy::SandboxDefaults`].
pub const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Host platform a backend was detected for.
///
/// Hosts normally use [`HostPlatform::current`]. Tests and hosts that ship a
/// bundled bubblewrap can state the platform explicitly instead of relying on
/// the build target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPlatform {
    /// Linux, where bubblewrap namespaces are available.
    Linux,
    /// Any platform without bubblewrap support (macOS, Windows, ...).
    Unsupported,
}

impl HostPlatform {
    /// The platform this build is running on.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unsupported
        }
    }

    /// Whether bubblewrap-style sandboxing can be attempted here.
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Linux)
    }
}

/// How much of the host filesystem a sandboxed process can read.
///
/// Read visibility is reported separately from write confinement because a
/// read-only host root still exposes everything the caller's user can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostReadVisibility {
    /// No isolation: the process sees the host filesystem normally.
    FullHostAccess,
    /// The host root is visible read-only. Bounds writes, not reads.
    HostRootReadOnly,
    /// Only the explicitly listed mounts are visible.
    ExplicitMountsOnly,
}

/// Isolation a backend can provide for its wrapped commands.
///
/// For a [`BackendStatus`] the flags describe the backend's capability. For a
/// [`SandboxCommand`] they describe what that specific plan requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsolationGuarantees {
    /// How much of the host filesystem stays readable.
    pub host_read_visibility: HostReadVisibility,
    /// Writable host paths are limited to the declared workspace.
    pub workspace_only_writes: bool,
    /// The command gets its own network namespace.
    pub network_unshared: bool,
    /// `/tmp` is a private tmpfs.
    pub private_tmp: bool,
    /// The command gets its own PID namespace.
    pub private_pid: bool,
    /// Sandboxed descendants are killed when the sandbox or supervisor exits.
    pub process_exit_cleanup: bool,
    /// CPU / address-space / file-descriptor rlimits are supported and are
    /// applied whenever the plan's [`ResourceLimits`] are set.
    pub resource_limits: bool,
}

impl IsolationGuarantees {
    /// No isolation at all, as reported by [`UnrestrictedBackend`].
    pub const NONE: Self = Self {
        host_read_visibility: HostReadVisibility::FullHostAccess,
        workspace_only_writes: false,
        network_unshared: false,
        private_tmp: false,
        private_pid: false,
        process_exit_cleanup: false,
        resource_limits: false,
    };

    /// `true` only when host reads are confined to an explicit allowlist.
    ///
    /// A read-only host root returns `false` on purpose: it bounds writes
    /// while every host file readable by the caller's user stays readable.
    pub fn confines_host_reads(&self) -> bool {
        matches!(
            self.host_read_visibility,
            HostReadVisibility::ExplicitMountsOnly
        )
    }
}

/// Resource ceilings installed for each sandboxed process.
///
/// These are POSIX `rlimit` values. They are installed in the sandbox monitor
/// between `fork` and `exec` and are inherited by its descendants, but each
/// process gets its own limit: `cpu_seconds` bounds one process's CPU time and
/// `address_space_bytes` bounds one process's address space. They are **not**
/// an aggregate budget for the whole process tree, so forked workers each get
/// the full ceiling. Unset fields keep the host default, and a process can
/// lower its own soft limit but cannot raise the hard one installed here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Per-process `RLIMIT_CPU` soft and hard limit, in CPU seconds.
    pub cpu_seconds: Option<u64>,
    /// Per-process `RLIMIT_AS` soft and hard limit, in bytes.
    pub address_space_bytes: Option<u64>,
    /// Per-process `RLIMIT_NOFILE` soft and hard limit, in file descriptors.
    pub open_files: Option<u64>,
}

impl ResourceLimits {
    /// Whether every field is unset (the host default).
    pub fn is_unset(&self) -> bool {
        self.cpu_seconds.is_none()
            && self.address_space_bytes.is_none()
            && self.open_files.is_none()
    }

    fn validate(&self) -> Result<(), BackendError> {
        if self.cpu_seconds == Some(0) {
            return Err(BackendError::InvalidRequest(
                "cpu_seconds must be at least 1".into(),
            ));
        }
        if self.address_space_bytes == Some(0) {
            return Err(BackendError::InvalidRequest(
                "address_space_bytes must be at least 1".into(),
            ));
        }
        if let Some(open_files) = self.open_files
            && open_files < 3
        {
            return Err(BackendError::InvalidRequest(
                "open_files must leave room for stdin, stdout and stderr".into(),
            ));
        }
        Ok(())
    }

    /// Apply these limits to the calling process (and its descendants).
    ///
    /// Safe to call from a `Command::pre_exec` hook: it only invokes
    /// `setrlimit`. Returns [`io::ErrorKind::Unsupported`] on non-Unix hosts.
    #[cfg(unix)]
    pub fn apply_to_process(&self) -> io::Result<()> {
        if let Some(seconds) = self.cpu_seconds {
            let limit = libc::rlimit {
                rlim_cur: seconds,
                rlim_max: seconds,
            };
            // SAFETY: RLIMIT_CPU is a valid resource and `limit` is fully
            // initialized; setrlimit only reads those two arguments.
            if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if let Some(bytes) = self.address_space_bytes {
            let limit = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            // SAFETY: see above; RLIMIT_AS is a valid resource.
            if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if let Some(files) = self.open_files {
            let limit = libc::rlimit {
                rlim_cur: files,
                rlim_max: files,
            };
            // SAFETY: see above; RLIMIT_NOFILE is a valid resource.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Non-Unix hosts cannot install POSIX rlimits.
    #[cfg(not(unix))]
    pub fn apply_to_process(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "resource limits require a Unix host",
        ))
    }
}

/// Filesystem layout requested for a sandboxed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilesystemPolicy {
    /// Mount the host root read-only and make only the workspace writable.
    ///
    /// This bounds writes only. Any host file the caller's user can read
    /// stays readable, so this is not a secret boundary.
    HostRootReadOnly,
    /// Hide the host root and mount only these paths read-only, plus the
    /// workspace read-write. No host runtime path is mounted implicitly, so
    /// callers must list the ones the program needs; see
    /// [`linux_runtime_mounts`].
    ExplicitMounts {
        /// Host paths to expose read-only.
        read_only: Vec<PathBuf>,
    },
}

/// Environment handed to the sandboxed command.
///
/// A sandbox that inherits the supervisor environment also inherits its
/// secrets, so the bubblewrap backend defaults to
/// [`EnvironmentPolicy::SandboxDefaults`] and hosts must opt in explicitly to
/// any variable they want to pass through.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvironmentPolicy {
    /// Inherit the supervisor environment unchanged.
    Inherit,
    /// Start empty and set only these variables.
    Explicit(Vec<(String, String)>),
    /// Start empty and set `PATH`, `HOME` (the workspace), `TMPDIR=/tmp` and
    /// `LANG=C.UTF-8`.
    SandboxDefaults,
}

impl fmt::Debug for EnvironmentPolicy {
    /// Redacts values: an environment can hold credentials and plans end up
    /// in logs and error messages.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inherit => formatter.write_str("Inherit"),
            Self::SandboxDefaults => formatter.write_str("SandboxDefaults"),
            Self::Explicit(variables) => {
                let names: Vec<&str> = variables.iter().map(|(name, _)| name.as_str()).collect();
                formatter.debug_tuple("Explicit").field(&names).finish()
            }
        }
    }
}

/// Policy for the Linux bubblewrap backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BubblewrapPolicy {
    /// Filesystem layout; see [`FilesystemPolicy`].
    pub filesystem: FilesystemPolicy,
    /// CPU / address-space / file-descriptor ceilings.
    pub limits: ResourceLimits,
    /// Environment handed to the sandboxed command.
    pub environment: EnvironmentPolicy,
    /// Mount a private tmpfs on `/tmp`.
    pub private_tmp: bool,
    /// Unshare the PID namespace.
    pub private_pid: bool,
    /// Unshare the network namespace (no connectivity except loopback).
    pub disable_network: bool,
    /// Kill sandboxed processes when the sandbox or its parent dies.
    pub die_with_parent: bool,
    /// Run the sandboxed command in a new session.
    pub new_session: bool,
    /// Unshare the System V IPC namespace.
    pub unshare_ipc: bool,
    /// Unshare the UTS namespace (private hostname).
    pub unshare_uts: bool,
    /// Sandbox helper binary override.
    ///
    /// `None` (the default) uses the distribution path [`BWRAP_PROGRAM`].
    /// Hosts that ship their own helper, and tests that need to prove
    /// fail-closed behavior, can point this at another binary; a path that is
    /// missing or not executable makes the policy unavailable instead of
    /// falling back to an unsandboxed command.
    pub helper_program: Option<PathBuf>,
}

impl Default for BubblewrapPolicy {
    fn default() -> Self {
        Self {
            filesystem: FilesystemPolicy::HostRootReadOnly,
            limits: ResourceLimits::default(),
            environment: EnvironmentPolicy::SandboxDefaults,
            private_tmp: true,
            private_pid: true,
            disable_network: true,
            die_with_parent: true,
            new_session: true,
            unshare_ipc: true,
            unshare_uts: true,
            helper_program: None,
        }
    }
}

impl BubblewrapPolicy {
    /// Replace the filesystem layout.
    pub fn with_filesystem(mut self, filesystem: FilesystemPolicy) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// Replace the environment policy.
    pub fn with_environment(mut self, environment: EnvironmentPolicy) -> Self {
        self.environment = environment;
        self
    }

    /// Replace the resource limits.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replace the sandbox helper binary.
    pub fn with_helper_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.helper_program = Some(program.into());
        self
    }

    fn guarantees(&self) -> IsolationGuarantees {
        IsolationGuarantees {
            host_read_visibility: match self.filesystem {
                FilesystemPolicy::HostRootReadOnly => HostReadVisibility::HostRootReadOnly,
                FilesystemPolicy::ExplicitMounts { .. } => HostReadVisibility::ExplicitMountsOnly,
            },
            workspace_only_writes: true,
            network_unshared: self.disable_network,
            private_tmp: self.private_tmp,
            private_pid: self.private_pid,
            process_exit_cleanup: self.die_with_parent,
            resource_limits: true,
        }
    }
}

/// One command to wrap: the program and arguments plus the workspace that is
/// allowed to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRequest {
    /// Absolute path of the program, resolved inside the sandbox.
    pub program: PathBuf,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Directory that is writable and used as the working directory. Also
    /// host-mounted read-write by the bubblewrap backend.
    pub workspace: PathBuf,
}

impl ExecutionRequest {
    /// Start a request for `program` with `workspace` as the writable root.
    pub fn new(program: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            workspace: workspace.into(),
        }
    }

    /// Append one argument.
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }
}

/// The concrete command a supervisor should run.
///
/// Stdio is intentionally not configured here: the supervisor keeps ownership
/// of pipes, redirection, timeouts and process-group teardown.
#[derive(Clone)]
pub struct SandboxCommand {
    backend: &'static str,
    program: PathBuf,
    args: Vec<OsString>,
    working_directory: PathBuf,
    environment: EnvironmentPolicy,
    limits: ResourceLimits,
    guarantees: IsolationGuarantees,
}

impl fmt::Debug for SandboxCommand {
    /// Redacts environment values; see [`EnvironmentPolicy`].
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxCommand")
            .field("backend", &self.backend)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("working_directory", &self.working_directory)
            .field("environment", &self.environment)
            .field("limits", &self.limits)
            .field("guarantees", &self.guarantees)
            .finish()
    }
}

impl SandboxCommand {
    fn new(
        backend: &'static str,
        program: PathBuf,
        args: Vec<OsString>,
        working_directory: PathBuf,
        environment: EnvironmentPolicy,
        limits: ResourceLimits,
        guarantees: IsolationGuarantees,
    ) -> Self {
        Self {
            backend,
            program,
            args,
            working_directory,
            environment,
            limits,
            guarantees,
        }
    }

    /// Name of the backend that produced this plan.
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// Program to execute (bubblewrap itself for the sandboxed backend).
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Arguments for [`Self::program`].
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Working directory for the supervisor process.
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// Environment policy for the supervisor process.
    pub fn environment(&self) -> &EnvironmentPolicy {
        &self.environment
    }

    /// Resource limits applied to the child before `exec`.
    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    /// Isolation this plan requests.
    pub fn guarantees(&self) -> IsolationGuarantees {
        self.guarantees
    }

    /// Overwrite `command` with this plan.
    ///
    /// This **replaces the whole command**: program, arguments, working
    /// directory and environment are rewritten, and any stdio, process group,
    /// `pre_exec` hook or kill-on-drop setting the caller configured on
    /// `command` before this call is discarded. The rlimit-aware `pre_exec`
    /// hook from [`Self::limits`] is installed as part of the replacement.
    ///
    /// Callers must therefore set stdin/stdout/stderr, a process group and any
    /// other spawn configuration **after** `apply`.
    pub fn apply(&self, command: &mut Command) -> io::Result<()> {
        let mut fresh = Command::new(&self.program);
        fresh.args(&self.args);
        fresh.current_dir(&self.working_directory);
        if let Some(variables) = environment_variables(&self.environment, &self.working_directory) {
            fresh.env_clear();
            for (name, value) in variables {
                fresh.env(name, value);
            }
        }
        if !self.limits.is_unset() {
            apply_limits(&mut fresh, self.limits)?;
        }
        *command = fresh;
        Ok(())
    }

    /// Build a fresh std [`Command`] for this plan.
    pub fn to_std_command(&self) -> io::Result<Command> {
        let mut command = Command::new(&self.program);
        self.apply(&mut command)?;
        Ok(command)
    }
}

/// Effective variables for an environment policy.
///
/// `None` means "inherit the supervisor environment".
fn environment_variables(
    environment: &EnvironmentPolicy,
    workspace: &Path,
) -> Option<Vec<(String, String)>> {
    match environment {
        EnvironmentPolicy::Inherit => None,
        EnvironmentPolicy::Explicit(variables) => Some(variables.clone()),
        EnvironmentPolicy::SandboxDefaults => Some(vec![
            ("PATH".into(), SANDBOX_PATH.into()),
            ("HOME".into(), workspace.to_string_lossy().into_owned()),
            ("TMPDIR".into(), "/tmp".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ]),
    }
}

fn apply_limits(command: &mut Command, limits: ResourceLimits) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls `setrlimit` on `rlim_t` values it
        // owns, which is async-signal-safe and allocation free, as required
        // between fork and exec.
        unsafe {
            command.pre_exec(move || limits.apply_to_process());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (command, limits);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "resource limits require a Unix host",
        ))
    }
}

/// How far a backend's availability has actually been checked.
///
/// [`BackendStatus::available`] only says the helper binary and platform
/// requirements are met. That is not proof the kernel lets the sandbox start,
/// so it must never be reported as verified. [`BackendProbe`] keeps the two
/// claims apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendProbe {
    /// The helper and platform check passed, but no sandboxed command has been
    /// run here, so kernel operability is unknown.
    NotProbed,
    /// A bounded sandboxed probe command actually ran and exited successfully.
    Verified,
    /// A bounded probe was attempted and failed; the sandbox is not usable.
    /// The string is the observed reason.
    Failed(String),
}

/// Availability and isolation of one backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendStatus {
    /// Stable backend name.
    pub name: &'static str,
    /// Whether the cheap helper and platform check passes and
    /// [`ExecutionBackend::wrap`] can produce a plan.
    ///
    /// This inspects the helper binary and the host platform only. It does
    /// **not** prove that the kernel actually permits the sandbox; see
    /// [`BackendStatus::probe`] and [`ExecutionPolicy::probe`].
    pub available: bool,
    /// Why the backend is unavailable; always set when `available` is false,
    /// and `None` otherwise.
    pub unavailable_reason: Option<String>,
    /// Capability this backend provides.
    pub guarantees: IsolationGuarantees,
    /// What the guarantees do not cover.
    pub caveats: &'static [&'static str],
    /// Sandbox helper binary, when the backend has one.
    pub program: Option<PathBuf>,
    /// Result of an actual bounded probe; [`BackendProbe::NotProbed`] from a
    /// plain [`ExecutionBackend::status`] call.
    pub probe: BackendProbe,
}

/// Failure to produce a sandboxed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The backend cannot run here, so no command is produced. There is no
    /// fallback to an unsandboxed command.
    Unavailable {
        /// Backend name.
        backend: &'static str,
        /// Human-readable reason.
        reason: String,
    },
    /// The request contradicts the policy.
    InvalidRequest(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { backend, reason } => {
                write!(formatter, "{backend} backend is unavailable: {reason}")
            }
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid execution request: {reason}")
            }
        }
    }
}

impl std::error::Error for BackendError {}

/// A pluggable execution environment.
pub trait ExecutionBackend: Send + Sync + fmt::Debug {
    /// Stable backend name.
    fn name(&self) -> &'static str;

    /// Availability plus the isolation the backend offers.
    fn status(&self) -> BackendStatus;

    /// Turn a request into a command plan.
    ///
    /// Implementations must fail closed: when the policy cannot be honoured,
    /// return [`BackendError`] instead of a weaker command.
    fn wrap(&self, request: &ExecutionRequest) -> Result<SandboxCommand, BackendError>;
}

const UNRESTRICTED_CAVEATS: &[&str] = &[
    "No OS isolation: the program runs with the supervisor's user, filesystem, network and process privileges.",
    "Supervisor timeouts and process-group cleanup are cooperative, not an OS sandbox.",
    "Arbitrary RunTools::extra_tools are accepted under this policy; requesting isolation instead refuses them and accepts only engine-loaded policy-bound plugin and MCP tools.",
];

/// Explicit "no sandbox" backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnrestrictedBackend;

impl ExecutionBackend for UnrestrictedBackend {
    fn name(&self) -> &'static str {
        "unrestricted"
    }

    fn status(&self) -> BackendStatus {
        BackendStatus {
            name: self.name(),
            available: true,
            unavailable_reason: None,
            guarantees: IsolationGuarantees::NONE,
            caveats: UNRESTRICTED_CAVEATS,
            program: None,
            probe: BackendProbe::NotProbed,
        }
    }

    fn wrap(&self, request: &ExecutionRequest) -> Result<SandboxCommand, BackendError> {
        let workspace = canonical_workspace(&request.workspace)?;
        Ok(SandboxCommand::new(
            self.name(),
            request.program.clone(),
            request.args.iter().map(OsString::from).collect(),
            workspace,
            EnvironmentPolicy::Inherit,
            ResourceLimits::default(),
            IsolationGuarantees::NONE,
        ))
    }
}

const BUBBLEWRAP_CAVEATS: &[&str] = &[
    "A read-only host root bounds writes, not reads: every host file the caller's user can read stays readable. Use FilesystemPolicy::ExplicitMounts for read confinement.",
    "Namespaces share the host kernel; this is not a virtual machine and not a seccomp filter.",
    "Writable host paths are limited to the declared workspace; this backend adds no other writable mount.",
    "Applies to engine-started child processes only: the shell tool, declarative plugin tools, stdio MCP servers and delegated subagents.",
    "Arbitrary RunTools::extra_tools are refused before registration when a non-Unrestricted policy is requested; only engine-loaded policy-bound plugin and MCP tools are accepted.",
    "The host's own Provider/model network client runs in the host process, outside this child-process sandbox.",
];

/// Linux bubblewrap backend.
#[derive(Clone, Debug)]
pub struct BubblewrapBackend {
    program: PathBuf,
    platform: HostPlatform,
    policy: BubblewrapPolicy,
}

impl BubblewrapBackend {
    /// Detect bubblewrap on the current platform.
    pub fn detect() -> Self {
        Self::at(BWRAP_PROGRAM, HostPlatform::current())
    }

    /// Use `program` while asserting the platform explicitly.
    pub fn at(program: impl Into<PathBuf>, platform: HostPlatform) -> Self {
        Self {
            program: program.into(),
            platform,
            policy: BubblewrapPolicy::default(),
        }
    }

    /// Replace the sandbox policy.
    pub fn with_policy(mut self, policy: BubblewrapPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sandbox helper binary this backend will execute.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Platform this backend was configured for.
    pub fn platform(&self) -> HostPlatform {
        self.platform
    }

    /// Configured policy.
    pub fn policy(&self) -> &BubblewrapPolicy {
        &self.policy
    }

    fn program_problem(&self) -> Option<String> {
        if !self.platform.is_supported() {
            return Some(format!(
                "bubblewrap needs Linux namespaces; this backend was configured for a host other than Linux ({})",
                std::env::consts::OS
            ));
        }
        match std::fs::metadata(&self.program) {
            Ok(metadata) if metadata.is_dir() => Some(format!(
                "bubblewrap program {} is a directory",
                self.program.display()
            )),
            Ok(metadata) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o111 == 0 {
                        return Some(format!(
                            "bubblewrap program {} is not executable",
                            self.program.display()
                        ));
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = metadata;
                }
                None
            }
            Err(error) => Some(format!(
                "bubblewrap program {} is unavailable: {error}",
                self.program.display()
            )),
        }
    }

    fn wrap_args(
        &self,
        request: &ExecutionRequest,
        workspace: &Path,
        program: &Path,
    ) -> Result<Vec<OsString>, BackendError> {
        let policy = &self.policy;
        let mut args: Vec<OsString> = Vec::new();
        if policy.die_with_parent {
            args.push("--die-with-parent".into());
        }
        if policy.new_session {
            args.push("--new-session".into());
        }
        match &policy.filesystem {
            FilesystemPolicy::HostRootReadOnly => {
                args.push("--ro-bind".into());
                args.push("/".into());
                args.push("/".into());
            }
            FilesystemPolicy::ExplicitMounts { read_only } => {
                if read_only.is_empty() {
                    return Err(BackendError::InvalidRequest(
                        "FilesystemPolicy::ExplicitMounts must list the runtime paths the program needs; no host runtime path is mounted implicitly".into(),
                    ));
                }
                for mount in read_only {
                    let source = canonical_mount(mount)?;
                    if source == Path::new("/") {
                        return Err(BackendError::InvalidRequest(
                            "FilesystemPolicy::ExplicitMounts must not include the host root; use FilesystemPolicy::HostRootReadOnly to declare that read visibility explicitly".into(),
                        ));
                    }
                    if policy.private_tmp
                        && (source.starts_with("/tmp") || mount.starts_with("/tmp"))
                    {
                        return Err(BackendError::InvalidRequest(format!(
                            "read-only mount {} is under /tmp, which this policy replaces with a private tmpfs",
                            mount.display()
                        )));
                    }
                    // Keep the caller's spelling as both source and destination:
                    // canonicalizing the destination would drop loader aliases such
                    // as /bin or /lib64 and break programs that reference them.
                    push_ro_bind(&mut args, mount.as_os_str());
                    // Also expose the resolved target. A trusted runtime path is
                    // often a symlink (/bin -> usr/bin), and a program may
                    // reference either the alias or the real location.
                    if source.as_path() != mount.as_path() {
                        push_ro_bind(&mut args, source.as_os_str());
                    }
                }
            }
        }
        if policy.private_tmp {
            args.push("--tmpfs".into());
            args.push("/tmp".into());
        }
        args.push("--proc".into());
        args.push("/proc".into());
        args.push("--dev".into());
        args.push("/dev".into());
        args.push("--bind".into());
        args.push(workspace.as_os_str().to_os_string());
        args.push(workspace.as_os_str().to_os_string());
        if policy.disable_network {
            args.push("--unshare-net".into());
        }
        if policy.private_pid {
            args.push("--unshare-pid".into());
        }
        if policy.unshare_ipc {
            args.push("--unshare-ipc".into());
        }
        if policy.unshare_uts {
            args.push("--unshare-uts".into());
        }
        args.push("--chdir".into());
        args.push(workspace.as_os_str().to_os_string());
        args.push("--".into());
        args.push(program.as_os_str().to_os_string());
        args.extend(request.args.iter().map(OsString::from));
        Ok(args)
    }
}

impl ExecutionBackend for BubblewrapBackend {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    fn status(&self) -> BackendStatus {
        let unavailable_reason = self.program_problem();
        BackendStatus {
            name: self.name(),
            available: unavailable_reason.is_none(),
            unavailable_reason,
            guarantees: self.policy.guarantees(),
            caveats: BUBBLEWRAP_CAVEATS,
            program: Some(self.program.clone()),
            probe: BackendProbe::NotProbed,
        }
    }

    fn wrap(&self, request: &ExecutionRequest) -> Result<SandboxCommand, BackendError> {
        let status = self.status();
        if !status.available {
            return Err(BackendError::Unavailable {
                backend: self.name(),
                reason: status
                    .unavailable_reason
                    .unwrap_or_else(|| "backend unavailable".into()),
            });
        }
        self.policy.limits.validate()?;
        let workspace = canonical_workspace(&request.workspace)?;
        if !request.program.is_absolute() {
            return Err(BackendError::InvalidRequest(format!(
                "program {} must be an absolute path so it resolves inside the sandbox",
                request.program.display()
            )));
        }
        let program = std::fs::canonicalize(&request.program).map_err(|error| {
            BackendError::InvalidRequest(format!(
                "program {} is unavailable: {error}",
                request.program.display()
            ))
        })?;
        if !program.is_file() {
            return Err(BackendError::InvalidRequest(format!(
                "program {} is not a file",
                program.display()
            )));
        }
        if let FilesystemPolicy::ExplicitMounts { read_only } = &self.policy.filesystem {
            // A program inside the writable workspace is always reachable;
            // it is not required to also appear in the read-only mounts.
            let covered = program.starts_with(&workspace)
                || read_only.iter().any(|mount| {
                    program.starts_with(mount)
                        || std::fs::canonicalize(mount)
                            .map(|canonical| program.starts_with(canonical))
                            .unwrap_or(false)
                });
            if !covered {
                return Err(BackendError::InvalidRequest(format!(
                    "program {} is not covered by the workspace or the explicit read-only mounts",
                    program.display()
                )));
            }
        }
        if self.policy.private_tmp
            && program.starts_with("/tmp")
            && !program.starts_with(&workspace)
        {
            return Err(BackendError::InvalidRequest(format!(
                "program {} lives under /tmp, which this policy replaces with a private tmpfs",
                program.display()
            )));
        }
        let args = self.wrap_args(request, &workspace, &program)?;
        Ok(SandboxCommand::new(
            self.name(),
            self.program.clone(),
            args,
            workspace,
            self.policy.environment.clone(),
            self.policy.limits,
            self.policy.guarantees(),
        ))
    }
}

/// Environment for a [`SupervisedCommand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandEnvironment {
    /// Inherit the supervisor environment unchanged.
    ///
    /// A sandboxed policy refuses this: inheriting would hand the whole host
    /// environment, including secrets, to the sandboxed command.
    Supervisor,
    /// Add exactly these variables.
    ///
    /// An unrestricted policy keeps the legacy behavior and adds them to the
    /// inherited environment. A sandboxed policy starts from its own
    /// [`BubblewrapPolicy::environment`] and never inherits the supervisor's,
    /// so MCP `spec.env` and plugin `MIO_TOOL_ARGS` stay explicit configuration
    /// instead of a copy of the host environment.
    Configured(Vec<(String, String)>),
}

impl CommandEnvironment {
    fn variables(&self) -> &[(String, String)] {
        match self {
            Self::Supervisor => &[],
            Self::Configured(variables) => variables,
        }
    }
}

/// One command to run under an [`ExecutionPolicy`], with its explicit argv,
/// environment and workspace.
///
/// This is the supervisor-facing helper for the engine's own entry points
/// (shell, plugin tools, MCP servers). It keeps the caller's argument vector
/// verbatim, never shell-reparses it, and always pins the working directory.
/// [`ExecutionPolicy::command`] turns it into a concrete [`Command`]; when the
/// requested backend cannot run, that call fails and no unsandboxed command is
/// substituted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisedCommand {
    program: PathBuf,
    args: Vec<OsString>,
    workspace: PathBuf,
    environment: CommandEnvironment,
}

impl SupervisedCommand {
    /// Start a command for `program` in `workspace`.
    ///
    /// The default environment is [`CommandEnvironment::Configured`] with no
    /// variables, so a sandboxed run gets exactly its policy environment and an
    /// unrestricted run keeps inheriting the supervisor environment.
    pub fn new(program: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            workspace: workspace.into(),
            environment: CommandEnvironment::Configured(Vec::new()),
        }
    }

    /// Append one argument, verbatim.
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// Append several arguments, verbatim.
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Add one explicit environment variable.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let mut variables = match self.environment {
            CommandEnvironment::Configured(variables) => variables,
            CommandEnvironment::Supervisor => Vec::new(),
        };
        variables.push((name.into(), value.into()));
        self.environment = CommandEnvironment::Configured(variables);
        self
    }

    /// Add several explicit environment variables.
    pub fn envs<I, K, V>(self, variables: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        variables
            .into_iter()
            .fold(self, |command, (name, value)| command.env(name, value))
    }

    /// Inherit the supervisor environment instead of adding explicit variables.
    pub fn inherit_environment(mut self) -> Self {
        self.environment = CommandEnvironment::Supervisor;
        self
    }

    /// Program to execute.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Argument vector, without the program path.
    pub fn arguments(&self) -> &[OsString] {
        &self.args
    }

    /// Working directory.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Environment policy for this command.
    pub fn environment(&self) -> &CommandEnvironment {
        &self.environment
    }

    fn unrestricted_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.current_dir(&self.workspace);
        for (name, value) in self.environment.variables() {
            command.env(name, value);
        }
        command
    }
}

/// A backend plus the policy it should apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionPolicy {
    /// Run directly with the caller's privileges.
    Unrestricted,
    /// Run inside a Linux bubblewrap sandbox; fails closed when unavailable.
    Bubblewrap(BubblewrapPolicy),
}

impl ExecutionPolicy {
    /// Stable backend name for this policy.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Bubblewrap(_) => "bubblewrap",
        }
    }

    fn bubblewrap_backend(policy: &BubblewrapPolicy) -> BubblewrapBackend {
        match &policy.helper_program {
            Some(program) => BubblewrapBackend::at(program.clone(), HostPlatform::current()),
            None => BubblewrapBackend::detect(),
        }
    }

    /// Availability and isolation for this policy on the current host.
    ///
    /// This is the cheap helper and platform check; it does not prove the
    /// kernel allows the sandbox. [`Self::probe`] runs a real bounded command.
    pub fn status(&self) -> BackendStatus {
        match self {
            Self::Unrestricted => UnrestrictedBackend.status(),
            Self::Bubblewrap(policy) => Self::bubblewrap_backend(policy)
                .with_policy(policy.clone())
                .status(),
        }
    }

    /// Resolve the request into a command plan.
    ///
    /// A bubblewrap policy returns [`BackendError::Unavailable`] rather than an
    /// unwrapped command when the sandbox cannot run, so callers cannot
    /// accidentally execute outside the requested isolation.
    pub fn resolve(&self, request: &ExecutionRequest) -> Result<SandboxCommand, BackendError> {
        match self {
            Self::Unrestricted => UnrestrictedBackend.wrap(request),
            Self::Bubblewrap(policy) => Self::bubblewrap_backend(policy)
                .with_policy(policy.clone())
                .wrap(request),
        }
    }

    /// Build the concrete [`Command`] for one supervised command.
    ///
    /// `Unrestricted` runs the program directly, preserving the caller's argv,
    /// workspace and explicit variables (the legacy host behavior).
    ///
    /// `Bubblewrap` resolves the argv into a sandbox plan and applies it. The
    /// program is resolved to an absolute path against the effective
    /// environment (a bare name searches `PATH`; a relative path resolves
    /// inside the workspace), and the sandbox environment is the policy's
    /// environment plus the command's explicit variables. A policy that asks to
    /// inherit the supervisor environment, or that cannot run, is refused with
    /// [`BackendError`]: no unsandboxed command is returned.
    ///
    /// The returned command has no stdio, process group or kill-on-drop
    /// configured; [`SandboxCommand::apply`] replaces the whole command, so
    /// callers must set those after this call.
    pub fn command(&self, request: &SupervisedCommand) -> Result<Command, BackendError> {
        match self {
            Self::Unrestricted => Ok(request.unrestricted_command()),
            Self::Bubblewrap(policy) => {
                if matches!(request.environment, CommandEnvironment::Supervisor) {
                    return Err(BackendError::InvalidRequest(
                        "a sandboxed command cannot inherit the supervisor environment; pass explicit variables or use ExecutionPolicy::Unrestricted".into(),
                    ));
                }
                let workspace = canonical_workspace(&request.workspace)?;
                let base =
                    environment_variables(&policy.environment, &workspace).ok_or_else(|| {
                        BackendError::InvalidRequest(
                            "a sandboxed policy cannot inherit the supervisor environment; set BubblewrapPolicy::environment to SandboxDefaults or Explicit".into(),
                        )
                    })?;
                let environment = merge_environment(base, request.environment.variables());
                let program = resolve_program(&request.program, &workspace, &environment)?;
                let args = request
                    .args
                    .iter()
                    .map(|argument| {
                        argument.to_str().map(str::to_owned).ok_or_else(|| {
                            BackendError::InvalidRequest(
                                "sandboxed arguments must be valid UTF-8".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let execution = ExecutionRequest {
                    program,
                    args,
                    workspace: workspace.clone(),
                };
                let policy = policy
                    .clone()
                    .with_environment(EnvironmentPolicy::Explicit(environment));
                let plan = Self::bubblewrap_backend(&policy)
                    .with_policy(policy)
                    .wrap(&execution)?;
                let mut command = Command::new(plan.program());
                plan.apply(&mut command).map_err(|error| {
                    BackendError::InvalidRequest(format!(
                        "cannot configure the sandboxed command: {error}"
                    ))
                })?;
                Ok(command)
            }
        }
    }

    /// A stable, credential-free digest of this policy.
    ///
    /// Every policy field is covered, including the *values* of an explicit
    /// environment. Hosts may use this as a cache key so two runs with
    /// different isolation or credentials never share a live child. The digest
    /// is a SHA-256 hex string; [`fmt::Debug`] is redacted and must not be used
    /// as a key.
    pub fn fingerprint(&self) -> String {
        let mut digest = PolicyFingerprint::new(self.name());
        let Self::Bubblewrap(policy) = self else {
            return digest.finish();
        };
        let helper = match &policy.helper_program {
            Some(program) => program.to_string_lossy().into_owned(),
            None => BWRAP_PROGRAM.to_owned(),
        };
        digest.field("helper", helper);
        match &policy.filesystem {
            FilesystemPolicy::HostRootReadOnly => {
                digest.field("filesystem", "host-root-read-only");
            }
            FilesystemPolicy::ExplicitMounts { read_only } => {
                digest.field("filesystem", "explicit-mounts");
                for mount in read_only {
                    digest.field("mount", mount.to_string_lossy().into_owned());
                }
            }
        }
        digest.optional_u64("cpu_seconds", policy.limits.cpu_seconds);
        digest.optional_u64("address_space_bytes", policy.limits.address_space_bytes);
        digest.optional_u64("open_files", policy.limits.open_files);
        match &policy.environment {
            EnvironmentPolicy::Inherit => digest.field("environment", "inherit"),
            EnvironmentPolicy::SandboxDefaults => digest.field("environment", "sandbox-defaults"),
            EnvironmentPolicy::Explicit(variables) => {
                digest.field("environment", "explicit");
                for (name, value) in variables {
                    digest.field("env-name", name.as_bytes());
                    digest.field("env-value", value.as_bytes());
                }
            }
        }
        digest.flag("private_tmp", policy.private_tmp);
        digest.flag("private_pid", policy.private_pid);
        digest.flag("disable_network", policy.disable_network);
        digest.flag("die_with_parent", policy.die_with_parent);
        digest.flag("new_session", policy.new_session);
        digest.flag("unshare_ipc", policy.unshare_ipc);
        digest.flag("unshare_uts", policy.unshare_uts);
        digest.finish()
    }

    /// Run a bounded, real probe in `workspace`.
    ///
    /// Unlike [`Self::status`], this starts a short-lived sandboxed command and
    /// observes it, so [`BackendStatus::probe`] becomes
    /// [`BackendProbe::Verified`] only when the kernel really allowed the
    /// sandbox. `Unrestricted` has no isolation to verify and stays
    /// [`BackendProbe::NotProbed`]. A failed probe sets `available` to `false`
    /// and records the observed reason instead of claiming the backend works.
    pub async fn probe(&self, workspace: &Path) -> BackendStatus {
        let mut status = self.status();
        match self {
            Self::Unrestricted => status,
            Self::Bubblewrap(_) => {
                if !status.available {
                    status.probe = BackendProbe::Failed(
                        status
                            .unavailable_reason
                            .clone()
                            .unwrap_or_else(|| "backend unavailable".into()),
                    );
                    return status;
                }
                match run_sandbox_probe(self, workspace).await {
                    Ok(()) => status.probe = BackendProbe::Verified,
                    Err(reason) => {
                        status.available = false;
                        status.probe = BackendProbe::Failed(reason.clone());
                        status.unavailable_reason = Some(reason);
                    }
                }
                status
            }
        }
    }
}

/// Linux runtime paths needed by a dynamically linked program when the host
/// root is not mounted.
///
/// Only paths that exist are returned. Trusted runtime paths are frequently
/// symlinked aliases (`/bin -> usr/bin`, `/lib64 -> usr/lib64`), so each entry
/// is returned together with its canonical target: dropping either the alias or
/// the real location can break a program or the dynamic loader. Callers should
/// extend the list with the locale, TLS bundle, toolchain or data directories
/// they need and must not add the host root back.
pub fn linux_runtime_mounts() -> Vec<PathBuf> {
    let mut mounts: Vec<PathBuf> = Vec::new();
    for candidate in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
    ] {
        let path = PathBuf::from(candidate);
        if !path.exists() {
            continue;
        }
        push_unique(&mut mounts, path.clone());
        if let Ok(canonical) = std::fs::canonicalize(&path)
            && canonical != Path::new("/")
            && canonical != path
        {
            push_unique(&mut mounts, canonical);
        }
    }
    mounts
}

fn push_unique(mounts: &mut Vec<PathBuf>, path: PathBuf) {
    if !mounts.contains(&path) {
        mounts.push(path);
    }
}

fn push_ro_bind(args: &mut Vec<OsString>, path: &std::ffi::OsStr) {
    args.push("--ro-bind".into());
    args.push(path.to_os_string());
    args.push(path.to_os_string());
}

/// Add explicit variables on top of a base environment, with the explicit
/// value winning on a name collision.
fn merge_environment(
    mut base: Vec<(String, String)>,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    for (name, value) in extra {
        match base.iter_mut().find(|(existing, _)| existing == name) {
            Some(slot) => slot.1 = value.clone(),
            None => base.push((name.clone(), value.clone())),
        }
    }
    base
}

/// Resolve a supervised program to an absolute, canonical path inside the
/// sandbox. A bare name searches the effective `PATH`; a relative path is
/// resolved against the workspace. Failure is explicit instead of falling back
/// to an unresolved name.
fn resolve_program(
    program: &Path,
    workspace: &Path,
    environment: &[(String, String)],
) -> Result<PathBuf, BackendError> {
    let candidate = if program.is_absolute() {
        program.to_path_buf()
    } else if program.components().count() > 1 {
        workspace.join(program)
    } else {
        let path = environment
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap_or(SANDBOX_PATH);
        std::env::split_paths(path)
            .filter(|directory| !directory.as_os_str().is_empty())
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| {
                BackendError::InvalidRequest(format!(
                    "program {} is not on the sandbox PATH",
                    program.display()
                ))
            })?
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        BackendError::InvalidRequest(format!(
            "program {} is unavailable: {error}",
            candidate.display()
        ))
    })?;
    if !canonical.is_file() {
        return Err(BackendError::InvalidRequest(format!(
            "program {} is not a file",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Bound on one sandbox probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

async fn run_sandbox_probe(policy: &ExecutionPolicy, workspace: &Path) -> Result<(), String> {
    let program = ["/usr/bin/true", "/bin/true"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| "no true(1) binary found for the sandbox probe".to_string())?;
    let request = SupervisedCommand::new(program, workspace);
    let mut native = policy
        .command(&request)
        .map_err(|error| error.to_string())?;
    native
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut command = tokio::process::Command::from(native);
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.status())
        .await
        .map_err(|_| "sandbox probe timed out".to_string())?
        .map_err(|error| error.to_string())?;
    if output.success() {
        Ok(())
    } else {
        Err(format!("sandbox probe exited with {output}"))
    }
}

/// Length-prefixed SHA-256 accumulator for [`ExecutionPolicy::fingerprint`].
struct PolicyFingerprint(Sha256);

impl PolicyFingerprint {
    fn new(policy: &str) -> Self {
        let mut hasher = Sha256::new();
        write_field(&mut hasher, b"schema", b"mio-execution-policy-v1");
        write_field(&mut hasher, b"policy", policy.as_bytes());
        Self(hasher)
    }

    fn field(&mut self, label: &str, value: impl AsRef<[u8]>) {
        write_field(&mut self.0, label.as_bytes(), value.as_ref());
    }

    fn flag(&mut self, label: &str, value: bool) {
        self.field(label, [u8::from(value)]);
    }

    fn optional_u64(&mut self, label: &str, value: Option<u64>) {
        match value {
            Some(value) => self.field(label, value.to_le_bytes()),
            None => self.field(label, "unset"),
        }
    }

    fn finish(self) -> String {
        let mut output = String::with_capacity(64);
        use std::fmt::Write as _;
        for byte in self.0.finalize() {
            let _ = write!(output, "{byte:02x}");
        }
        output
    }
}

fn write_field(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, BackendError> {
    if !workspace.is_absolute() {
        return Err(BackendError::InvalidRequest(format!(
            "workspace {} must be an absolute path",
            workspace.display()
        )));
    }
    let canonical = std::fs::canonicalize(workspace).map_err(|error| {
        BackendError::InvalidRequest(format!(
            "workspace {} is unavailable: {error}",
            workspace.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(BackendError::InvalidRequest(format!(
            "workspace {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn canonical_mount(mount: &Path) -> Result<PathBuf, BackendError> {
    if !mount.is_absolute() {
        return Err(BackendError::InvalidRequest(format!(
            "read-only mount {} must be an absolute path",
            mount.display()
        )));
    }
    std::fs::canonicalize(mount).map_err(|error| {
        BackendError::InvalidRequest(format!(
            "read-only mount {} is unavailable: {error}",
            mount.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_debug_redacts_values() {
        let policy = EnvironmentPolicy::Explicit(vec![(
            "MIO_TEST_SECRET".into(),
            "super-secret-value".into(),
        )]);
        let rendered = format!("{policy:?}");
        assert!(rendered.contains("MIO_TEST_SECRET"));
        assert!(!rendered.contains("super-secret-value"));
    }

    #[test]
    fn limits_validation_rejects_degenerate_values() {
        let limits = ResourceLimits {
            cpu_seconds: Some(0),
            ..ResourceLimits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(BackendError::InvalidRequest(_))
        ));
        let limits = ResourceLimits {
            open_files: Some(2),
            ..ResourceLimits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(BackendError::InvalidRequest(_))
        ));
        assert!(
            ResourceLimits {
                cpu_seconds: Some(1),
                address_space_bytes: Some(1024),
                open_files: Some(3),
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn read_confinement_is_reported_only_for_explicit_mounts() {
        assert!(!IsolationGuarantees::NONE.confines_host_reads());
        let read_only = IsolationGuarantees {
            host_read_visibility: HostReadVisibility::HostRootReadOnly,
            ..IsolationGuarantees::NONE
        };
        assert!(!read_only.confines_host_reads());
        let explicit = IsolationGuarantees {
            host_read_visibility: HostReadVisibility::ExplicitMountsOnly,
            ..IsolationGuarantees::NONE
        };
        assert!(explicit.confines_host_reads());
    }

    #[test]
    fn linux_runtime_mounts_never_include_the_host_root() {
        for mount in linux_runtime_mounts() {
            assert!(mount.is_absolute());
            assert_ne!(mount, PathBuf::from("/"));
        }
    }
}
