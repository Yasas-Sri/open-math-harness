//! Runs the kernel against real host processes.
//!
//! Identity: stdio transport creates a pair of pipes for each process it starts
//! and hands them to the child as stdin and stdout. The thread reading the
//! child's output was created for exactly one [`ProcessId`] and stamps that id on
//! every message. There is no listening socket and no way to attach to a
//! channel after the fact, so a process cannot speak as anyone else. The pipes
//! are the runtime's user's: a project's process uses the descriptors it was
//! given but cannot open them again through `/proc/<pid>/fd`. A process's
//! stderr is the supervisor's own, so any process can write lines into the
//! runtime's log.
//!
//! Locking: one mutex guards the kernel and the channel table, and outputs are
//! applied before it is released so that every process sees kernel messages in
//! the order the kernel produced them. Nothing done under the lock waits on a
//! child: sends are `try_send`, stopping a process only signals it, and the
//! waiting happens on a reaper thread. Starting a process does fork and exec
//! under the lock; that is bounded host work, not a wait on untrusted code.
//!
//! Processes: a program of the runtime's own runs in a sandbox; a project's
//! process (one the kernel started as a project) runs as the projects' Linux
//! user in none, in its project's cgroup (module `identity`).
//!
//! Spawning: every host process is forked from one dedicated thread that lives
//! as long as the supervisor. The sandbox asks the host kernel to kill it when
//! its parent dies (`--die-with-parent`), and Linux defines "parent" as the
//! *thread* that forked. Forking from whichever thread happened to handle a
//! `Spawn` message would tie a child's life to that unrelated thread and kill
//! it without the `Terminate` grace period when the thread ended.
//!
//! Slow consumers: each process has a bounded outbound queue. A process that
//! lets it fill is terminated rather than buffered for.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nucleus_kernel::{CapSpec, Config, Kernel, Namespace, NamespaceBind, Output, SecurityEvent};
use nucleus_protocol::{
    EndpointName, Error, ErrorCode, FrameError, FromKernel, Generation, IdentityRequest, ProcessId,
    ProgramName, RequestId, ToKernel, read_frame, write_frame,
};

use crate::cgroup::{CGROUP_CLEANUP_TIMEOUT, CgroupRoot, ProgramCgroup};
use crate::confinement::{Confinement, ConfinementError, ProgramSpec, detect, resolve_child};

/// Receives security events. Called with the supervisor lock held: it must
/// return promptly and must not call back into the supervisor.
pub type EventSink = Arc<dyn Fn(&SecurityEvent) + Send + Sync>;

/// Everything needed to start a supervisor.
pub struct SupervisorConfig {
    /// Kernel limits. `programs` is overwritten with the keys of
    /// [`Self::programs`] so the two cannot disagree.
    pub limits: Config,
    pub programs: BTreeMap<ProgramName, ProgramSpec>,
    /// Where `bwrap` is, if not on `PATH`.
    pub sandbox_tool: Option<PathBuf>,
    /// How long a process gets to exit after `Terminate` before it is killed.
    pub grace: Duration,
    /// Messages that may wait for one process before it counts as a slow consumer.
    pub outbound_queue: usize,
    pub tick: Duration,
    /// The setuid helper that starts a project's process as the projects'
    /// user (`nucleus-as`). Required when `limits.projects`.
    pub identity_helper: Option<PathBuf>,
    /// Start every program in a cgroup of its own and stop it with
    /// `cgroup.kill`. Required with projects, whose processes the runtime
    /// cannot signal and which each have a cgroup of their own.
    pub cgroups: bool,
    /// Most projects with processes at once, counted by their cgroups: the
    /// first process of one more is refused.
    pub max_active_projects: usize,
}

