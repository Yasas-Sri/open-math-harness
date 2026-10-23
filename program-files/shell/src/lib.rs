//! The shell service: the provider of the `shell` interface.
//!
//! One process, one root directory of workspaces. `exec` runs
//! `bash -e -o pipefail -c <command>` inside the workspace the call's resource names, with
//! the environment the operator configured and nothing inherited, a
//! wall-clock limit, and a cap on the output returned. Reading and writing
//! files, copying a workspace, running Lean are all commands (`cat`, `cp`,
//! `lean`); optional `stdin` carries literal text separately from the command.
//! It is what gives a model-driven prover Lean in its loop: read the file,
//! edit it, run `lean` on it, read the errors, again.
//!
//! # What a command may reach
//!
//! What the process this service is may reach, no less: a command is its
//! child and runs as its Linux user. In the VM a project's shell is a
//! process of the projects' user, in no sandbox, so a command reads and
//! writes what that user may, every project's folder included, at the files'
//! real paths, and has the network unless the instance was started without.
//! In a composition that runs the service as a program of the runtime's own
//! it is in that program's sandbox, and sees its binds. The workspace a call
//! names is where its command starts, checked against the caller's
//! capability; it is not a boundary: the workspaces of one instance belong
//! to one user.
//!
//! What the service does for every command, whoever it runs as:
//!
//! - the working directory is the workspace, and so are `HOME` and the
//!   temporary directory unless the operator named them;
//! - no command leaves anything running: the service and each command's
//!   process are child subreapers, so whatever a command starts stays beneath
//!   it, whatever process group or session it moves to (`setsid`, GNU
//!   `timeout`, a daemon), and when the command ends — by exit, by the
//!   wall-clock limit, because the caller's deadline is near, or because the
//!   caller withdrew the call — its process is killed if it still runs and
//!   everything it started is killed before the call is answered
//!   (`nucleus_preexec::reaper`). A command's process group is its own, so
//!   a script's `kill 0` reaches only its own processes;
//! - `ulimit -v` bounds each command's address space (`--max-vmem-mb`), the
//!   only memory bound: the instance's cgroup has none;
//! - the output returned is cut at a cap; what the command wrote past it is
//!   discarded, not buffered: while nobody takes a command's output, at most
//!   [`MAX_WAITING_BYTES`] of it wait in this process, however much it
//!   prints.
//!
//! What it does **not** do, and the operator must know: there is no CPU or
//! process-count limit and no seccomp filter, the memory bound is per
//! process rather than per command tree, and a command can signal every
//! process of its user, this service included.
//! Where the service runs as the runtime's user (a composition without
//! identities), a command can also open the service's kernel channel by name
//! under `/proc` and call the kernel as the service; as the projects' user
//! it cannot, the channel being a pipe of the runtime's user.
//!
//! Commands run on their own threads, `--max-inflight` at a time, so one long
//! Lean run does not block another worker's quick `cat` while a slot is free.
//! A call that arrives while every slot is taken waits for one, oldest first,
//! within its own deadline: it fails without running once too little of that
//! deadline is left to run a command, and a call cancelled before its turn
//! never runs. A call cancelled while it runs (its caller withdrew it or
//! exited, or its deadline passed) has its command and everything it started
//! killed at once, so its slot passes on instead of staying taken until the
//! command's own limit. A command's processes are one value, the
//! [`nucleus_preexec::reaper::Tree`] its thread holds, and end when their
//! holder ends them. Everything a command prints is untrusted input.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use nucleus_protocol::{
    ACTION_PROVIDE, DispatchId, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Event};
use nucleus_shell_interface::jobs::{self as job_wire, By};
use nucleus_shell_interface::{
    ACTION_DETACH, ACTION_EXEC, ACTION_OUTPUT, ACTION_STOP, ACTION_WAIT, Detached, ENDPOINT, ExecRequest,
    ExecResponse, JobRef, MAX_COMMAND_BYTES, MAX_OUTPUT_BYTES, MAX_STDIN_BYTES, OutputRequest, WaitRequest,
    host_interface, interface, workspace_segments,
};

mod jobs;

const DEFAULT_MAX_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
/// Kept back from the caller's deadline so the reply arrives before it.
const REPLY_MARGIN_MS: u64 = 500;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const DEFAULT_MAX_VMEM_MB: u64 = 32 * 1024;
const DEFAULT_MAX_INFLIGHT: usize = 16;
/// How long to collect output once the command and everything it started
/// have ended (a process outside it that was handed the pipe would
/// otherwise hold the reply forever).
const DRAIN_GRACE: Duration = Duration::from_millis(500);
const REPLY_POLL: Duration = Duration::from_millis(50);
/// Most output of one command that waits in this process to be taken: by its
/// call when it ends, or by the job it became. More is dropped and counted,
/// so a command that prints without end cannot fill the service's memory,
/// which every other command and job of the instance shares. It is what a
/// job's file keeps, so a command moved to the background loses nothing its
/// file would have held.
const MAX_WAITING_BYTES: u64 = job_wire::MAX_OUTPUT_BYTES;
const MAX_ERROR_CHARS: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The directory holding every workspace.
    pub root: PathBuf,
    /// The whole environment of every command; nothing else is inherited.
    pub env: Vec<(String, String)>,
    pub bash: PathBuf,
    pub max_timeout_ms: u64,
    pub default_timeout_ms: u64,
    pub max_output_bytes: u64,
    /// `ulimit -v` for each command, in MiB; 0 leaves it unbounded.
    pub max_vmem_mb: u64,
    pub max_inflight: usize,
    pub endpoint: EndpointName,
    /// The operator's account of what a command finds beyond what this
    /// service can see for itself (a toolchain on PATH, a library build, …),
    /// appended to the environment description the service declares.
    pub description: String,
    /// Whether commands have network (`--network on|off`), as whoever starts
    /// the instance knows it: this service only says so in its description.
    pub network: Option<bool>,
    /// Keeps background jobs (`--jobs on`, module `jobs`): only an instance
    /// the projects service runs and drives, which names every job.
    pub jobs: bool,
}

impl Options {
    /// What an instance started with these options tells a model about its
    /// commands ([`ShellService::environment_description`]), without starting
    /// one: a router declaring its hosts' interface needs it.
    pub fn description(&self) -> String {
        ShellService { options: self.clone(), jobs: None }.environment_description()
    }

