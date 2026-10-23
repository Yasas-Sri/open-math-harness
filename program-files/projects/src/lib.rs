//! `nucleus-projects`: the projects service.
//!
//! It runs as the runtime's own identity, in its sandbox, and reads no
//! project's files. It provides the endpoints the window and the session
//! agents call about projects (`projects`, `files`, `diffs`, and
//! the project-scoped `fs`, `shell`, `pdf`, `skills` and `jobs`), and the shell of a project's
//! discovery workers (`prover-shell` without network, `prover-shell-net`
//! with; in the folder's `runs/`), each call
//! naming its project in its resource, and forwards every call to that
//! project's host for the interface: an ordinary service program started on
//! demand as the project, an ordinary process of the projects' Linux user in
//! no sandbox, in the project's cgroup (`hosts.rs`, `router.rs`). The
//! projects directory is the registry (`registry.rs`).
//!
//! # Authority
//!
//! `kernel provide` for each endpoint it serves (it provides those it holds
//! a grant for, and needs `projects`); `kernel create_endpoint` for each
//! host's endpoint and its own readiness endpoint; `kernel spawn_executable`
//! on the host executables; with projects' processes, `kernel spawn_as` on
//! every project, the authority to start a process as one, and `kernel
//! stop_project`, the authority to end every process of one (its Stop, and
//! its deletion). Its namespace holds the projects directory (writable) at
//! its real path, which is the path it gives its hosts, and the host
//! executables (read-only). In the directory it makes only its own markers
//! and renames a project being archived, unarchived or deleted; the projects' shared home it only
//! names to its hosts. Without projects' processes its hosts are sandboxed
//! children and can only inherit from it: then it also makes and removes
//! the folders, and needs the shared home in its namespace and the network
//! a shell has.
//!
//! With a grant for `jobs` it also keeps the sessions' background shell jobs
//! and wakes a session when one of them ends, which needs
//! `conversation wake` on every session; the jobs' records are in its own
//! state directory (`--jobs`).
//!
//! It holds no capability of a service that starts after it: they use its
//! endpoints (`fs`, `shell`, `jobs`, ...), and a handle taken before its
//! endpoint is first provided is stale. So a deletion's `forget` calls on the
//! services that keep a project's records are the UI service's, between this
//! service's `delete` and `purge`.
//!
//! A compromised instance can start any project's hosts and forward any call
//! to them: it reaches every project, as the window does, and no other user;
//! it can stop and delete any project; it can wake any session, which spends
//! that session's model calls.

mod hosts;
mod registry;
mod router;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use nucleus_protocol::{
    ACTION_CREATE_ENDPOINT, ACTION_PROVIDE, ACTION_SPAWN_AS, ACTION_SPAWN_EXECUTABLE, ACTION_STOP_PROJECT,
    ActionDecl, ActionName, ChildProgram, CreatedEndpoint, DispatchId, EffectClass, EndpointName, Error,
    Generation, Grant, HandleId, IdentityRequest, InterfaceDecl, KERNEL_ENDPOINT, Payload, ProcessId,
    RequestId, ResourcePath,
};
use nucleus_sdk::{ACTION_READY, ChannelError, Connection, Event, Reply};

pub use hosts::{HostConfig, Kind};
pub use registry::Registry;
pub use router::{Authority, Kernel, MAX_FORWARDS, Router};

/// How often idle hosts are looked for.
const SWEEP: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The projects directory: its real path, at which this process's
    /// sandbox has it too, so a home's path is one for it and for its hosts.
    pub root: PathBuf,
    /// Whether projects' processes run as the projects' user; without, hosts
    /// run as this service and folders are its directories.
    pub identities: bool,
    pub hosts: HostConfig,
    /// A host that served no call for this long is stopped.
    pub idle: Duration,
    /// Where the background jobs' records are kept.
    pub jobs: Option<PathBuf>,
}