/// Why the supervisor refused to start.
#[derive(Debug)]
pub enum StartError {
    Confinement(ConfinementError),
    Config(Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Confinement(e) => write!(f, "{e}"),
            Self::Config(e) => write!(f, "invalid configuration: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// How one host process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReapReport {
    pub process: ProcessId,
    /// True when the process ignored `Terminate` for the whole grace period.
    pub killed: bool,
    /// Set when the host process could not be confirmed gone.
    pub error: Option<String>,
}

/// Result of [`Supervisor::shutdown`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    pub reaped: Vec<ReapReport>,
}

impl ShutdownReport {
    /// True when every tracked cleanup obligation completed.
    pub fn is_clean(&self) -> bool {
        self.reaped.iter().all(|r| r.error.is_none())
    }
}

struct Channel {
    outbound: SyncSender<FromKernel>,
    child: Child,
    /// The program's own cgroup, when programs run in one.
    cgroup: Option<ProgramCgroup>,
    /// The project whose cgroup holds the program's, for a project's process.
    project: Option<String>,
}

struct State {
    kernel: Kernel,
    channels: BTreeMap<ProcessId, Channel>,
    /// The resolved host spec each live process was started from, so a child
    /// started with `SpawnExecutable` can be resolved against its parent's
    /// real binds. Removed when the process stops.
    specs: BTreeMap<ProcessId, ProgramSpec>,
}

/// A command to fork on the spawner thread and where to send the result.
type SpawnJob = (Command, SyncSender<std::io::Result<Child>>);

struct Inner {
    state: Mutex<State>,
    /// `None` once shutdown has confirmed every child gone.
    spawner: Mutex<Option<Sender<SpawnJob>>>,
    programs: BTreeMap<ProgramName, ProgramSpec>,
    sandbox: Box<dyn Confinement>,
    identity_helper: Option<PathBuf>,
    cgroups: Option<CgroupRoot>,
    max_active_projects: usize,
    started: Instant,
    grace: Duration,
    outbound_queue: usize,
    sink: EventSink,
    stopping: AtomicBool,
    reapers: Mutex<Vec<JoinHandle<ReapReport>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// Owns the kernel and every host process. Dropping it shuts everything down.
pub struct Supervisor {
    inner: Arc<Inner>,
    ticker: Option<JoinHandle<()>>,
    spawner: Option<JoinHandle<()>>,
}

// The kernel never panics by construction, so a poisoned lock can only come
// from a panicking event sink; kernel state is still consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn namespace(spec: &ProgramSpec) -> Namespace {
    Namespace {
        binds: spec
            .binds
            .iter()
            .map(|b| NamespaceBind {
                path: b.guest.to_string_lossy().into_owned(),
                source: b.host.clone(),
                writable: b.writable,
            })
            .collect(),
        network: spec.network,
    }
}

pub(crate) const PROCESS_GROUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Observe an already terminated launcher's retained group without signaling
/// a numeric ID that might have been reused after its last member exited.
pub(crate) fn confirm_process_group_cleanup(group: u32) -> Result<(), String> {
    let pid = i32::try_from(group)
        .ok()
        .filter(|pid| *pid > 1)
        .and_then(rustix::process::Pid::from_raw)
        .ok_or("invalid host process group")?;
    let deadline = Instant::now() + PROCESS_GROUP_CLEANUP_TIMEOUT;
    loop {
        match rustix::process::test_kill_process_group(pid) {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Err(error) => return Err(format!("cannot confirm host process group cleanup: {error}")),
            Ok(()) if !host_group_has_live_members(group)? => return Ok(()),
            Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(()) => return Err("host process group cleanup remains unconfirmed".into()),
        }
    }
}

// kill(0) also sees zombies, whose resources and IPC are already closed but
// whose new parent may not yet have collected their exit status. Do not wait
// indefinitely for an unrelated init process to reap them.
pub(crate) fn host_group_has_live_members(group: u32) -> Result<bool, String> {
    let entries = std::fs::read_dir("/proc").map_err(|e| format!("cannot inspect host processes: {e}"))?;
    host_group_has_live_members_in(
        group,
        entries.map(|entry| {
            let entry = entry.map_err(|e| format!("cannot inspect host processes: {e}"))?;
            if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                return Ok(None);
            }
            host_process_stat(std::fs::read_to_string(entry.path().join("stat")))
        }),
    )
}

fn host_process_stat(result: std::io::Result<String>) -> Result<Option<String>, String> {
    match result {
        Ok(stat) => Ok(Some(stat)),
        // A task can disappear before open (ENOENT) or after its proc stat
        // descriptor opens (ESRCH). Only that task is absent: keep scanning
        // the other entries before concluding that the group has no members.
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("cannot inspect host process state: {e}")),
    }
}