    /// Parses `--root <dir> [--env NAME=VALUE]... [--bash <exe>]
    /// [--max-timeout-ms <n>] [--default-timeout-ms <n>] [--max-output-bytes <n>]
    /// [--max-vmem-mb <n>] [--max-inflight <n>] [--endpoint <name>]
    /// [--description <text>] [--network on|off] [--jobs on|off]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut root = None;
        let mut options = Options {
            root: PathBuf::new(),
            env: Vec::new(),
            bash: PathBuf::from("/usr/bin/bash"),
            max_timeout_ms: DEFAULT_MAX_TIMEOUT_MS,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_vmem_mb: DEFAULT_MAX_VMEM_MB,
            max_inflight: DEFAULT_MAX_INFLIGHT,
            endpoint: EndpointName::new(ENDPOINT).map_err(|e| e.to_string())?,
            description: String::new(),
            network: None,
            jobs: false,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let number = || value.parse::<u64>().map_err(|_| format!("{flag} must be a number"));
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(&value)),
                "--env" => {
                    let (name, val) = value
                        .split_once('=')
                        .ok_or_else(|| format!("--env takes NAME=VALUE, not {value:?}"))?;
                    if name.is_empty() || name.contains(['\0', '=']) || val.contains('\0') {
                        return Err(format!("--env: invalid variable {value:?}"));
                    }
                    options.env.push((name.to_owned(), val.to_owned()));
                }
                "--bash" => options.bash = PathBuf::from(&value),
                "--max-timeout-ms" => options.max_timeout_ms = number()?,
                "--default-timeout-ms" => options.default_timeout_ms = number()?,
                "--max-output-bytes" => options.max_output_bytes = number()?,
                "--max-vmem-mb" => options.max_vmem_mb = number()?,
                "--max-inflight" => {
                    options.max_inflight =
                        usize::try_from(number()?).map_err(|_| "--max-inflight too large")?;
                }
                "--endpoint" => options.endpoint = EndpointName::new(&value).map_err(|e| e.to_string())?,
                "--description" => options.description = value,
                "--network" => {
                    options.network = Some(match value.as_str() {
                        "on" => true,
                        "off" => false,
                        other => return Err(format!("--network takes on or off, not {other:?}")),
                    });
                }
                "--jobs" => {
                    options.jobs = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        other => return Err(format!("--jobs takes on or off, not {other:?}")),
                    };
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        options.root = root.ok_or("--root <dir> is required")?;
        if !options.root.is_absolute() {
            return Err("--root must be an absolute path".to_owned());
        }
        if options.max_timeout_ms < MIN_TIMEOUT_MS || options.default_timeout_ms < MIN_TIMEOUT_MS {
            return Err(format!("timeouts must be at least {MIN_TIMEOUT_MS} ms"));
        }
        options.default_timeout_ms = options.default_timeout_ms.min(options.max_timeout_ms);
        options.max_output_bytes = options.max_output_bytes.clamp(1, MAX_OUTPUT_BYTES);
        if options.max_inflight < 1 {
            return Err("--max-inflight must be at least 1".to_owned());
        }
        Ok(options)
    }
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> Error {
    Error::new(code, message.into())
}

fn io_error(what: &str, e: &std::io::Error) -> Error {
    // An escape attempt is reported as a permission error.
    let code = match e.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        std::io::ErrorKind::NotFound => ErrorCode::InvalidInput,
        _ => ErrorCode::ServiceError,
    };
    let detail: String = e.to_string().chars().take(MAX_ERROR_CHARS).collect();
    Error::new(code, format!("{what}: {detail}"))
}

/// A workspace directory, resolved and created by [`ShellService::workspace_dir`].
struct Workspace {
    /// Its path, for `HOME` and `TMPDIR`.
    path: PathBuf,
    /// The directory itself, opened without following any symlink.
    fd: std::os::fd::OwnedFd,
}

impl Workspace {
    /// Where a command starts: the opened directory itself, through its
    /// descriptor, so the path is not resolved a second time.
    fn cwd(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.fd.as_raw_fd()))
    }
}

/// What ends the wait for a running command: its exit, or its caller
/// withdrawing the call; for one that may become a job, the projects
/// service moving it to the background, and for a job, its stop.
pub(crate) enum Wake {
    Exited,
    Withdrawn,
    Detach,
    Stop,
}

/// The service's whole ability: its root and its limits, and its jobs.
pub struct ShellService {
    options: Options,
    jobs: Option<Arc<jobs::Jobs>>,
}

impl ShellService {
    /// Creates the root if it is missing; refuses a root that is not a
    /// directory, and a process that could not end what its commands leave.
    pub fn open(options: &Options) -> Result<Self, String> {
        std::fs::create_dir_all(&options.root)
            .map_err(|e| format!("cannot create the root {}: {e}", options.root.display()))?;
        nucleus_preexec::reaper::adopt_orphans()
            .map_err(|e| format!("cannot take over what commands leave running: {e}"))?;
        let jobs = options.jobs.then(|| Arc::new(jobs::Jobs::new(&options.root)));
        Ok(Self { options: options.clone(), jobs })
    }

    /// What this service tells a model about the environment of its commands
    /// (declared with the interface, listed by the kernel with the `exec`
    /// tool): the facts it knows from its own configuration, then the
    /// operator's `--description`.
    pub fn environment_description(&self) -> String {
        let o = &self.options;
        let mut text: String =
            "Commands run with `bash -e -o pipefail -c` in your workspace directory, which is already the \
             working directory: create files there and refer to them by relative path (`pwd` prints its \
             absolute path)."
                .to_owned();
        match o.env.iter().rev().find(|(name, _)| name == "HOME") {
            Some((_, home)) => text.push_str(&format!(
                " HOME is {home}: programs installed under it run from there (`pip install --user`, a venv, \
                 `uv`, `cargo install`, `npm install -g`, `elan`)."
            )),
            None => text.push_str(" It is also HOME and the temporary directory of every command."),
        }
        text.push_str(
            " A command runs with the permissions of the Linux user this shell runs as, at the files' real \
             paths: it can read and write whatever that user can, and the workspace is where it starts, not \
             a fence. Other workspaces of this shell belong to the same user, so only your own care keeps a \
             command out of them: work in yours, and write elsewhere only when the task is there.",
        );
        text.push(' ');
        text.push_str(match o.network {
            Some(true) => nucleus_shell_interface::NETWORK_ON,
            Some(false) => nucleus_shell_interface::NETWORK_OFF,
            None => "Network access is determined by this service's execution configuration.",
        });
        text.push_str(
            " Every process a command starts is killed when it returns, also one in the background or under \
             `setsid`, `nohup` or `timeout`, and stdout and stderr come back interleaved with the exit \
             status. An unhandled failed command or pipeline stops the script before later commands run. \
             Bash conditionals and explicit error handling still apply; previous side effects are not \
             undone. Use the optional stdin field for literal script or file content instead of embedding \
             it in shell syntax. For example, python3 - executes Python supplied in stdin. When replacing a \
             valuable file, write a temporary file in the same directory, validate it, then rename it over \
             the destination only on success. Inspect diagnostics as well as status.",
        );
        self.details(&mut text);
        text
    }