impl Options {
    /// Parses `--root <dir> --home <dir> --programs <dir>
    /// [--identities on|off]
    /// [--env NAME=VALUE]... [--shell-arg <arg>]...
    /// [--prover-shell-arg <arg>]... [--skills-catalog <url>]... [--idle-ms <n>] [--jobs <dir>]`. `--env` is the environment of
    /// shells before the project's own (`HOME`, `PATH`'s start, …), which it cannot
    /// replace.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut root, mut home, mut programs) = (None, None, None);
        let mut options = Self {
            root: PathBuf::new(),
            identities: false,
            hosts: HostConfig::default(),
            idle: Duration::from_secs(600),
            jobs: None,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--home" => home = Some(value),
                "--programs" => programs = Some(value),
                "--identities" => {
                    options.identities = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => return Err("--identities takes on or off".into()),
                    };
                }
                "--env" => {
                    let (name, val) = value
                        .split_once('=')
                        .ok_or_else(|| format!("--env takes NAME=VALUE, not {value:?}"))?;
                    if name.is_empty() || name.contains(['\0', '=']) || val.contains('\0') {
                        return Err(format!("--env: invalid variable {value:?}"));
                    }
                    options.hosts.env.push((name.to_owned(), val.to_owned()));
                }
                "--shell-arg" => options.hosts.shell_args.push(value),
                "--prover-shell-arg" => options.hosts.prover_shell_args.push(value),
                "--skills-catalog" if value.starts_with("https://") => {
                    options.hosts.skills_catalogs.push(value)
                }
                "--skills-catalog" => {
                    return Err(format!("--skills-catalog must be an https URL, not {value:?}"));
                }
                "--jobs" => options.jobs = Some(PathBuf::from(value)),
                "--idle-ms" => {
                    options.idle =
                        Duration::from_millis(value.parse().map_err(|_| "--idle-ms must be a number")?);
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        options.root = root.ok_or("--root is required")?;
        options.hosts.home = home.ok_or("--home is required")?;
        options.hosts.programs = programs.ok_or("--programs is required")?;
        // What shell hosts will declare, so this service declares the same,
        // for every project: the folder is named by its pattern.
        let home = format!("{}/<project>", options.root.display());
        let plain = options.identities;
        let shell = hosts::args(Kind::Shell, &options.hosts, &home, plain);
        options.hosts.shell_description =
            nucleus_shell::Options::parse(shell).map_err(|e| format!("--shell-arg: {e}"))?.description();
        let workers = |kind: Kind| {
            nucleus_shell::Options::parse(hosts::args(kind, &options.hosts, &home, plain))
                .map(|shell| shell.description())
                .map_err(|e| format!("--prover-shell-arg: {e}"))
        };
        (options.hosts.prover_shell_description, options.hosts.prover_shell_net_description) =
            (workers(Kind::ProverShell)?, workers(Kind::ProverShellNet)?);
        Ok(options)
    }
}

impl<R: Read, W: Write> Kernel for Connection<R, W> {
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError> {
        Connection::reply(self, dispatch, result)
    }
    fn submit(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline_ms: Option<u64>,
    ) -> Reply<RequestId> {
        Connection::submit(self, handle, action, resource, payload, deadline_ms)
    }
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
        Connection::cancel(self, request)
    }
    fn create_endpoint(
        &mut self,
        authority: HandleId,
        actions: std::collections::BTreeSet<ActionName>,
    ) -> Reply<CreatedEndpoint> {
        Connection::create_endpoint(self, authority, actions)
    }
    fn spawn(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        identity: Option<IdentityRequest>,
    ) -> Reply<ProcessId> {
        Connection::spawn_executable_as(self, authority, program, grants, identity)
    }
    fn kill(&mut self, child: ProcessId) -> Reply<()> {
        Connection::kill(self, child)
    }
    fn rebind(&mut self, handle: HandleId) -> Reply<Generation> {
        Connection::rebind(self, handle)
    }
    fn close(&mut self, handle: HandleId) -> Reply<()> {
        Connection::close(self, handle)
    }
    fn stop_project(&mut self, authority: HandleId, project: &str) -> Reply<RequestId> {
        Connection::submit_project_stop(self, authority, project)
    }
}