fn host_group_has_live_members_in(
    group: u32,
    stats: impl IntoIterator<Item = Result<Option<String>, String>>,
) -> Result<bool, String> {
    for stat in stats {
        let Some(stat) = stat? else { continue };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            return Err("invalid host process state".into());
        };
        let mut fields = fields.split_whitespace();
        let state = fields.next();
        let _parent = fields.next();
        let process_group = fields.next().and_then(|id| id.parse::<u32>().ok());
        if process_group == Some(group) && !matches!(state, Some("Z" | "X")) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) fn wait_for_test_watchdog(launcher: u32) -> Result<u32, String> {
    // spawn can finish after the ready byte but before the watcher closes its
    // readiness FD. Wait for the final FD layout rather than sampling once.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let children = std::fs::read_to_string(format!("/proc/{launcher}/task/{launcher}/children"))
            .map_err(|error| format!("cannot inspect test launcher {launcher}: {error}"))?;
        if let Some(watcher) =
            children.split_whitespace().filter_map(|pid| pid.parse::<u32>().ok()).find(|pid| {
                std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|entries| {
                    let targets: Result<Vec<_>, _> = entries
                        .map(|entry| entry.and_then(|entry| std::fs::read_link(entry.path())))
                        .collect();
                    targets.is_ok_and(|targets| {
                        targets.len() == 2
                            && targets
                                .iter()
                                .all(|target| target == std::path::Path::new("anon_inode:[pidfd]"))
                    })
                })
            })
        {
            return Ok(watcher);
        }
        if Instant::now() >= deadline {
            return Err(format!("test launcher {launcher} has no ready two-pidfd watchdog"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// How long a project's processes get to end once its cgroup was killed.
const PROJECT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

impl Inner {
    fn now(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Carries out kernel outputs until none remain. Called with the lock held.
    /// A program that cannot be started is reported to the kernel as an exit:
    /// its spawner receives `ChildExited`, and [`Supervisor::boot`] answers
    /// `unavailable`. Nothing retries it.
    fn apply(self: &Arc<Self>, state: &mut State) {
        loop {
            let outputs = state.kernel.drain_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                match output {
                    Output::Event(event) => (self.sink)(&event),
                    Output::StartProcess { process, parent, program } => {
                        let started = self
                            .programs
                            .get(&program)
                            .ok_or_else(|| format!("unknown program {program}"))
                            .and_then(|spec| {
                                crate::confinement::with_resolved_paths(spec).map_err(|e| e.to_string())
                            })
                            .and_then(|spec| {
                                if let Some(parent) = parent {
                                    let parent_spec = state
                                        .specs
                                        .get(&parent)
                                        .ok_or("the spawning process has no known namespace")?;
                                    namespace(parent_spec)
                                        .admits(&namespace(&spec))
                                        .map_err(|e| e.to_string())?;
                                }
                                self.start_process(process, &spec, None).map(|channel| (spec, channel))
                            });
                        match started {
                            Ok((spec, channel)) => {
                                state.specs.insert(process, spec);
                                state.channels.insert(process, channel);
                            }
                            Err(why) => {
                                crate::diagnostics::emit(format_args!(
                                    "nucleus: cannot start {program} as process {process}: {why}"
                                ));
                                state.kernel.process_exited(process, self.now());
                            }
                        }
                    }
                    Output::StartExecutable { process, parent, program, identity } => {
                        // Resolve the child's guest paths against the parent's real
                        // binds; a path or symlink that escapes is refused here.
                        let started = state
                            .specs
                            .get(&parent)
                            .ok_or_else(|| "the spawning process has no known namespace".to_owned())
                            .and_then(|parent_spec| {
                                // A project's process is in no sandbox: its
                                // children are named by the VM's own paths, and
                                // it is the VM's own paths they see.
                                if identity.is_some() && parent_spec.binds == crate::identity::whole_vm() {
                                    return Ok(crate::identity::child_of_project(&program));
                                }
                                let mut spec =
                                    resolve_child(parent_spec, &program).map_err(|e| e.to_string())?;
                                if identity.is_some() {
                                    spec.binds = crate::identity::whole_vm();
                                }
                                Ok(spec)
                            })
                            .and_then(|spec| {
                                self.start_process(process, &spec, identity.as_deref())
                                    .map(|channel| (spec, channel))
                            });
                        match started {
                            Ok((spec, channel)) => {
                                state.specs.insert(process, spec);
                                state.channels.insert(process, channel);
                            }
                            Err(why) => {
                                crate::diagnostics::emit(format_args!(
                                    "nucleus: cannot start process {process} from {}: {why}",
                                    program.executable
                                ));
                                state.kernel.process_exited(process, self.now());
                            }
                        }
                    }
                    Output::Send { to, msg } => {
                        let Some(channel) = state.channels.get(&to) else { continue };
                        match channel.outbound.try_send(msg) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                crate::diagnostics::emit(format_args!(
                                    "nucleus: process {to} is not reading; terminating it"
                                ));
                                state.kernel.process_exited(to, self.now());
                            }
                            // A duplex reader can remain open after its writer
                            // fails. Never leave that principal apparently live
                            // while silently discarding every response.
                            Err(TrySendError::Disconnected(_)) => {
                                state.kernel.process_exited(to, self.now());
                            }
                        }
                    }
                    Output::StopProcess { process } => {
                        state.specs.remove(&process);
                        match state.channels.remove(&process) {
                            Some(channel) => self.reap(process, channel),
                            None => state.kernel.process_reaped(process, false),
                        }
                    }
                    Output::StopProject { process, request, project } => {
                        self.stop_project(process, request, project);
                    }
                }
            }
        }
    }

    /// Starts `process` from `spec`, as the project `identity` names when it
    /// names one. Called with the lock held, which every making and removing
    /// of a project's cgroup takes.
    fn start_process(
        self: &Arc<Self>,
        process: ProcessId,
        spec: &ProgramSpec,
        identity: Option<&str>,
    ) -> Result<Channel, String> {
        // Fails closed: a process the kernel started as a project never runs
        // as the runtime's own, and never without its project's cgroup, which
        // is the only way the runtime can stop another user's process.
        let helper = match (identity, &self.identity_helper) {
            (None, _) => None,
            (Some(_), Some(helper)) if self.cgroups.is_some() => Some(helper),
            (Some(project), _) => {
                return Err(format!("this runtime cannot start a process of project {project}"));
            }
        };
        let name = format!("p{}", process.0);
        let cgroup = match (&self.cgroups, identity) {
            (Some(root), Some(project)) => {
                let made = root.project(project)?.create(&name);
                if made.is_err() {
                    let _ = root.project_cgroup(project).remove_if_unused();
                }
                Some(made?)
            }
            (Some(root), None) => Some(root.create(&name)?),
            (None, _) => None,
        };
        let started = (|| {
            let files = match &cgroup {
                Some(cgroup) => Some((cgroup.procs()?, cgroup.kill_file()?)),
                None => None,
            };
            let mut command = match (helper, files) {
                (Some(helper), Some(files)) => crate::identity::command(helper, spec, files),
                (_, files) => self.sandbox.launch(spec, files),
            }
            .map_err(|e| e.to_string())?;
            command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
            self.spawn_on_spawner(command)
        })();
        let mut child = match started {
            Ok(child) => child,
            Err(why) => {
                // Nothing ran in it; the empty cgroup goes now.
                if let Some(cgroup) = &cgroup {
                    let _ = cgroup.remove();
                }
                self.project_ended(identity);
                return Err(why);
            }
        };
        let (Some(writer), Some(reader)) = (child.stdin.take(), child.stdout.take()) else {
            match &cgroup {
                Some(cgroup) => drop(cgroup.kill()),
                None => drop(child.kill()),
            }
            return Err("the child's channel could not be opened".to_owned());
        };
        let (outbound, queue) = sync_channel(self.outbound_queue);
        let mut workers = lock(&self.workers);
        workers.retain(|worker| !worker.is_finished());
        let inner = Arc::clone(self);
        workers.push(thread::spawn(move || inner.read_loop(process, reader)));
        let inner = Arc::clone(self);
        workers.push(thread::spawn(move || inner.write_loop(process, queue, writer)));
        Ok(Channel { outbound, child, cgroup, project: identity.map(str::to_owned) })
    }

    /// A program of `project` is gone: its project's cgroup goes too if
    /// nothing else of the project is in it. Called with the lock held.
    fn project_ended(&self, project: Option<&str>) {
        if let (Some(root), Some(project)) = (&self.cgroups, project)
            && let Err(why) = root.project_cgroup(project).remove_if_unused()
        {
            crate::diagnostics::emit(format_args!("nucleus: {why}"));
        }
    }

    /// The first process of one project more than the configured number is
    /// refused before the kernel admits it, counting the projects' cgroups;
    /// a process of a project that has processes always starts. Called with
    /// the lock held, so no project's cgroup is made or removed meanwhile.
    fn room_for(&self, msg: &ToKernel) -> Result<(), (RequestId, Error)> {
        let ToKernel::SpawnExecutable { request, identity: Some(IdentityRequest { project, .. }), .. } = msg
        else {
            return Ok(());
        };
        let Some(root) = self.cgroups.as_ref().filter(|_| nucleus_protocol::valid_project(project)) else {
            return Ok(());
        };
        root.room_for_project(project, self.max_active_projects)
            .map_err(|why| (*request, Error::new(ErrorCode::LimitExceeded, why)))
    }

    fn spawn_on_spawner(&self, command: Command) -> Result<Child, String> {
        let (result, spawned) = sync_channel(1);
        match &*lock(&self.spawner) {
            Some(jobs) => jobs.send((command, result)).map_err(|_| "spawner thread is gone".to_owned())?,
            None => return Err("supervisor has shut down".to_owned()),
        }
        spawned.recv().map_err(|_| "spawner thread is gone".to_owned())?.map_err(|e| e.to_string())
    }

    /// Reads one process's channel. `process` is fixed for the life of the
    /// thread; nothing in a message can change whose authority is exercised.
    fn read_loop(self: Arc<Self>, process: ProcessId, channel: impl std::io::Read) {
        let mut reader = BufReader::new(channel);
        let mut ordinary = false;
        loop {
            match read_frame::<_, ToKernel>(&mut reader) {
                Ok(msg) => {
                    let mut state = lock(&self.state);
                    // A closing process's requests are the kernel's to refuse.
                    let room = if state.kernel.is_live(process) { self.room_for(&msg) } else { Ok(()) };
                    match room {
                        Ok(()) => state.kernel.handle_message(process, msg, self.now()),
                        Err((request, error)) => state.kernel.refuse(process, request, error),
                    }
                    self.apply(&mut state);
                }
                Err(FrameError::Closed) => {
                    ordinary = true;
                    break;
                }
                Err(FrameError::Io(_)) => break,
                Err(
                    error @ (FrameError::TooLarge(_)
                    | FrameError::Malformed(_)
                    | FrameError::UnsupportedVersion(_)),
                ) => {
                    let reason = match error {
                        FrameError::TooLarge(_) => "oversized_frame",
                        FrameError::UnsupportedVersion(_) => "unsupported_version",
                        _ => "malformed_frame",
                    };
                    crate::diagnostics::emit_terminal(format_args!(
                        "nucleus: local IPC protocol failure: process={process} reason={reason}"
                    ));
                    // The stream can no longer be trusted to be in sync.
                    (self.sink)(&SecurityEvent::ProtocolViolation { process, detail: "unreadable frame" });
                    break;
                }
            }
        }
        let mut state = lock(&self.state);
        if ordinary {
            state.kernel.process_disconnected(process, self.now());
        } else {
            state.kernel.process_exited(process, self.now());
        }
        self.apply(&mut state);
    }

    /// A failed write ends this channel's principal even if its independent
    /// reader is still open. Otherwise a duplex peer can keep sending requests
    /// whose replies disappear into a dead writer. Normal queue closure after
    /// teardown is not a transport failure and does not start another teardown.
    fn write_loop(self: Arc<Self>, process: ProcessId, queue: Receiver<FromKernel>, mut socket: impl Write) {
        for msg in queue {
            if let Err(error) = write_frame(&mut socket, &msg) {
                let (reason, os_error) = match error {
                    FrameError::TooLarge(_) => ("oversized_frame", None),
                    FrameError::Io(error) => ("io_error", error.raw_os_error()),
                    _ => ("invalid_frame", None),
                };
                let mut state = lock(&self.state);
                if state.kernel.is_live(process) {
                    // Fixed metadata only: errors can contain serialized data,
                    // so do not log the message or the frame being written.
                    crate::diagnostics::emit_terminal(format_args!(
                        "nucleus: local IPC write failure: process={process} reason={reason} os_error={os_error:?}"
                    ));
                    state.kernel.process_exited(process, self.now());
                    self.apply(&mut state);
                }
                break;
            }
        }
    }

    /// Kills every process of `project` with its cgroup, off the lock, and
    /// tells the kernel once none is left, or why not. Each process ended so
    /// is torn down as one that exited.
    fn stop_project(self: &Arc<Self>, process: ProcessId, request: RequestId, project: String) {
        let inner = Arc::clone(self);
        let worker = thread::spawn(move || {
            let result = match &inner.cgroups {
                None => Err(Error::new(ErrorCode::Unavailable, "this runtime runs no project's processes")),
                Some(root) => {
                    let cgroup = root.project_cgroup(&project);
                    let killed = match cgroup.kill() {
                        // No cgroup: nothing of the project runs.
                        Err(_) if !cgroup.exists() => Ok(()),
                        killed => killed,
                    };
                    killed.and_then(|()| cgroup.emptied(Instant::now() + PROJECT_STOP_TIMEOUT)).map_err(
                        |why| Error::new(ErrorCode::ServiceError, format!("stopping {project}: {why}")),
                    )
                }
            };
            let mut state = lock(&inner.state);
            state.kernel.project_stopped(process, request, result);
            inner.apply(&mut state);
        });
        let mut workers = lock(&self.workers);
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
    }

    /// Asks the process to exit, then makes sure it has, off the lock.
    fn reap(self: &Arc<Self>, process: ProcessId, channel: Channel) {
        let grace = self.grace;
        let inner = Arc::clone(self);
        let reaper = thread::spawn(move || {
            let Channel { outbound, mut child, cgroup, project } = channel;
            // Best effort: a full queue or dead writer just means it gets killed.
            let _ = outbound.try_send(FromKernel::Terminate);
            let deadline = Instant::now() + grace;
            let mut report = ReapReport { process, killed: false, error: None };
            let mut success = false;
            let group = child.id();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        success = status.success();
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Ok(None) => {
                        // The sandbox owns its PID namespace; killing the
                        // wrapper ends everything inside it. In a cgroup the
                        // cgroup is killed instead: a project's process runs
                        // as the projects' user, which no signal from here
                        // reaches, and in no PID namespace.
                        report.killed = true;
                        let killed = match &cgroup {
                            Some(cgroup) => cgroup.kill(),
                            None => child.kill().map_err(|e| e.to_string()),
                        };
                        if let Err(e) =
                            killed.and_then(|()| child.wait().map(|_| ()).map_err(|e| e.to_string()))
                        {
                            report.error = Some(format!("kill failed: {e}"));
                        }
                        break;
                    }
                    Err(e) => {
                        report.error = Some(format!("wait failed: {e}"));
                        break;
                    }
                }
            }
            // Bubblewrap and its non-execing watchdog share a dedicated group
            // (and, with cgroups, a cgroup). Waiting on the wrapper alone can
            // precede the watchdog's kill of namespace init. Confirm that no
            // member executes before reporting cleanup; in a cgroup, whatever is
            // left of a program that ended goes with it.
            let confirmed = match &cgroup {
                Some(cgroup) => cgroup
                    .kill()
                    .and_then(|()| cgroup.remove_when_empty(Instant::now() + CGROUP_CLEANUP_TIMEOUT)),
                None => confirm_process_group_cleanup(group),
            };
            if let Err(e) = confirmed {
                report.error.get_or_insert(e);
            }
            // With the process gone its pipe ends are closed, which ends the
            // reader; dropping the queue ends the writer.
            drop(outbound);
            if let Some(why) = &report.error {
                crate::diagnostics::emit_terminal(format_args!(
                    "nucleus: process {process} may still be running: {why}"
                ));
            }
            {
                let mut state = lock(&inner.state);
                if report.error.is_none() {
                    inner.project_ended(project.as_deref());
                    state.kernel.process_reaped(process, success && !report.killed);
                }
                inner.apply(&mut state);
            }
            report
        });
        lock(&self.reapers).push(reaper);
    }
}