    /// The limits, the environment and the operator's own description.
    fn details(&self, text: &mut String) {
        let o = &self.options;
        text.push_str(&format!(
            " A command in the foreground is killed after {} s by default (at most {} s); its output is cut \
             at {} bytes; its address space is limited to {} MiB. At most {} commands run at once, across \
             every caller; one that arrives while they all run waits its turn, and no foreground command \
             runs past its call's deadline, so a long wait shortens its time limit.",
            o.default_timeout_ms / 1000,
            o.max_timeout_ms / 1000,
            o.max_output_bytes,
            o.max_vmem_mb,
            o.max_inflight
        ));
        text.push_str(" The environment is exactly:");
        for (name, value) in &o.env {
            text.push_str(&format!(" {name}={value};"));
        }
        text.push_str(&format!(
            " and {}, a number naming the command, by which what it leaves running is counted for it.",
            nucleus_preexec::reaper::TAG
        ));
        if !o.description.trim().is_empty() {
            text.push(' ');
            text.push_str(o.description.trim());
        }
    }

    /// The directory of the workspace `resource` names, created if absent.
    /// The root itself (an empty resource) is not a workspace. Each
    /// segment is opened beneath the previous one and a symlink anywhere is
    /// refused: the caller's scope was checked against the literal path, and
    /// a link would make the call start somewhere its capability does not
    /// name. The command's working directory is then the opened directory,
    /// never the path again.
    fn workspace_dir(&self, resource: &ResourcePath) -> Result<Workspace, Error> {
        use rustix::fs::{Mode, OFlags, ResolveFlags};
        use rustix::io::Errno;
        let segments = workspace_segments(&resource.segments().join("/"))?;
        let fail = |e: Errno| match e {
            Errno::LOOP | Errno::XDEV => {
                refuse(ErrorCode::PermissionDenied, "workspace: the path crosses a symbolic link")
            }
            e => io_error("workspace", &e.into()),
        };
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut fd = rustix::fs::open(&self.options.root, flags, Mode::empty()).map_err(fail)?;
        for segment in &segments {
            match rustix::fs::mkdirat(&fd, segment.as_str(), Mode::from_raw_mode(0o777)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(fail(e)),
            }
            let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
            fd = rustix::fs::openat2(&fd, segment.as_str(), flags, Mode::empty(), resolve).map_err(fail)?;
        }
        let path = segments.iter().fold(self.options.root.clone(), |dir, s| dir.join(s));
        Ok(Workspace { path, fd })
    }

    /// The time and output budget of one `exec`: a background job has no
    /// time limit unless its command names one.
    fn admit(&self, request: &ExecRequest, remaining_ms: u64) -> Result<(Option<Duration>, u64), Error> {
        if request.command.is_empty() {
            return Err(refuse(ErrorCode::InvalidInput, "command is empty"));
        }
        if request.command.len() > MAX_COMMAND_BYTES {
            return Err(refuse(
                ErrorCode::InvalidInput,
                format!("command exceeds {MAX_COMMAND_BYTES} bytes"),
            ));
        }
        if request.command.contains('\0') {
            return Err(refuse(ErrorCode::InvalidInput, "command contains a NUL byte"));
        }
        if request.stdin.as_ref().is_some_and(|text| text.len() > MAX_STDIN_BYTES) {
            return Err(refuse(ErrorCode::InvalidInput, format!("stdin exceeds {MAX_STDIN_BYTES} bytes")));
        }
        // A job is named by the projects service, which alone runs them.
        let background = request.background == Some(true);
        match (&request.job, &self.jobs) {
            (Some(_), None) | (None, _) if background => {
                return Err(refuse(
                    ErrorCode::InvalidInput,
                    "background jobs run only in a session's own shell in the VM, which names them",
                ));
            }
            (Some(_), None) => return Err(refuse(ErrorCode::InvalidInput, "this shell keeps no jobs")),
            (Some(id), Some(_)) if !job_wire::valid_id(id) => {
                return Err(refuse(ErrorCode::InvalidInput, format!("{id:?} is no job id")));
            }
            _ => {}
        }
        // A job's time is its own, whatever its call's deadline: the limit
        // its command names, or none. A foreground command's is its limit,
        // within that deadline.
        let budget = if background {
            request.timeout_ms
        } else {
            let asked = request.timeout_ms.unwrap_or(self.options.default_timeout_ms);
            Some(asked.min(self.options.max_timeout_ms).min(remaining_ms.saturating_sub(REPLY_MARGIN_MS)))
        };
        if budget.is_some_and(|budget| budget < MIN_TIMEOUT_MS)
            || remaining_ms < MIN_TIMEOUT_MS / 2 + REPLY_MARGIN_MS
        {
            return Err(refuse(ErrorCode::InvalidInput, "deadline is too short to run a command"));
        }
        let cap = request
            .max_output_bytes
            .map_or(self.options.max_output_bytes, |n| n.clamp(1, self.options.max_output_bytes));
        Ok((budget.map(Duration::from_millis), cap))
    }

    /// The wrapper Bash runs: limits, one output stream, then the
    /// caller's command as `$1` so that no quoting touches it.
    ///
    /// `-e` and `pipefail` make an unhandled failure visible: a failed
    /// command stops the script before a trailing `echo`, and a failed
    /// producer makes `producer | tail` fail. They are Bash options, not an
    /// enforcement boundary: `if`, `!`, `&&`, `||`, subshells, command
    /// substitutions and `set +e` follow ordinary Bash rules, and a program
    /// can still exit 0 on a bad result. A producer whose consumer stops early
    /// (`yes | head`) dies of SIGPIPE and the call reports 141; the status is
    /// returned as it is, never turned into success.
    fn wrapper(&self) -> String {
        let mut script = String::new();
        if self.options.max_vmem_mb > 0 {
            script.push_str(&format!(
                "ulimit -v {} 2>/dev/null\n",
                self.options.max_vmem_mb.saturating_mul(1024)
            ));
        }
        script.push_str("exec 2>&1\n");
        script.push_str("exec \"$2\" -e -o pipefail -c \"$1\"\n");
        script
    }

    #[cfg(test)]
    fn exec(
        &self,
        resource: &ResourcePath,
        request: &ExecRequest,
        remaining_ms: u64,
    ) -> Result<ExecResponse, Error> {
        self.exec_withdrawable(resource, request, remaining_ms, mpsc::channel())
    }