/// The interfaces this service may provide, by endpoint name.
fn interfaces(options: &Options) -> Result<BTreeMap<&'static str, InterfaceDecl>, Error> {
    let name = EndpointName::new;
    Ok(BTreeMap::from([
        ("projects", nucleus_projects_interface::projects_interface(name("projects")?)?),
        ("files", nucleus_projects_interface::files_interface(name("files")?)?),
        ("diffs", nucleus_diffs_interface::interface(name("diffs")?)?),
        ("fs", nucleus_fs::interface(name("fs")?)?),
        (
            "shell",
            nucleus_shell_interface::interface(
                name("shell")?,
                options.hosts.shell_description.clone(),
                true,
            )?,
        ),
        ("jobs", nucleus_shell_interface::jobs::interface(name("jobs")?)?),
        ("pdf", nucleus_pdf_interface::interface(name("pdf")?)?),
        ("skills", nucleus_skills_interface::interface(name("skills")?)?),
        (
            hosts::PROVER_SHELL,
            nucleus_shell_interface::interface(
                name(hosts::PROVER_SHELL)?,
                options.hosts.prover_shell_description.clone(),
                false,
            )?,
        ),
        (
            hosts::PROVER_SHELL_NET,
            nucleus_shell_interface::interface(
                name(hosts::PROVER_SHELL_NET)?,
                options.hosts.prover_shell_net_description.clone(),
                false,
            )?,
        ),
    ]))
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Provides the endpoints this service holds grants for and routes their
/// calls until told to stop.
pub fn run(options: &Options) -> Result<(), String> {
    let registry = Registry::open(&options.root, options.identities)?;
    let mut kernel = Connection::from_stdio().map_err(text)?;
    let held = |kernel: &Connection<_, _>, action: &str| kernel.find_handle(KERNEL_ENDPOINT, action);
    let create = held(&kernel, ACTION_CREATE_ENDPOINT).ok_or("started without kernel create_endpoint")?;
    let spawn = held(&kernel, ACTION_SPAWN_EXECUTABLE).ok_or("started without kernel spawn_executable")?;
    let (spawn_as, stop) = (held(&kernel, ACTION_SPAWN_AS), held(&kernel, ACTION_STOP_PROJECT));
    if [spawn_as.is_some(), stop.is_some()] != [options.identities; 2] {
        return Err("--identities on needs kernel spawn_as and stop_project, and they need it".into());
    }
    let ready = kernel
        .create_endpoint(create, [ActionName::new(ACTION_READY).map_err(text)?].into())
        .map_err(text)?
        .map_err(text)?;
    kernel
        .provide(
            ready.provide.handle,
            InterfaceDecl {
                endpoint: ready.endpoint.clone(),
                version: 1,
                description: String::new(),
                actions: [(
                    ActionName::new(ACTION_READY).map_err(text)?,
                    ActionDecl::internal(EffectClass::Observation),
                )]
                .into(),
            },
        )
        .map_err(text)?
        .map_err(text)?;
    let mut served = Vec::new();
    // `projects` last: the launcher boots the next program once it is
    // provided, and that program may use any endpoint this one serves.
    let mut interfaces: Vec<_> = interfaces(options).map_err(text)?.into_iter().collect();
    interfaces.sort_by_key(|(name, _)| *name == "projects");
    for (name, interface) in interfaces {
        let resource = ResourcePath::new(["endpoint", name]).map_err(text)?;
        if let Some(authority) = kernel.find_handle_for(KERNEL_ENDPOINT, ACTION_PROVIDE, &resource) {
            kernel
                .provide(authority, interface)
                .map_err(text)?
                .map_err(|e| format!("provide {name}: {e}"))?;
            served.push(name);
        }
    }
    if !served.contains(&"projects") {
        return Err("started without kernel provide for the projects endpoint".into());
    }
    // A job's end wakes its session: jobs are served only with that authority.
    let wake = kernel
        .find_handle(nucleus_conversation_interface::ENDPOINT, nucleus_conversation_interface::ACTION_WAKE);
    if served.contains(&"jobs") && wake.is_none() {
        return Err("jobs need conversation wake on the sessions".into());
    }
    let authority =
        Authority { create, spawn, spawn_as, stop, ready: (ready.endpoint, ready.invoke.handle), wake };
    let router = Router::new(registry, options.hosts.clone(), authority, options.idle)
        .with_jobs(options.jobs.clone())?;
    let router = Rc::new(RefCell::new(router));
    router.borrow_mut().wake_untold(&mut kernel).map_err(text)?;
    router.borrow_mut().delete_interrupted(&mut kernel).map_err(text)?;
    // Calls are routed as they arrive, also while `next_answer` waits. While
    // the loop below holds the router for an answer, an event waits in the
    // backlog for the next turn.
    let serving = Rc::clone(&router);
    kernel.set_event_handler(move |kernel, event| {
        let Ok(mut router) = serving.try_borrow_mut() else { return Ok(false) };
        match event {
            Event::Dispatch(d) => router.dispatch(kernel, d)?,
            Event::CancelDispatch(dispatch) => router.cancel(kernel, *dispatch)?,
            Event::ChildExited(child) => router.child_exited(kernel, *child)?,
            Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(false),
        }
        Ok(true)
    });
    let waker = kernel.waker();
    std::thread::spawn(move || {
        while waker.wake() {
            std::thread::sleep(SWEEP);
        }
    });
    loop {
        let outcome = match kernel.next_answer() {
            Ok(Some((request, result))) => router.borrow_mut().answer(&mut kernel, request, result),
            Ok(None) => router.borrow_mut().sweep(&mut kernel),
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => Err(e),
        };
        outcome.map_err(text)?;
    }
}

#[cfg(test)]
mod tests;