impl Supervisor {
    /// Verifies the sandbox, then starts the clock. Confinement is required:
    /// a missing or broken backend fails startup, never falls back.
    pub fn start(mut config: SupervisorConfig, sink: EventSink) -> Result<Self, StartError> {
        crate::diagnostics::initialize();
        let sandbox = detect(config.sandbox_tool.as_deref()).map_err(StartError::Confinement)?;
        for spec in config.programs.values_mut() {
            // Freeze validated backing paths before describing them to the kernel.
            *spec = crate::confinement::with_resolved_paths(spec).map_err(StartError::Confinement)?;
            sandbox.command(spec).map_err(StartError::Confinement)?;
        }
        if config.outbound_queue == 0 || config.tick.is_zero() {
            return Err(StartError::Config(Error::new(
                nucleus_protocol::ErrorCode::InvalidInput,
                "outbound_queue and tick must be non-zero",
            )));
        }
        // Projects need the helper and cgroups, or a project's process would
        // either not start or not stop: both are checked before anything runs.
        if config.limits.projects {
            let helper = config.identity_helper.as_deref().ok_or_else(|| {
                StartError::Config(Error::new(
                    nucleus_protocol::ErrorCode::InvalidInput,
                    "projects need the identity helper",
                ))
            })?;
            crate::identity::check_helper(helper)
                .map_err(|why| StartError::Confinement(ConfinementError::Unavailable(why)))?;
            if !config.cgroups || config.max_active_projects == 0 {
                return Err(StartError::Config(Error::new(
                    nucleus_protocol::ErrorCode::InvalidInput,
                    "projects need a cgroup per program and room for at least one active project",
                )));
            }
        }
        let cgroups = if config.cgroups {
            Some(
                CgroupRoot::open()
                    .map_err(|why| StartError::Confinement(ConfinementError::Unavailable(why)))?,
            )
        } else {
            None
        };
        let mut limits = config.limits;
        limits.programs = config.programs.keys().cloned().collect();
        // Tell the kernel what each program can see, so it can attenuate a
        // child's namespace on either spawn path, comparing backing identities.
        limits.namespaces =
            config.programs.iter().map(|(name, spec)| (name.clone(), namespace(spec))).collect();
        let kernel = Kernel::new(limits).map_err(StartError::Config)?;

        let (jobs, queue) = channel::<SpawnJob>();
        let spawner = thread::spawn(move || {
            for (mut command, result) in queue {
                let _ = result.send(command.spawn());
            }
        });
        let inner = Arc::new(Inner {
            state: Mutex::new(State { kernel, channels: BTreeMap::new(), specs: BTreeMap::new() }),
            spawner: Mutex::new(Some(jobs)),
            programs: config.programs,
            sandbox,
            identity_helper: config.identity_helper,
            cgroups,
            max_active_projects: config.max_active_projects,
            started: Instant::now(),
            grace: config.grace,
            outbound_queue: config.outbound_queue,
            sink,
            stopping: AtomicBool::new(false),
            reapers: Mutex::new(Vec::new()),
            workers: Mutex::new(Vec::new()),
        });
        let ticking = Arc::clone(&inner);
        let ticker = thread::spawn(move || {
            let mut previous_poll = Instant::now();
            let mut previous_work = Duration::ZERO;
            let mut last_delay_record = None::<Instant>;
            while !ticking.stopping.load(Ordering::Relaxed) {
                thread::sleep(config.tick);
                let acquiring = Instant::now();
                let mut state = lock(&ticking.state);
                let poll_started = Instant::now();
                let observation_gap = poll_started.duration_since(previous_poll);
                if observation_gap > config.tick.saturating_mul(4).max(Duration::from_secs(1))
                    && last_delay_record.is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
                {
                    // Fixed metadata only, emitted after the observer can run
                    // again. This distinguishes a resident observation stall
                    // from proof that its VM or network went offline.
                    crate::diagnostics::emit_terminal(format_args!(
                        "nucleus: supervisor observation delayed: {}",
                        serde_json::json!({
                            "v": 1, "elapsed_ms": ticking.now(),
                            "observation_gap_ms": observation_gap.as_millis(),
                            "state_lock_wait_ms": poll_started.duration_since(acquiring).as_millis(),
                            "previous_work_ms": previous_work.as_millis(),
                            "diagnostics": crate::diagnostics::snapshot(),
                        })
                    ));
                    last_delay_record = Some(poll_started);
                }
                previous_poll = poll_started;
                state.kernel.tick(ticking.now());
                ticking.apply(&mut state);
                previous_work = poll_started.elapsed();
            }
        });
        Ok(Self { inner, ticker: Some(ticker), spawner: Some(spawner) })
    }