    /// Runs one `exec` call. A [`Wake::Withdrawn`] on the channel also ends
    /// its command: it is killed as at its time limit, and the call is
    /// answered `Cancelled`. A [`Wake::Detach`] makes a command with a job
    /// id a background job instead, and the call answers at once, as it
    /// does for one started in the background.
    fn exec_withdrawable(
        &self,
        resource: &ResourcePath,
        request: &ExecRequest,
        remaining_ms: u64,
        (wake, woken): (Sender<Wake>, Receiver<Wake>),
    ) -> Result<ExecResponse, Error> {
        let (timeout, cap) = self.admit(request, remaining_ms)?;
        let workspace = self.workspace_dir(resource)?;
        let dir = &workspace.path;
        // A bounded, anonymous file avoids a pipe writer that could block
        // forever when a command never reads stdin. The descriptor follows
        // the command lifetime; no named temporary artifact needs cleanup.
        let input = match &request.stdin {
            Some(text) => {
                let mut file = tempfile::tempfile().map_err(|e| io_error("create stdin", &e))?;
                file.write_all(text.as_bytes()).map_err(|e| io_error("write stdin", &e))?;
                file.rewind().map_err(|e| io_error("rewind stdin", &e))?;
                Stdio::from(file)
            }
            None => Stdio::null(),
        };
        let mut command = Command::new(&self.options.bash);
        command
            .args(["-e", "-o", "pipefail"])
            .arg("-c")
            .arg(self.wrapper())
            .arg("nucleus-exec")
            .arg(&request.command)
            .arg(&self.options.bash)
            .current_dir(workspace.cwd())
            .env_clear()
            .envs(self.options.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // `HOME` and the temporary directory are the caller's own workspace
        // unless the operator named them: the workspaces of one service share
        // a user — and one `/tmp` — so a shared home or `TMPDIR` is a place
        // where two callers' commands overwrite each other's files, a tool's
        // cache or a scratch file, with neither at fault.
        for name in ["HOME", "TMPDIR", "TMP", "TEMP"] {
            if !self.options.env.iter().any(|(set, _)| set == name) {
                command.env(name, dir);
            }
        }
        {
            use std::os::unix::process::CommandExt;
            // A group of its own, so that a script's `kill 0` reaches its own
            // processes, never the service or another caller's command.
            command.process_group(0);
        }
        let started = Instant::now();
        let mut tree =
            nucleus_preexec::reaper::spawn(&mut command).map_err(|e| io_error("spawn bash", &e))?;
        let child = tree.child();
        let pid = child.id();
        let (chunks, collect) = mpsc::channel::<Vec<u8>>();
        let flow = Arc::new(Flow::default());
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, chunks.clone(), Arc::clone(&flow));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, chunks, Arc::clone(&flow));
        }
        let exited = wake.clone();
        std::thread::spawn(move || {
            let _ = nucleus_preexec::reaper::wait_exited(pid);
            let _ = exited.send(Wake::Exited);
        });
        if request.background == Some(true) {
            let running = jobs::Running { tree, collect, flow, woken, wake, started, limit: timeout };
            return self.go_background(request, running, By::Agent);
        }
        // It runs until it exits, its time limit passes or its call is
        // withdrawn; then it ends, and everything it started with it. Moved
        // to the background, it goes on as a job, which takes over its
        // processes and its output and kills nothing until it ends.
        let deadline = timeout.map(|timeout| started + timeout);
        let woke = loop {
            match jobs::recv_until(&woken, deadline) {
                // Moved, it has no time limit: a job's time is its own.
                Ok(Wake::Detach) if request.job.is_some() => {
                    let running = jobs::Running { tree, collect, flow, woken, wake, started, limit: None };
                    return self.go_background(request, running, By::Person);
                }
                Ok(Wake::Detach | Wake::Stop) => {}
                woke => break woke,
            }
        };
        let ended = tree.end().map_err(|e| {
            refuse(ErrorCode::ServiceError, format!("the command's processes could not all be ended: {e}"))
        })?;
        if matches!(woke, Ok(Wake::Withdrawn)) {
            let also = match ended.leftovers {
                0 => String::new(),
                1 => format!(", with the 1 other process it had started.{}", self.instead()),
                n => format!(", with the {n} other processes it had started.{}", self.instead()),
            };
            return Err(refuse(
                ErrorCode::Cancelled,
                format!("the call was withdrawn; its command was killed{also}"),
            ));
        }
        let timed_out = matches!(woke, Err(RecvTimeoutError::Timeout));
        let (output, total) = collect_output(&collect, &flow, cap, DRAIN_GRACE);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(ExecResponse {
            exit_code: ended.status.code(),
            timed_out,
            output: String::from_utf8_lossy(&output).into_owned(),
            truncated: total > cap,
            elapsed_ms,
            stopped_leftovers: (ended.leftovers > 0).then_some(ended.leftovers),
            note: self.leftovers_note(ended.leftovers, timed_out),
            ..ExecResponse::default()
        })
    }

    /// What the model is told when a command's end stopped processes it had
    /// left running: that it did, and how work keeps running instead. At its
    /// time limit a command is killed as it runs, so what it had started is
    /// whatever it was running, not something it left.
    fn leftovers_note(&self, leftovers: u64, timed_out: bool) -> Option<String> {
        let what = match (leftovers, timed_out) {
            (0, _) => return None,
            (1, true) => {
                "The 1 other process this command had started was stopped with it at its time limit."
                    .to_owned()
            }
            (n, true) => format!(
                "The {n} other processes this command had started were stopped with it at its time limit."
            ),
            (1, false) => "1 process this command left running (started in the background, or with setsid, \
                           nohup or timeout) was stopped when it returned."
                .to_owned(),
            (n, false) => format!(
                "{n} processes this command left running (started in the background, or with setsid, nohup \
                 or timeout) were stopped when it returned."
            ),
        };
        Some(format!("{what}{}", self.instead()))
    }

    /// What keeps work running when a command's processes do not: a
    /// background job, where this instance keeps them.
    fn instead(&self) -> &'static str {
        if self.jobs.is_some() {
            " To keep work running after a command, start it as a background job (background: true)."
        } else {
            " Everything a command starts ends with it."
        }
    }

    /// Hands a running command to a new job and answers its call.
    fn go_background(
        &self,
        request: &ExecRequest,
        running: jobs::Running,
        by: By,
    ) -> Result<ExecResponse, Error> {
        let (Some(jobs), Some(id)) = (&self.jobs, &request.job) else {
            return Err(refuse(ErrorCode::InvalidInput, "this shell keeps no jobs"));
        };
        let elapsed_ms = u64::try_from(running.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let limit_ms = running.limit.map(|limit| u64::try_from(limit.as_millis()).unwrap_or(u64::MAX));
        jobs.adopt(id, running)?;
        let output = jobs.output_path(id);
        let until = match limit_ms {
            Some(ms) => format!("until its time limit, {} s from its start", ms / 1000),
            None => "with no time limit".to_owned(),
        };
        let note = format!(
            "This command goes on as background job {id}; it keeps running after this answer, {until}. Its \
             output goes to {output}."
        );
        Ok(ExecResponse {
            elapsed_ms,
            job: Some(Detached { id: id.clone(), by, author: None, output, limit_ms, note }),
            ..ExecResponse::default()
        })
    }
}