    /// Starts a top-level process with root capabilities. Operator authority.
    pub fn boot(&self, program: ProgramName, grants: Vec<CapSpec>) -> Result<ProcessId, Error> {
        let mut state = lock(&self.inner.state);
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(Error::new(nucleus_protocol::ErrorCode::Unavailable, "supervisor is shutting down"));
        }
        let process = state.kernel.boot_process(program, grants, self.inner.now())?;
        self.inner.apply(&mut state);
        if state.kernel.is_live(process) {
            Ok(process)
        } else {
            Err(Error::new(nucleus_protocol::ErrorCode::Unavailable, "host process could not be started"))
        }
    }

    /// Terminates a process and everything it owns. Operator authority.
    pub fn terminate(&self, process: ProcessId) {
        let mut state = lock(&self.inner.state);
        state.kernel.process_exited(process, self.inner.now());
        self.inner.apply(&mut state);
    }

    pub fn is_live(&self, process: ProcessId) -> bool {
        lock(&self.inner.state).kernel.is_live(process)
    }

    /// Positive cleanup evidence for an identity allocated by this supervisor.
    /// Logical absence, an unknown ID and an elapsed deadline do not suffice.
    pub fn cleanup_confirmed(&self, process: ProcessId) -> bool {
        lock(&self.inner.state).kernel.cleanup_confirmed(process)
    }

    pub fn live_count(&self) -> usize {
        lock(&self.inner.state).kernel.process_count()
    }

    pub fn provider_of(&self, endpoint: &EndpointName) -> Option<(ProcessId, Generation)> {
        lock(&self.inner.state).kernel.provider_of(endpoint)
    }

    /// Host PID of the process started for `process` (the sandbox wrapper,
    /// or the identity helper), for failure-injection tests.
    pub fn host_pid(&self, process: ProcessId) -> Option<u32> {
        lock(&self.inner.state).channels.get(&process).map(|c| c.child.id())
    }

    /// Polls `condition` until it holds or `timeout` passes.
    pub fn wait_until(&self, timeout: Duration, condition: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Withdraws authority immediately, then drains physical cleanup from
    /// logical leaves toward their owners. A finite deadline reports unresolved
    /// obligations; it never converts a timeout into cleanup evidence or stops
    /// a waiting parent early. Safe to call again to observe later progress.
    pub fn shutdown(&mut self) -> ShutdownReport {
        self.inner.stopping.store(true, Ordering::Release);
        let count = {
            let mut state = lock(&self.inner.state);
            state.kernel.begin_shutdown(self.inner.now());
            self.inner.apply(&mut state);
            u32::try_from(state.kernel.closing_processes().len()).unwrap_or(u32::MAX)
        };
        // A chain may consume one grace period per process.
        let deadline = Instant::now() + self.inner.grace.saturating_mul(count) + Duration::from_secs(2);
        let mut report = ShutdownReport::default();
        loop {
            // Reaping a leaf can schedule its parent on a NEW reaper thread.
            // Never snapshot-and-join only the initial generation of threads.
            let finished = {
                let mut reapers = lock(&self.inner.reapers);
                let (finished, pending): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut *reapers).into_iter().partition(JoinHandle::is_finished);
                *reapers = pending;
                finished
            };
            for reaper in finished {
                match reaper.join() {
                    Ok(reaped) => report.reaped.push(reaped),
                    Err(_) => report.reaped.push(ReapReport {
                        process: ProcessId(0),
                        killed: false,
                        error: Some("reaper thread panicked".to_owned()),
                    }),
                }
            }
            let state = lock(&self.inner.state);
            let processes_done = state.kernel.closing_processes().is_empty()
                && state.channels.is_empty()
                && lock(&self.inner.reapers).is_empty();
            if processes_done || Instant::now() >= deadline {
                for process in state.kernel.closing_processes() {
                    report.reaped.push(ReapReport {
                        process,
                        killed: false,
                        error: Some("subtree cleanup unconfirmed; ownership reservation retained".into()),
                    });
                }
                break;
            }
            drop(state);
            thread::sleep(Duration::from_millis(5));
        }
        loop {
            let mut workers = lock(&self.inner.workers);
            let (finished, pending): (Vec<_>, Vec<_>) =
                std::mem::take(&mut *workers).into_iter().partition(JoinHandle::is_finished);
            *workers = pending;
            let done = workers.is_empty();
            drop(workers);
            for worker in finished {
                let _ = worker.join();
            }
            if done || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if self.ticker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(ticker) = self.ticker.take()
        {
            let _ = ticker.join();
        }
        let cleanup_complete = {
            let state = lock(&self.inner.state);
            state.kernel.closing_processes().is_empty() && state.channels.is_empty()
        };
        if cleanup_complete {
            // Only confirmed cleanup releases the spawner: Linux ties host
            // children to this thread through --die-with-parent/PDEATHSIG.
            lock(&self.inner.spawner).take();
            if let Some(spawner) = self.spawner.take() {
                let _ = spawner.join();
            }
        }
        report
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let report = self.shutdown();
        if !report.is_clean() {
            crate::diagnostics::emit_terminal(format_args!(
                "nucleus: shutdown could not confirm every process gone: {report:?}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn config(spec: ProgramSpec) -> SupervisorConfig {
        SupervisorConfig {
            limits: Config::with_programs(Default::default()),
            programs: BTreeMap::from([(ProgramName::new("fixture").unwrap(), spec)]),
            sandbox_tool: None,
            grace: Duration::from_millis(20),
            outbound_queue: 16,
            tick: Duration::from_millis(5),
            identity_helper: None,
            cgroups: false,
            max_active_projects: 96,
        }
    }

    /// A runtime configured with projects does not start unless the helper
    /// is a root-owned setuid file: otherwise a project's program would run as
    /// the runtime.
    #[test]
    fn projects_need_an_installed_helper() {
        let spec =
            ProgramSpec { executable: "/bin/true".into(), args: vec![], binds: vec![], network: false };
        let not_setuid = tempfile::NamedTempFile::new().unwrap();
        for (helper, cgroups) in [(None, true), (Some(not_setuid.path().to_owned()), true)] {
            let mut config = config(spec.clone());
            config.limits.projects = true;
            config.identity_helper = helper.clone();
            config.cgroups = cgroups;
            assert!(Supervisor::start(config, Arc::new(|_| {})).is_err(), "{helper:?} {cgroups}");
        }
    }

    /// With cgroups the program's first process is in its cgroup before it
    /// executes, a stop kills the whole cgroup, and cleanup is confirmed only
    /// once the cgroup is empty and gone.
    #[test]
    #[ignore = "needs a cgroup this user may create children in (a systemd user session, or Delegate=yes)"]
    fn a_program_in_its_own_cgroup_is_stopped_with_all_it_started() {
        let spec = ProgramSpec {
            executable: "/usr/bin/sh".into(),
            args: vec!["-c".into(), "setsid sleep 300 & exec sleep 300".into()],
            binds: vec![],
            network: false,
        };
        let mut config = config(spec);
        config.cgroups = true;
        let mut supervisor = Supervisor::start(config, Arc::new(|_| {})).unwrap();
        let process = supervisor.boot(ProgramName::new("fixture").unwrap(), vec![]).unwrap();
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let own = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap().trim_start_matches('/').to_owned();
        let cgroup = std::path::Path::new("/sys/fs/cgroup").join(own).join(format!("p{}", process.0));
        let procs = || std::fs::read_to_string(cgroup.join("cgroup.procs")).map(|p| p.lines().count());
        assert!(supervisor.wait_until(Duration::from_secs(5), |_| procs().is_ok_and(|n| n >= 4)));
        supervisor.terminate(process);
        assert!(supervisor.wait_until(Duration::from_secs(5), |s| s.cleanup_confirmed(process)));
        assert!(!cgroup.exists(), "the cgroup is removed once empty");
        assert!(supervisor.shutdown().is_clean());
    }

    #[test]
    fn a_missing_backend_fails_startup() {
        let spec =
            ProgramSpec { executable: "/bin/true".into(), args: vec![], binds: vec![], network: false };
        let mut config = config(spec);
        config.sandbox_tool = Some("/missing/bwrap".into());
        assert!(matches!(Supervisor::start(config, Arc::new(|_| {})), Err(StartError::Confinement(_))));
    }

    #[test]
    fn confined_cleanup_waits_for_the_stopped_watchdog_after_its_wrapper_exits() {
        use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
        struct Resume(std::os::fd::OwnedFd);
        impl Drop for Resume {
            fn drop(&mut self) {
                let _ = pidfd_send_signal(&self.0, Signal::CONT);
            }
        }
        let spec = ProgramSpec {
            executable: "/usr/bin/sleep".into(),
            args: vec!["60".into()],
            binds: Vec::new(),
            network: false,
        };
        let mut supervisor = Supervisor::start(config(spec), Arc::new(|_| {})).unwrap();
        let process = supervisor.boot(ProgramName::new("fixture").unwrap(), vec![]).unwrap();
        let group = supervisor.host_pid(process).unwrap();
        let watcher = wait_for_test_watchdog(group).unwrap();
        let resume = Resume(pidfd_open(Pid::from_raw(watcher as i32).unwrap(), PidfdFlags::empty()).unwrap());
        pidfd_send_signal(&resume.0, Signal::STOP).unwrap();
        let stopped = || {
            std::fs::read_to_string(format!("/proc/{watcher}/stat"))
                .is_ok_and(|stat| stat.rsplit_once(')').unwrap().1.trim_start().starts_with('T'))
        };
        assert!(supervisor.wait_until(Duration::from_secs(3), |_| stopped()));
        supervisor.terminate(process);
        assert!(supervisor.wait_until(Duration::from_secs(3), |_| {
            !std::path::Path::new(&format!("/proc/{group}")).exists()
        }));
        assert!(
            !supervisor.wait_until(Duration::from_millis(100), |s| s.cleanup_confirmed(process)),
            "reaping the wrapper is insufficient while its watchdog remains stopped"
        );
        assert!(host_group_has_live_members(group).unwrap());
        pidfd_send_signal(&resume.0, Signal::CONT).unwrap();
        assert!(supervisor.wait_until(Duration::from_secs(3), |s| s.cleanup_confirmed(process)));
        assert!(!host_group_has_live_members(group).unwrap());
        assert!(supervisor.shutdown().is_clean());
    }

    #[test]
    fn group_confirmation_times_out_without_signaling_live_processes() {
        use std::os::unix::process::CommandExt;
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child =
            ChildGuard(Command::new("/usr/bin/sleep").arg("60").process_group(0).spawn().unwrap());
        assert!(confirm_process_group_cleanup(child.0.id()).is_err());
        assert!(child.0.try_wait().unwrap().is_none(), "confirmation must only observe the group");
    }

    #[test]
    fn a_proc_stat_reader_that_outlives_its_process_does_not_hide_a_live_group_member() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut departed = ChildGuard(Command::new("/usr/bin/sleep").arg("60").spawn().unwrap());
        let departed_path = format!("/proc/{}/stat", departed.0.id());
        // read_to_string opens then reads. Hold exactly that descriptor over
        // process exit to exercise the real procfs ESRCH, without timing luck.
        let mut stat = std::fs::File::open(&departed_path).unwrap();
        departed.0.kill().unwrap();
        departed.0.wait().unwrap();
        let mut read_departed = || {
            let mut value = String::new();
            let error = stat.read_to_string(&mut value).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(rustix::io::Errno::SRCH.raw_os_error()));
            host_process_stat(Err(error))
        };
        let mut live = ChildGuard(Command::new("/usr/bin/sleep").arg("60").process_group(0).spawn().unwrap());
        let group = live.0.id();
        let live_stat = std::fs::read_to_string(format!("/proc/{group}/stat")).unwrap();
        assert!(
            host_group_has_live_members_in(group, [read_departed(), host_process_stat(Ok(live_stat))],)
                .unwrap(),
            "one vanished proc record must not be mistaken for an empty group"
        );
        assert!(host_group_has_live_members(group).unwrap());
        assert!(!host_group_has_live_members_in(group, [read_departed()]).unwrap());
        assert_eq!(host_process_stat(std::fs::read_to_string(departed_path)).unwrap(), None);
        live.0.kill().unwrap();
        live.0.wait().unwrap();
        assert!(!host_group_has_live_members(group).unwrap());
    }

    #[test]
    fn proc_stat_inspection_errors_still_fail_closed() {
        for error in [rustix::io::Errno::ACCESS, rustix::io::Errno::PERM, rustix::io::Errno::IO] {
            let stat = host_process_stat(Err(std::io::Error::from_raw_os_error(error.raw_os_error())));
            assert!(host_group_has_live_members_in(42, [stat]).is_err(), "{error:?}");
        }
        assert!(host_group_has_live_members_in(42, [Ok(Some("malformed stat".into()))]).is_err());
        assert!(host_group_has_live_members_in(42, [Err("cannot inspect host processes".into())]).is_err());
    }
}