/// What a command's output readers have handed over and nobody has taken
/// yet, and what they dropped because that had reached
/// [`MAX_WAITING_BYTES`].
#[derive(Default)]
pub(crate) struct Flow {
    waiting: AtomicU64,
    dropped: AtomicU64,
}

impl Flow {
    /// A job took `bytes` of its command's output.
    pub(crate) fn taken(&self, bytes: usize) {
        self.waiting.fetch_sub(bytes as u64, Ordering::Relaxed);
    }
}

/// Reads a pipe to its end, handing chunks over as they come, within
/// [`MAX_WAITING_BYTES`] not yet taken; the writer is never blocked on a
/// full pipe: what does not fit is dropped and counted. Runs on its own
/// thread; the receiver may be gone (the reply was sent), in which case the
/// rest is drained and dropped too.
fn spawn_reader(mut pipe: impl Read + Send + 'static, chunks: Sender<Vec<u8>>, flow: Arc<Flow>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if flow.waiting.load(Ordering::Relaxed) + n as u64 > MAX_WAITING_BYTES {
                        flow.dropped.fetch_add(n as u64, Ordering::Relaxed);
                        continue;
                    }
                    flow.waiting.fetch_add(n as u64, Ordering::Relaxed);
                    if chunks.send(buffer[..n].to_vec()).is_err() {
                        // Nobody listens any more; keep the pipe flowing.
                        let mut sink = std::io::sink();
                        let _ = std::io::copy(&mut pipe, &mut sink);
                        break;
                    }
                }
            }
        }
    });
}

/// Collects up to `cap` bytes of output and counts the rest, what the
/// readers dropped included, waiting at most `grace` after they fall silent.
fn collect_output(chunks: &Receiver<Vec<u8>>, flow: &Flow, cap: u64, grace: Duration) -> (Vec<u8>, u64) {
    let mut kept = Vec::new();
    let mut total = 0u64;
    loop {
        match chunks.recv_timeout(grace) {
            Ok(chunk) => {
                total = total.saturating_add(chunk.len() as u64);
                let room = usize::try_from(cap.saturating_sub(kept.len() as u64)).unwrap_or(usize::MAX);
                if room > 0 {
                    kept.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
            }
            Err(_) => return (kept, total.saturating_add(flow.dropped.load(Ordering::Relaxed))),
        }
    }
}

// --- Serving ------------------------------------------------------------------

type Finished = (DispatchId, Result<Payload, Error>);

/// An admitted call that has not started yet.
struct Waiting {
    id: DispatchId,
    resource: ResourcePath,
    request: ExecRequest,
    arrived: Instant,
    /// The caller's time left when the call arrived.
    remaining_ms: u64,
}

impl Waiting {
    fn remaining_ms(&self, now: Instant) -> u64 {
        let waited =
            u64::try_from(now.saturating_duration_since(self.arrived).as_millis()).unwrap_or(u64::MAX);
        self.remaining_ms.saturating_sub(waited)
    }
}

/// The `--max-inflight` command slots: the commands running, each on its own
/// thread and reporting back on one channel, and the calls waiting for a
/// slot, oldest first. The kernel bounds how many dispatches one provider
/// holds at once, and with it this queue.
struct Slots {
    /// The calls whose commands run, each with the way to withdraw it.
    running: HashMap<DispatchId, Sender<Wake>>,
    /// The running commands that may become jobs, by job id.
    by_job: HashMap<String, DispatchId>,
    /// Job calls a thread answers (`wait`, `stop`): they take no slot.
    answering: HashSet<DispatchId>,
    waiting: VecDeque<Waiting>,
    report: Sender<Finished>,
    finished: Receiver<Finished>,
}

impl Slots {
    fn new() -> Self {
        let (report, finished) = mpsc::channel();
        Self {
            running: HashMap::new(),
            by_job: HashMap::new(),
            answering: HashSet::new(),
            waiting: VecDeque::new(),
            report,
            finished,
        }
    }

    /// A call is answered: what it held is free.
    fn answered(&mut self, id: DispatchId) {
        self.running.remove(&id);
        self.answering.remove(&id);
        self.by_job.retain(|_, running| *running != id);
    }

    /// A job action (module `jobs`), answered now, or by a thread of its
    /// own (`None`).
    fn job_action(
        &mut self,
        service: &ShellService,
        d: &nucleus_sdk::Dispatch,
    ) -> Option<Result<Payload, Error>> {
        let jobs = service.jobs.as_ref()?;
        let threaded = |slots: &mut Self, work: Box<dyn FnOnce() -> Result<Payload, Error> + Send>| {
            slots.answering.insert(d.id);
            let (report, id) = (slots.report.clone(), d.id);
            std::thread::spawn(move || {
                let _ = report.send((id, work()));
            });
            None
        };
        match d.action.as_str() {
            ACTION_DETACH => Some(d.payload.decode::<JobRef>().and_then(|named| {
                let wake = self.by_job.get(&named.job).and_then(|running| self.running.get(running));
                let waits = self.waiting.iter().any(|w| w.request.job.as_deref() == Some(named.job.as_str()));
                match wake {
                    Some(wake) if wake.send(Wake::Detach).is_ok() => Payload::encode(&serde_json::json!({})),
                    // Not for good: it starts when a slot is free.
                    _ if waits => Err(refuse(
                        ErrorCode::ServiceError,
                        "that command has not started yet; try again in a moment",
                    )),
                    _ => Err(refuse(
                        ErrorCode::InvalidInput,
                        format!("job {} has no running command here", named.job),
                    )),
                }
            })),
            ACTION_WAIT => match d.payload.decode::<WaitRequest>() {
                Ok(request) => {
                    let jobs = Arc::clone(jobs);
                    threaded(
                        self,
                        Box::new(move || {
                            let wait = Duration::from_millis(request.wait_ms);
                            jobs.wait(&request.job, wait).and_then(|status| Payload::encode(&status))
                        }),
                    )
                }
                Err(e) => Some(Err(e)),
            },
            ACTION_STOP => match d.payload.decode::<JobRef>() {
                Ok(named) => {
                    let jobs = Arc::clone(jobs);
                    threaded(
                        self,
                        Box::new(move || jobs.stop(&named.job).and_then(|status| Payload::encode(&status))),
                    )
                }
                Err(e) => Some(Err(e)),
            },
            ACTION_OUTPUT => Some(
                d.payload
                    .decode::<OutputRequest>()
                    .and_then(|request| jobs.output(&request.job, request.after))
                    .and_then(|chunk| Payload::encode(&chunk)),
            ),
            _ => None,
        }
    }

    /// Starts waiting calls, oldest first, while a slot is free. A call whose
    /// deadline no longer leaves time to run a command never starts: it is
    /// returned with its answer.
    fn start_waiting(&mut self, service: &Arc<ShellService>) -> Vec<(DispatchId, Error)> {
        let now = Instant::now();
        let slots = service.options.max_inflight;
        let mut late = Vec::new();
        self.waiting.retain(|w| {
            let runnable = w.remaining_ms(now) >= MIN_TIMEOUT_MS + REPLY_MARGIN_MS;
            if !runnable {
                let why =
                    format!("no command slot came free before the call's deadline ({slots} run at once)");
                late.push((w.id, refuse(ErrorCode::Timeout, why)));
            }
            runnable
        });
        while self.running.len() < slots
            && let Some(call) = self.waiting.pop_front()
        {
            let remaining_ms = call.remaining_ms(now);
            let (wake, woken) = mpsc::channel();
            self.running.insert(call.id, wake.clone());
            if let Some(job) = &call.request.job {
                self.by_job.insert(job.clone(), call.id);
            }
            let (service, report) = (Arc::clone(service), self.report.clone());
            std::thread::spawn(move || {
                let result = service
                    .exec_withdrawable(&call.resource, &call.request, remaining_ms, (wake, woken))
                    .and_then(|r| Payload::encode(&r));
                let _ = report.send((call.id, result));
            });
        }
        late
    }
}

/// Provides the endpoint and serves requests until told to terminate. A
/// terminate ends the process; its sandbox or its cgroup ends every command
/// with it.
pub fn run(options: &Options) -> Result<(), String> {
    let service = Arc::new(ShellService::open(options)?);
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    let (endpoint, description) = (options.endpoint.clone(), service.environment_description());
    let declared = if options.jobs {
        host_interface(endpoint, description)
    } else {
        interface(endpoint, description, false)
    };
    kernel
        .provide(authority, declared.map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    // As a project's host, its router forwards nothing before this.
    kernel.announce_ready().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    serve(&service, &mut kernel)
}

/// Serves dispatches until told to terminate. A call is checked when it
/// arrives, then starts as soon as a slot is free ([`Slots`]).
fn serve<R: Read, W: Write>(
    service: &Arc<ShellService>,
    kernel: &mut Connection<R, W>,
) -> Result<(), String> {
    let mut slots = Slots::new();
    loop {
        while let Ok((id, result)) = slots.finished.try_recv() {
            slots.answered(id);
            kernel.reply(id, result).map_err(|e| e.to_string())?;
        }
        for (id, late) in slots.start_waiting(service) {
            kernel.reply(id, Err(late)).map_err(|e| e.to_string())?;
        }
        // A call still waiting means every slot is running, so this polls,
        // as it does while a thread answers a job's call.
        let event = if slots.running.is_empty() && slots.answering.is_empty() {
            kernel.next_event().map_err(|e| e.to_string())?
        } else {
            match kernel.next_event_within(REPLY_POLL).map_err(|e| e.to_string())? {
                Some(event) => event,
                None => continue,
            }
        };
        match event {
            Event::Dispatch(dispatch) if dispatch.action.as_str() == ACTION_EXEC => {
                // What its turn would refuse is refused now, without a wait.
                let admitted = dispatch
                    .payload
                    .decode::<ExecRequest>()
                    .and_then(|request| service.admit(&request, dispatch.remaining_ms).map(|_| request));
                match admitted {
                    Ok(request) => slots.waiting.push_back(Waiting {
                        id: dispatch.id,
                        resource: dispatch.resource,
                        request,
                        arrived: Instant::now(),
                        remaining_ms: dispatch.remaining_ms,
                    }),
                    Err(e) => kernel.reply(dispatch.id, Err(e)).map_err(|e| e.to_string())?,
                }
            }
            Event::Dispatch(dispatch) => {
                let unknown = || Err(refuse(ErrorCode::InvalidInput, "unknown action"));
                let answer = match slots.job_action(service, &dispatch) {
                    Some(answer) => Some(answer),
                    None if slots.answering.contains(&dispatch.id) => None,
                    None => Some(unknown()),
                };
                if let Some(answer) = answer {
                    kernel.reply(dispatch.id, answer).map_err(|e| e.to_string())?;
                }
            }
            // Nobody waits for its outcome: a call that has not started never
            // does, and one that runs has its command killed as at its limit,
            // so its slot passes on. Either is answered `Cancelled`, which
            // frees the kernel's in-flight entry; a running one by its thread.
            Event::CancelDispatch(id) => {
                if let Some(wake) = slots.running.get(&id) {
                    let _ = wake.send(Wake::Withdrawn);
                } else if let Some(at) = slots.waiting.iter().position(|w| w.id == id) {
                    slots.waiting.remove(at);
                    let withdrawn = refuse(ErrorCode::Cancelled, "the call was withdrawn before it started");
                    kernel.reply(id, Err(withdrawn)).map_err(|e| e.to_string())?;
                }
            }
            Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::path::Path;

    use super::*;

    fn service(root: &Path, extra: &[&str]) -> ShellService {
        let mut args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        args.extend(["--env".to_owned(), "PATH=/usr/bin:/bin".to_owned()]);
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        ShellService::open(&Options::parse(args).unwrap()).unwrap()
    }

    fn ws(segments: &[&str]) -> ResourcePath {
        ResourcePath::new(segments.iter().copied()).unwrap()
    }

    fn exec(s: &ShellService, r: &ResourcePath, command: &str, timeout_ms: Option<u64>) -> ExecResponse {
        s.exec(
            r,
            &ExecRequest {
                command: command.to_owned(),
                stdin: None,
                timeout_ms,
                max_output_bytes: None,
                ..Default::default()
            },
            60_000,
        )
        .unwrap()
    }

    #[test]
    fn an_unhandled_failure_is_not_hidden_by_echo_or_a_pipeline_consumer() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path(), &[]);
        let w = ws(&["run", "problem", "task"]);
        for command in [
            "bash -c 'echo failed >&2; exit 7'; echo misleading-success",
            "bash -c 'echo failed >&2; exit 7' 2>&1 | tail -n 10; echo misleading-success",
        ] {
            let result = exec(&s, &w, command, None);
            assert_eq!(result.exit_code, Some(7), "{result:?}");
            assert!(result.output.contains("failed"), "{result:?}");
            assert!(!result.output.contains("misleading-success"), "{result:?}");
        }
        // Intentional handling remains ordinary Bash behavior.
        for command in [
            "bash -c 'exit 7' || echo handled",
            "if bash -c 'exit 7'; then echo unexpected; else echo handled; fi",
        ] {
            let result = exec(&s, &w, command, None);
            assert_eq!(result.exit_code, Some(0), "{result:?}");
            assert_eq!(result.output, "handled\n");
        }
        // A consumer closing a pipe early is also visible, not normalized away.
        let result = exec(&s, &w, "yes | head -c 1", None);
        assert_eq!(result.exit_code, Some(141), "SIGPIPE: {result:?}");
    }

    #[test]
    fn stdin_is_literal_even_with_shell_syntax_unicode_and_nul_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path(), &[]);
        let w = ws(&["run", "problem", "task"]);
        let content = "EOF\nPYEOF\n'\"\\ $HOME $(touch injected) `touch injected2`\ncafé €\0tail\n";
        let request = ExecRequest {
            command: "cat > saved.txt && cat saved.txt".into(),
            stdin: Some(content.into()),
            timeout_ms: None,
            max_output_bytes: None,
            ..Default::default()
        };
        let result = s.exec(&w, &request, 60_000).unwrap();
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.output, content);
        let workspace = dir.path().join("run/problem/task");
        assert_eq!(std::fs::read(workspace.join("saved.txt")).unwrap(), content.as_bytes());
        assert!(!workspace.join("injected").exists());
        assert!(!workspace.join("injected2").exists());
    }

    #[test]
    fn stdin_is_bounded_and_does_not_block_a_command_that_never_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path(), &[]);
        let w = ws(&["run", "problem", "task"]);
        let mut request = ExecRequest {
            command: "wc -c".into(),
            stdin: Some("x".repeat(MAX_STDIN_BYTES)),
            timeout_ms: None,
            max_output_bytes: None,
            ..Default::default()
        };
        let result = s.exec(&w, &request, 60_000).unwrap();
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.output.trim(), MAX_STDIN_BYTES.to_string());
        request.command = "sleep 30".into();
        request.timeout_ms = Some(1_000);
        let started = Instant::now();
        let result = s.exec(&w, &request, 60_000).unwrap();
        assert!(result.timed_out, "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        request.command = "touch should-not-exist".into();
        request.stdin = Some("€".repeat(MAX_STDIN_BYTES / 3 + 1));
        assert_eq!(s.exec(&w, &request, 60_000).unwrap_err().code, ErrorCode::InvalidInput);
        assert!(!dir.path().join("run/problem/task/should-not-exist").exists());
    }

    /// A project's own instance: `HOME` is where the projects install their
    /// tools, so a command runs what was installed there, and it writes what
    /// its user may, a sibling worktree included. The description says both,
    /// and the network fact it was started with.
    #[test]
    fn a_project_instance_runs_what_it_installed_and_writes_what_its_user_may() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("project");
        std::fs::create_dir_all(home.join("repo")).unwrap();
        let path = format!("PATH={0}/.local/bin:{0}/.cargo/bin:/usr/bin:/bin", home.display());
        let open = |network: &str| {
            let args = ["--root", home.to_str().unwrap(), "--network", network, "--env", &path];
            let home = format!("HOME={}", home.display());
            let args = args.into_iter().chain(["--env", home.as_str()]).map(str::to_owned);
            ShellService::open(&Options::parse(args).unwrap()).unwrap()
        };
        let s = open("on");
        let script = "mkdir -p ~/.local/bin && printf '#!/bin/sh\\necho installed-ran\\n' > ~/.local/bin/hello \
                      && chmod +x ~/.local/bin/hello && hello; \
                      echo commit > ../../repo/.from-worktree && echo ok-project";
        let got = exec(&s, &ws(&["worktrees", "w"]), script, None);
        for expected in ["installed-ran", "ok-project"] {
            assert!(got.output.contains(expected), "{expected} missing from: {}", got.output);
        }
        assert!(home.join("repo/.from-worktree").exists());
        let description = s.environment_description();
        assert!(description.contains(&format!("HOME is {}", home.display())), "{description}");
        assert!(description.contains("Commands have network access."), "{description}");
        assert!(open("off").environment_description().contains("Commands have no IP network"));
    }

    /// A link cannot be named as a workspace, or as a prefix of one: a call
    /// starts in the directory its capability names, not where a link in it
    /// points.
    #[test]
    fn a_workspace_path_that_crosses_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path(), &[]);
        let mine = ws(&["r", "p", "t1"]);
        let sibling = dir.path().join("r/p/t2");
        std::fs::create_dir_all(&sibling).unwrap();
        exec(&s, &mine, &format!("ln -s {} esc", sibling.display()), None);
        let write = ExecRequest { command: "echo x > stolen.txt".into(), ..Default::default() };
        for through in [ws(&["r", "p", "t1", "esc"]), ws(&["r", "p", "t1", "esc", "sub"])] {
            let got = s.exec(&through, &write, 60_000);
            assert_eq!(got.map(|_| ()).map_err(|e| e.code), Err(ErrorCode::PermissionDenied), "{through:?}");
        }
        assert!(!sibling.join("stolen.txt").exists() && !sibling.join("sub").exists());
    }

    #[test]
    fn a_command_runs_in_its_workspace_with_the_configured_environment_and_is_killed_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path(), &["--max-output-bytes", "1000"]);
        let w = ws(&["run1", "p", "t1"]);
        let got = exec(
            &s,
            &w,
            "echo out; echo err 1>&2; pwd; [ \"$HOME\" = \"$PWD\" ] && echo home-is-own; exit 3",
            None,
        );
        assert_eq!(got.exit_code, Some(3), "{got:?}");
        assert!(got.output.starts_with("out\nerr\n"), "stderr is merged: {}", got.output);
        assert_eq!((got.stopped_leftovers, &got.note), (None, &None), "it left nothing running");
        // What a command leaves running is stopped when it returns, and the
        // answer says so, whatever session or group it moved to and whatever
        // it is called (a name that is no text hides it from nothing).
        let left = exec(&s, &w, "sleep 30 & setsid sleep 30 & echo bye", None);
        assert_eq!((left.exit_code, left.stopped_leftovers), (Some(0), Some(2)), "{left:?}");
        assert!(left.note.as_deref().is_some_and(|note| note.starts_with("2 processes")), "{left:?}");
        // A script whose name is no text, and the sleep it waits for.
        let named =
            "printf '#!/bin/sh\\nsleep 30; :\\n' > $'s\\xff'; chmod +x $'s\\xff'; ./$'s\\xff' & sleep 0.5";
        assert_eq!(exec(&s, &w, named, None).stopped_leftovers, Some(2), "found whatever it is called");
        assert!(
            got.output.contains("run1/p/t1\nhome-is-own\n"),
            "the workspace is the working directory and the home: {}",
            got.output
        );
        let cut = exec(&s, &w, "head -c 200000 /dev/zero", None);
        assert!(cut.truncated && cut.output.len() == 1000 && cut.exit_code == Some(0), "{cut:?}");
        // The wall clock ends the command and everything it started.
        let started = Instant::now();
        let late = exec(&s, &w, "sleep 30 & sleep 30; echo never", Some(1_500));
        assert!(late.timed_out && !late.output.contains("never"), "{late:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        // The root is not a workspace, a segment outside the plain alphabet is refused,
        // and a deadline too short to run anything is refused before anything runs.
        let request = ExecRequest {
            command: "true".into(),
            stdin: None,
            timeout_ms: None,
            max_output_bytes: None,
            ..Default::default()
        };
        assert_eq!(
            s.exec(&ResourcePath::root(), &request, 60_000).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(s.exec(&ws(&["a b"]), &request, 60_000).unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(s.exec(&w, &request, REPLY_MARGIN_MS + 10).unwrap_err().code, ErrorCode::InvalidInput);
    }

    /// A command that prints far more than anyone will take must not fill
    /// the service's memory, which every other command and job of the
    /// instance shares: what waits to be taken is bounded, the rest is
    /// dropped and still counted.
    #[test]
    fn a_command_that_prints_without_end_does_not_fill_the_services_memory() {
        let printed = MAX_WAITING_BYTES + 1024 * 1024;
        let (chunks, collect) = mpsc::channel();
        let flow = Arc::new(Flow::default());
        spawn_reader(std::io::repeat(0).take(printed), chunks, Arc::clone(&flow));
        let (waiting, dropped) =
            (|| flow.waiting.load(Ordering::Relaxed), || flow.dropped.load(Ordering::Relaxed));
        let until = Instant::now() + Duration::from_secs(10);
        while waiting() + dropped() < printed {
            assert!(Instant::now() < until, "the reader stalled at {} bytes", waiting() + dropped());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(waiting() <= MAX_WAITING_BYTES, "nobody took any, and {} bytes wait", waiting());
        let (kept, total) = collect_output(&collect, &flow, 1000, Duration::from_millis(100));
        assert_eq!((kept.len() as u64, total), (1000, printed), "what was dropped is still counted");
    }

    /// Calls beyond the slots wait for one, oldest first, rather than being
    /// refused: one whose deadline leaves no time to run is answered
    /// `Timeout`, and one cancelled while it waits never runs. One cancelled
    /// while it runs has its command killed, so the next call starts at once.
    #[test]
    fn a_call_beyond_the_slots_waits_for_one_within_its_deadline() {
        use nucleus_protocol::{ActionName, FromKernel, ProcessId, ToKernel, read_frame, write_frame};
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("r/p/t");
        let s = Arc::new(service(dir.path(), &["--max-inflight", "1"]));
        let (from_kernel, mut kernel_out) = std::io::pipe().unwrap();
        let (mut kernel_in, to_kernel) = std::io::pipe().unwrap();
        let exec = |id, command: &str, remaining_ms| FromKernel::Dispatch {
            dispatch: DispatchId(id),
            endpoint: EndpointName::new(ENDPOINT).unwrap(),
            caller: ProcessId(2),
            action: ActionName::new(ACTION_EXEC).unwrap(),
            resource: ws(&["r", "p", "t"]),
            payload: Payload::encode(&ExecRequest {
                command: command.into(),
                stdin: None,
                timeout_ms: None,
                max_output_bytes: None,
                ..Default::default()
            })
            .unwrap(),
            remaining_ms,
        };
        write_frame(&mut kernel_out, &FromKernel::Hello { process: ProcessId(1), handles: Vec::new() })
            .unwrap();
        let (replies, withdrawn_for) = std::thread::scope(|scope| {
            let served = scope.spawn(|| serve(&s, &mut Connection::open(from_kernel, to_kernel).unwrap()));
            let mut replies = std::collections::BTreeMap::new();
            let mut answered = |ids: &[u64], replies: &mut std::collections::BTreeMap<_, _>| {
                while !ids.iter().all(|id| replies.contains_key(id)) {
                    match read_frame(&mut kernel_in) {
                        Ok(ToKernel::Reply { dispatch, result }) => replies.insert(dispatch.0, result),
                        other => panic!("{other:?}"),
                    };
                }
            };
            for frame in [
                // Holds the one slot for two seconds.
                exec(1, "sleep 2; echo first", 60_000),
                exec(2, "echo second", 60_000),
                exec(3, "touch late", 1_800),
                exec(4, "touch cancelled", 60_000),
                FromKernel::CancelDispatch { dispatch: DispatchId(4) },
                // Queued after 4, so 4 would have run before it.
                exec(5, "echo last", 60_000),
            ] {
                write_frame(&mut kernel_out, &frame).unwrap();
            }
            answered(&[1, 2, 3, 5], &mut replies);
            // Would hold the slot for half a minute; withdrawn once it runs.
            for frame in
                [exec(6, "touch started; sleep 30; touch finished", 60_000), exec(7, "echo next", 60_000)]
            {
                write_frame(&mut kernel_out, &frame).unwrap();
            }
            let started = Instant::now();
            while !workspace.join("started").exists() {
                assert!(started.elapsed() < Duration::from_secs(20), "the command never started");
                std::thread::sleep(Duration::from_millis(10));
            }
            let withdrawn = Instant::now();
            write_frame(&mut kernel_out, &FromKernel::CancelDispatch { dispatch: DispatchId(6) }).unwrap();
            answered(&[6, 7], &mut replies);
            let withdrawn_for = withdrawn.elapsed();
            write_frame(&mut kernel_out, &FromKernel::Terminate).unwrap();
            served.join().unwrap().unwrap();
            (replies, withdrawn_for)
        });
        let outcome = |id: u64| {
            replies[&id].as_ref().map(|p| p.decode::<ExecResponse>().unwrap().output).map_err(|e| e.code)
        };
        assert_eq!(outcome(2), Ok("second\n".to_owned()), "it waited for the slot");
        assert_eq!(outcome(3), Err(ErrorCode::Timeout), "its deadline passed while it waited");
        assert_eq!((outcome(1), outcome(5)), (Ok("first\n".to_owned()), Ok("last\n".to_owned())));
        assert_eq!((outcome(4), outcome(6)), (Err(ErrorCode::Cancelled), Err(ErrorCode::Cancelled)));
        assert_eq!(outcome(7), Ok("next\n".to_owned()));
        assert!(withdrawn_for < Duration::from_secs(10), "the slot passed on at once: {withdrawn_for:?}");
        for never in ["late", "cancelled", "finished"] {
            assert!(!workspace.join(never).exists(), "{never} ran");
        }
    }
}
