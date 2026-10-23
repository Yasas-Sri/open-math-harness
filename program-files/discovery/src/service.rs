//! The discovery service. It provides `discovery` and `explore`, keeps the
//! registry of runs, starts every run it admits or resumes as its own child
//! process ([`crate::run_process`]) and forwards the run's workers' `explore`
//! calls to it ([`crate::wire`]). It answers `status`, `list`, `problems` and
//! `tree` from the runs' reports and journals, and stops a run by killing its
//! process.
//!
//! It never waits on a run: every call to a run's process is submitted and
//! its answer taken when it comes (the SDK's `submit` and `next_answer`),
//! and the only requests it waits on are the kernel's own. Its authority is
//! the discovery program's: it narrows it to each run's process and itself
//! uses only `provide`, `create_endpoint`, `spawn_executable` and, for a
//! project being deleted, `lean forget` (module doc, "Authority and
//! compromise").

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use super::*;
use crate::progress::Progress;
use crate::registry::{Phase, Registry, Run, run_of};
use crate::wire::{self, Admission, RunCommand};
use crate::workers::{EXPLORE_ACTIONS, LEAN_ACTIONS, executable_resource, find, path_text};
use nucleus_discovery_interface::ResumeResult;
use nucleus_protocol::{ACTION_CREATE_ENDPOINT, CreatedEndpoint, DispatchId, Generation, RequestId};
use nucleus_sdk::Reply;

mod forget;

/// Deadline of a `begin`: the run's process answers it at once.
const BEGIN_DEADLINE_MS: u64 = 30_000;

/// The most calls to its runs' processes the service has in progress. The
/// SDK takes at most 1024 outstanding requests, the service's own kernel
/// requests among them, and one more ends the channel with every run; a
/// call past this bound is refused instead.
const MAX_CALLS_IN_PROGRESS: usize = 1000;

/// What the service does with its kernel channel.
pub(crate) trait Kernel {
    fn submit(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline_ms: u64,
    ) -> Reply<RequestId>;
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError>;
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError>;
    fn kill(&mut self, child: ProcessId) -> Reply<()>;
    fn close(&mut self, handle: HandleId) -> Reply<()>;
    fn rebind(&mut self, handle: HandleId) -> Reply<Generation>;
    fn create_endpoint(
        &mut self,
        authority: HandleId,
        actions: BTreeSet<ActionName>,
    ) -> Reply<CreatedEndpoint>;
    fn spawn_executable(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
    ) -> Reply<ProcessId>;
}

impl<R: Read, W: std::io::Write> Kernel for Connection<R, W> {
    fn submit(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline_ms: u64,
    ) -> Reply<RequestId> {
        Connection::submit(self, handle, action, resource, payload, Some(deadline_ms))
    }
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
        Connection::cancel(self, request)
    }
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError> {
        Connection::reply(self, dispatch, result)
    }
    fn kill(&mut self, child: ProcessId) -> Reply<()> {
        Connection::kill(self, child)
    }
    fn close(&mut self, handle: HandleId) -> Reply<()> {
        Connection::close(self, handle)
    }
    fn rebind(&mut self, handle: HandleId) -> Reply<Generation> {
        Connection::rebind(self, handle)
    }
    fn create_endpoint(
        &mut self,
        authority: HandleId,
        actions: BTreeSet<ActionName>,
    ) -> Reply<CreatedEndpoint> {
        Connection::create_endpoint(self, authority, actions)
    }
    fn spawn_executable(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
    ) -> Reply<ProcessId> {
        Connection::spawn_executable(self, authority, program, grants)
    }
}

/// The service's capabilities, found in its own handle table.
pub(crate) struct Authority {
    provide_discovery: HandleId,
    provide_explore: HandleId,
    /// `kernel create_endpoint`: its report endpoint and every run's.
    create: HandleId,
    /// `kernel spawn_executable`, delegable: the run processes, and each
    /// run's narrowing to the prover executable.
    spawn: HandleId,
    model: HandleInfo,
    /// `model {next, cancel}` on a handle of its own, when the model handle
    /// holds `start` but not these (a budgeted generation grant beside an
    /// unbudgeted stream grant).
    streaming: Option<HandleInfo>,
    /// `lean {statement, check, verify, analyze, complete}`, delegable.
    lean: HandleId,
    /// `lean register`, delegable: each run's process gets it on `[key]`.
    register: HandleId,
    /// `lean forget` on the root, the service's own: a deleted project's
    /// runs' statements go (module `forget`). Absent where no project is
    /// deleted.
    forget: Option<HandleId>,
    explore: HandleId,
    /// A delegable `shell exec`, and its scope: each run's workspaces are
    /// `<scope>/<project>/runs/<run>` there.
    shell: Option<(HandleId, ResourcePath)>,
    /// The same on the workers' shell that has network, when the service
    /// holds one: where a run whose workers have network works.
    network_shell: Option<(HandleId, ResourcePath)>,
}

impl Authority {
    /// Missing authority is refused with a message naming it.
    pub(crate) fn from(handles: &[HandleInfo], options: &Options) -> Result<Self, String> {
        let provide = |name: &str| -> Result<HandleId, String> {
            let resource = ResourcePath::new(["endpoint", name]).map_err(|e| e.message)?;
            find(handles, KERNEL_ENDPOINT, &[ACTION_PROVIDE], false, Some(&resource))
                .map(|h| h.handle)
                .ok_or_else(|| format!("started without authority to provide the `{name}` endpoint"))
        };
        let endpoints = ResourcePath::new(["endpoint"]).map_err(|e| e.message)?;
        // The kernel refuses creation authority that is not delegable; the
        // service delegates none of it.
        let create = find(handles, KERNEL_ENDPOINT, &[ACTION_CREATE_ENDPOINT], true, Some(&endpoints))
            .ok_or("started without a delegable `kernel create_endpoint` on [endpoint]")?;
        let run_executable = options
            .run_executable
            .as_deref()
            .ok_or("--run-executable is required: the discovery program in the service's namespace")?;
        let (runs, workers) =
            (executable_resource(run_executable)?, executable_resource(&options.prover_executable)?);
        let spawn = find(handles, KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], true, Some(&runs))
            .filter(|h| workers.is_within(&h.resource))
            .ok_or("started without a delegable `kernel spawn_executable` covering the run and prover executables")?;
        let model = find(handles, model::ENDPOINT, &[model::ACTION_COMPLETE], true, None)
            .ok_or("started without a delegable `model complete` capability")?
            .clone();
        let holds = |h: &HandleInfo, action: &str| h.actions.iter().any(|a| a.as_str() == action);
        let streaming = (holds(&model, model::ACTION_START)
            && !(holds(&model, model::ACTION_NEXT) && holds(&model, model::ACTION_CANCEL)))
        .then(|| {
            find(
                handles,
                model::ENDPOINT,
                &[model::ACTION_NEXT, model::ACTION_CANCEL],
                true,
                Some(&model.resource),
            )
        })
        .flatten()
        .cloned();
        let lean_endpoint = options.lean_endpoint.as_str();
        let root = ResourcePath::root();
        let lean_actions: Vec<&str> = std::iter::once(lean::ACTION_STATEMENT).chain(LEAN_ACTIONS).collect();
        let lean_h = find(handles, lean_endpoint, &lean_actions, true, Some(&root)).ok_or_else(|| {
            format!(
                "started without a delegable `{lean_endpoint} {{statement, check, verify, analyze, complete}}` \
                 capability"
            )
        })?;
        let register =
            find(handles, lean_endpoint, &[lean::ACTION_REGISTER], true, Some(&root)).ok_or_else(|| {
                format!(
                    "started without a delegable `{lean_endpoint} register` on the root (each run's process \
                 registers the run's statements, which the run's candidates are judged against)"
                )
            })?;
        let forget =
            find(handles, lean_endpoint, &[lean::ACTION_FORGET], false, Some(&root)).map(|h| h.handle);
        let explore_endpoint = options.explore_endpoint.as_str();
        let explore_h = find(handles, explore_endpoint, &EXPLORE_ACTIONS, true, Some(&root)).ok_or_else(|| {
            format!(
                "started without a delegable `{explore_endpoint} {{assignment, submit, publish_plan, \
                 read_plans}}` capability (needed to hand each worker its task; providing the endpoint grants \
                 no handle on it)"
            )
        })?;
        let shell_h = find(handles, options.shell_endpoint.as_str(), &[shell::ACTION_EXEC], true, None);
        let network_shell_h = options
            .network_shell_endpoint
            .as_deref()
            .and_then(|endpoint| find(handles, endpoint, &[shell::ACTION_EXEC], true, None));
        Ok(Self {
            provide_discovery: provide(&options.discovery_endpoint)?,
            provide_explore: provide(&options.explore_endpoint)?,
            create: create.handle,
            spawn: spawn.handle,
            model,
            streaming,
            lean: lean_h.handle,
            register: register.handle,
            forget,
            explore: explore_h.handle,
            shell: shell_h.map(|h| (h.handle, h.resource.clone())),
            network_shell: network_shell_h.map(|h| (h.handle, h.resource.clone())),
        })
    }

    /// The workers' shell of a run: the one with network when its workers
    /// have it, which the service must then hold.
    fn workers_shell(&self, network: bool) -> Result<Option<&(HandleId, ResourcePath)>, Error> {
        if !network {
            return Ok(self.shell.as_ref());
        }
        self.network_shell.as_ref().map(Some).ok_or_else(|| {
            invalid(
                "task: worker_network: this service holds no separate workers' shell with network, so a run \
                 cannot choose one",
            )
        })
    }

    /// Everything the process of `run` of `project` gets: its run's
    /// narrowing of this authority on its `key`, with `provide` of its own
    /// endpoint `created` and `report` on the report endpoint through
    /// `report_invoke`, and its workspaces in its project's, in the workers'
    /// shell with network when its workers have it.
    fn run_grants(
        &self,
        options: &Options,
        key: &str,
        (project, run, network): (&str, &str, bool),
        created: &CreatedEndpoint,
        report_invoke: HandleId,
    ) -> Result<Vec<Grant>, String> {
        let acts = |names: &[&str]| -> Result<BTreeSet<ActionName>, String> {
            names.iter().map(|n| ActionName::new(*n).map_err(|e| e.to_string())).collect()
        };
        let grant =
            |from: HandleId, actions: BTreeSet<ActionName>, resource: ResourcePath, delegable: bool| Grant {
                from,
                actions,
                resource,
                ttl_ms: None,
                delegable,
                max_calls: None,
            };
        let scope = ResourcePath::new([key]).map_err(|e| e.message)?;
        let own = ResourcePath::new(["endpoint", created.endpoint.as_str()]).map_err(|e| e.message)?;
        let mut grants = vec![
            grant(created.provide.handle, acts(&[ACTION_PROVIDE])?, own, false),
            grant(
                self.spawn,
                acts(&[ACTION_SPAWN_EXECUTABLE])?,
                executable_resource(&options.prover_executable)?,
                false,
            ),
            grant(self.model.handle, self.model.actions.clone(), self.model.resource.clone(), true),
            grant(self.lean, acts(&[lean::ACTION_STATEMENT])?, ResourcePath::root(), false),
            grant(self.lean, acts(&LEAN_ACTIONS)?, scope.clone(), true),
            grant(self.register, acts(&[lean::ACTION_REGISTER])?, scope.clone(), false),
            grant(self.explore, acts(&EXPLORE_ACTIONS)?, scope.clone(), true),
            grant(report_invoke, acts(&[wire::ACTION_REPORT])?, scope, false),
        ];
        if let Some(streaming) = &self.streaming {
            let actions = acts(&[model::ACTION_NEXT, model::ACTION_CANCEL])?;
            grants.push(grant(streaming.handle, actions, self.model.resource.clone(), true));
        }
        if let Some((shell, scope)) = self.workers_shell(network).map_err(|e| e.message)? {
            let mut workspaces = scope.segments().to_vec();
            workspaces.extend([project.to_owned(), RUNS_DIR.to_owned(), run.to_owned()]);
            let workspaces = ResourcePath::new(workspaces).map_err(|e| e.message)?;
            grants.push(grant(*shell, acts(&[shell::ACTION_EXEC])?, workspaces, true));
        }
        Ok(grants)
    }
}

/// Where a project's runs' workspaces are, in the shell service's scope of
/// the project: `runs/<run>/<problem>/<task>`, in the project's folder when
/// the shell is the project's. These are the files a worker's commands work
/// on, and there the project's sessions can read a solved problem's proof.
/// A run's records (its journal, task, spend receipts and each worker's
/// transcript) are not here: they are the runtime's, under `--out`, so no
/// process of a project can read or write them and a worker's command cannot
/// write itself a solve. A worker's command is not held to its workspace: it
/// can write every project's folder, the other workers' workspaces included,
/// and what it leaves, a symbolic link included, is untrusted for whoever
/// reads it. What a worker hands in is still judged through `lean`.
const RUNS_DIR: &str = "runs";

/// The project a `start` names: `[project]`, a project's name.
fn project_of(resource: &ResourcePath) -> Result<String, Error> {
    match resource.segments() {
        [project] if discovery::valid_name(project) => Ok(project.clone()),
        _ => Err(invalid("start is called on [project]")),
    }
}

/// How the process of `run` of `project` is started: this program in `run`
/// mode, seeing the executables' directory and the policies read-only and
/// its project's output directory writable, which it is told is its own
/// output directory (`--out`, the last one given), at the path the service
/// has it at, so that a path it reports is the one the service knows, and
/// told its project (`--project`), which its workers' model calls name; with
/// no network. A run whose workers have network is told the workers' shell
/// that has it: the process finds its workspaces there, and its workers
/// theirs.
fn run_program(
    options: &Options,
    (project, run, network): (&str, &str, bool),
    endpoint: &EndpointName,
    report_endpoint: &EndpointName,
) -> Result<ChildProgram, Error> {
    let executable =
        options.run_executable.as_deref().ok_or_else(|| invalid("--run-executable is not set"))?;
    let programs = options.prover_executable.parent().unwrap_or(Path::new("/"));
    let mut binds = vec![ChildBind {
        path: path_text("the programs directory", programs)?,
        mount: None,
        writable: false,
    }];
    if options.policies_dir.is_dir() {
        binds.push(ChildBind {
            path: path_text("--policies-dir", &options.policies_dir)?,
            mount: None,
            writable: false,
        });
    }
    let out = path_text("--out", &options.out.join(project))?;
    binds.push(ChildBind { path: out.clone(), mount: None, writable: true });
    let mut args = RunCommand::args(run, endpoint, report_endpoint, options);
    args.extend(["--out".to_owned(), out, "--project".to_owned(), project.to_owned()]);
    if let (true, Some(shell)) = (network, &options.network_shell_endpoint) {
        // The last one given is the one the process takes.
        args.extend(["--shell-endpoint".to_owned(), shell.clone()]);
    }
    Ok(ChildProgram { executable: path_text("--run-executable", executable)?, args, binds, network: false })
}

/// A run's process, as the service tracks it.
struct Proc {
    project: String,
    run: String,
    /// The run's key ([`wire::key_of`]).
    key: String,
    /// The process's endpoint, which the service allocated for it and which
    /// names it as the owner of the directories it creates.
    endpoint: EndpointName,
    /// The service's two handles on the process's endpoint.
    provide: HandleId,
    invoke: HandleId,
    /// What to ask once the process says it is ready.
    admission: Option<Admission>,
    ready: bool,
    begun: bool,
    exited: bool,
    killed: bool,
    /// Its last report said `done` or `failed`.
    ended: bool,
    /// Worker calls forwarded to it and not yet answered.
    forwards: usize,
}

/// A call the service made to a run's process and has not had answered.
enum Call {
    Admit {
        process: ProcessId,
    },
    Begin {
        process: ProcessId,
    },
    /// `dispatch` is the worker's call, `None` once it was cancelled;
    /// `observation` when the worker's action changes nothing.
    Forward {
        process: ProcessId,
        dispatch: Option<DispatchId>,
        observation: bool,
    },
    /// `lean forget` of a run of the project being forgotten.
    Forget {
        project: String,
    },
}

pub(crate) struct Service {
    options: Options,
    authority: Authority,
    report_endpoint: EndpointName,
    report_invoke: HandleId,
    runs: Registry,
    procs: BTreeMap<ProcessId, Proc>,
    calls: BTreeMap<RequestId, Call>,
    /// Admissions so far: a queued run's place in line.
    admissions: u64,
    /// Projects being forgotten (module `forget`).
    forgetting: BTreeMap<String, forget::Forgetting>,
}

fn cancelled(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Cancelled, message)
}

impl Service {
    pub(crate) fn new(
        options: Options,
        authority: Authority,
        report_endpoint: EndpointName,
        report_invoke: HandleId,
        runs: Registry,
    ) -> Self {
        Service {
            options,
            authority,
            report_endpoint,
            report_invoke,
            runs,
            procs: BTreeMap::new(),
            calls: BTreeMap::new(),
            admissions: 0,
            forgetting: BTreeMap::new(),
        }
    }

    /// Runs whose process has begun and is not yet cleaned up.
    fn running(&self) -> usize {
        self.procs.values().filter(|p| p.begun).count()
    }

    /// Submits a call to a run's process, or refuses it when
    /// [`MAX_CALLS_IN_PROGRESS`] are in progress already.
    fn submit(
        &self,
        k: &mut impl Kernel,
        invoke: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline_ms: u64,
    ) -> Reply<RequestId> {
        if self.calls.len() >= MAX_CALLS_IN_PROGRESS {
            let busy = format!(
                "the service has {} calls to runs in progress, as many as it can have",
                self.calls.len()
            );
            return Ok(Err(Error::new(ErrorCode::ServiceError, busy)));
        }
        k.submit(invoke, action, resource, payload, deadline_ms)
    }

    /// Routes one dispatch by its action: the three interfaces the service
    /// provides declare disjoint actions (checked at startup).
    pub(crate) fn dispatch(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let answer = match d.action.as_str() {
            discovery::ACTION_START => return self.start(k, d),
            discovery::ACTION_RESUME => return self.resume(k, d),
            discovery::ACTION_STOP => return self.stop(k, d),
            discovery::ACTION_FORGET => return self.forget(k, d),
            wire::ACTION_REPORT => return self.report(k, d),
            action if EXPLORE_ACTIONS.contains(&action) => return self.explore(k, d),
            discovery::ACTION_STATUS => self.runs.status(&d.resource),
            discovery::ACTION_LIST => self.runs.list(&d.resource, &d.payload),
            discovery::ACTION_PROBLEMS => self.runs.problems(&self.options.out, &d.resource),
            discovery::ACTION_TREE => self.runs.tree(&self.options.out, &d.resource),
            _ => Err(invalid("unknown action")),
        };
        k.reply(d.id, answer)
    }

    /// `discovery start`: what the service can check alone (the task, its
    /// files and the ceilings, a tool-loop template, the name, the queue),
    /// then a process for the run, which admits it; the call is answered
    /// when it has.
    fn start(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let taken = Instant::now();
        let accepted = project_of(&d.resource).and_then(|project| {
            self.not_forgetting(&project)?;
            let task = d.payload.decode::<Task>()?;
            let out = self.project_dir(&project)?;
            let plan = resolve_task(&task, &self.options, &out)?;
            let shell = self.authority.workers_shell(plan.worker_network)?;
            resume::fits_tool_loop(&plan.prompt_template, shell.is_some())?;
            if self.runs.get(&project, &plan.name).is_some() {
                return Err(invalid(format!("task: run {:?} already exists", plan.name)));
            }
            self.runs.room(self.options.max_queued_runs)?;
            Ok((project, plan, admit::until(taken, task.timeout_ms, d.remaining_ms)))
        });
        let (project, plan, until) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        let admission = Admission::Start { task: Box::new(plan.document.clone()) };
        let process = match self.spawn(k, (&project, &plan.name, plan.worker_network), admission)? {
            Ok(process) => process,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        let status = Progress::new(plan.clone()).status(Spend::default());
        let phase = Phase::Admitting { process, held: Some(d.id), until, fresh: true };
        self.runs.push(Run::new(&project, plan, status, phase, false));
        Ok(())
    }

    /// The output directory of `project`'s runs, `<out>/<project>`, made
    /// with the project's first run. It is the runtime's own, like every
    /// record of a run: a project's processes, the workers' commands among
    /// them, cannot read or write it. With `--projects`, a project that has
    /// no home there has no runs.
    fn project_dir(&self, project: &str) -> Result<PathBuf, Error> {
        if let Some(projects) = &self.options.projects
            && !std::fs::symlink_metadata(projects.join(project)).is_ok_and(|meta| meta.is_dir())
        {
            return Err(invalid(format!("there is no project {project:?}")));
        }
        let dir = self.options.out.join(project);
        match std::fs::create_dir(&dir) {
            Ok(()) => Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(dir),
            Err(e) => Err(Error::new(ErrorCode::ServiceError, format!("{}: {e}", dir.display()))),
        }
    }

    /// `discovery resume`: a run that has ended and whose last process the
    /// kernel confirmed gone, admitted again by a process of its own.
    fn resume(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let taken = Instant::now();
        let accepted = d.payload.decode::<discovery::ResumeRequest>().and_then(|request| {
            request.validate()?;
            let (project, run) = run_of(&d.resource)?;
            self.not_forgetting(project)?;
            let entry = self
                .runs
                .get(project, run)
                .filter(|r| r.visible())
                .ok_or_else(|| invalid(format!("no such run {run:?}")))?;
            match entry.phase {
                Phase::Ended { process: None } => {}
                Phase::Ended { process: Some(_) } => {
                    return Err(Error::new(
                        ErrorCode::ServiceError,
                        format!(
                            "run {run}'s last process is still being cleaned up; resume it again in a moment"
                        ),
                    ));
                }
                _ => return Err(invalid("session still has a live or queued execution")),
            }
            // Its workers keep the network its task recorded.
            let network = entry.plan.worker_network;
            self.authority.workers_shell(network)?;
            self.runs.room(self.options.max_queued_runs)?;
            let until = admit::until(taken, request.timeout_ms, d.remaining_ms);
            Ok((project.to_owned(), run.to_owned(), network, until))
        });
        let (project, run, network, until) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        let process = match self.spawn(k, (&project, &run, network), Admission::Resume)? {
            Ok(process) => process,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        if let Some(entry) = self.runs.get_mut(&project, &run) {
            entry.phase = Phase::Admitting { process, held: Some(d.id), until, fresh: false };
        }
        Ok(())
    }

    /// Allocates the endpoint of `run`'s process and starts the process,
    /// which will be asked `admission` once it says it is ready. A refusal
    /// releases the endpoint again.
    fn spawn(
        &mut self,
        k: &mut impl Kernel,
        (project, run, network): (&str, &str, bool),
        admission: Admission,
    ) -> Result<Result<ProcessId, Error>, ChannelError> {
        let actions: Result<BTreeSet<ActionName>, Error> =
            wire::RUN_ACTIONS.iter().map(|a| ActionName::new(*a)).collect();
        let created = match actions {
            Ok(actions) => k.create_endpoint(self.authority.create, actions)?,
            Err(e) => Err(e),
        };
        let created = match created {
            Ok(created) => created,
            Err(e) => return Ok(Err(e)),
        };
        let key = wire::key_of(project, run);
        let named = (project, run, network);
        let program = run_program(&self.options, named, &created.endpoint, &self.report_endpoint);
        let grants = self
            .authority
            .run_grants(&self.options, &key, named, &created, self.report_invoke)
            .map_err(|why| Error::new(ErrorCode::ServiceError, why));
        let spawned = match (program, grants) {
            (Ok(program), Ok(grants)) => k.spawn_executable(self.authority.spawn, program, grants)?,
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        match spawned {
            Ok(process) => {
                self.procs.insert(
                    process,
                    Proc {
                        project: project.to_owned(),
                        run: run.to_owned(),
                        key,
                        endpoint: created.endpoint.clone(),
                        provide: created.provide.handle,
                        invoke: created.invoke.handle,
                        admission: Some(admission),
                        ready: false,
                        begun: false,
                        exited: false,
                        killed: false,
                        ended: false,
                        forwards: 0,
                    },
                );
                Ok(Ok(process))
            }
            Err(e) => {
                k.close(created.provide.handle)?.ok();
                k.close(created.invoke.handle)?.ok();
                Ok(Err(e))
            }
        }
    }

    /// Kills `process` once; a refusal means it is gone already.
    fn kill(&mut self, k: &mut impl Kernel, process: ProcessId) -> Result<(), ChannelError> {
        match self.procs.get_mut(&process) {
            Some(p) if !p.killed && !p.exited => p.killed = true,
            _ => return Ok(()),
        }
        k.kill(process)?.ok();
        Ok(())
    }

    /// The admission `process` holds is over without a run: its call is
    /// answered with `error`, the process killed, and the run goes back to
    /// how it was (a resume) or is gone once the process is (a start).
    fn abandon(&mut self, k: &mut impl Kernel, process: ProcessId, error: Error) -> Result<(), ChannelError> {
        let Some(entry) = self.runs.of_process(process) else { return self.kill(k, process) };
        let Phase::Admitting { held, fresh, .. } = entry.phase else { return self.kill(k, process) };
        entry.phase =
            if fresh { Phase::Cancelled { process } } else { Phase::Ended { process: Some(process) } };
        if let Some(held) = held {
            k.reply(held, Err(error))?;
        }
        self.kill(k, process)
    }

    /// A report from a run's process: accepted only from the process of the
    /// run it names, and read by its version first. The first one says the
    /// process is ready; the others say where the run stands.
    fn report(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let key = match d.resource.segments() {
            [key] => key.clone(),
            _ => return k.reply(d.id, Err(invalid("a report is on [key]"))),
        };
        if !self.procs.get(&d.caller).is_some_and(|p| p.key == key && !p.exited) {
            let refused = Error::new(ErrorCode::PermissionDenied, format!("not the process of run {key}"));
            return k.reply(d.id, Err(refused));
        }
        match wire::version_of(&d.payload) {
            Ok(v) if v != wire::PROTOCOL => {
                k.reply(d.id, Err(wire::mismatch(v)))?;
                return self.abandon(k, d.caller, wire::mismatch(v));
            }
            Err(e) => return k.reply(d.id, Err(e)),
            Ok(_) => {}
        }
        let report = match wire::decode::<wire::Report>(&d.payload) {
            Ok(report) => report,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        let ack = wire::encode(&wire::Ack { v: wire::PROTOCOL });
        let Some(status) = report.status else {
            k.reply(d.id, ack)?;
            return self.ready(k, d.caller);
        };
        let begun = self
            .runs
            .of_process(d.caller)
            .filter(|r| r.phase == Phase::Running { process: d.caller } && status.run == r.plan.name);
        let Some(entry) = begun else {
            return k.reply(d.id, Err(invalid(format!("run {key} has not begun"))));
        };
        let ended = matches!(status.state, RunState::Done | RunState::Failed);
        entry.status = status;
        entry.started = report.started;
        entry.live = report.live;
        if let Some(p) = self.procs.get_mut(&d.caller) {
            p.ended = ended;
        }
        k.reply(d.id, ack)
    }

    /// `process` provides its endpoint: the service's handle is rebound to it
    /// and the admission asked, with the time left of the call it holds.
    fn ready(&mut self, k: &mut impl Kernel, process: ProcessId) -> Result<(), ChannelError> {
        let Some(p) = self.procs.get_mut(&process).filter(|p| !p.ready) else { return Ok(()) };
        p.ready = true;
        let (invoke, key, admission) = (p.invoke, p.key.clone(), p.admission.take());
        let (project, run) = (p.project.clone(), p.run.clone());
        let until = match self.runs.get(&project, &run).map(|r| &r.phase) {
            Some(Phase::Admitting { process: holder, until, .. }) if *holder == process => *until,
            _ => return Ok(()),
        };
        let left =
            u64::try_from(until.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX);
        if left == 0 {
            let late = Error::new(
                ErrorCode::Timeout,
                "task: the call ran out of time before the run's process started; the run was not queued",
            );
            return self.abandon(k, process, late);
        }
        if let Err(e) = k.rebind(invoke)? {
            return self.abandon(k, process, e);
        }
        let asked = match (admission, ResourcePath::new([key.as_str()])) {
            (Some(admission), Ok(resource)) => {
                wire::encode(&wire::Admit { v: wire::PROTOCOL, admission }).map(|payload| (payload, resource))
            }
            (None, _) => Err(Error::new(ErrorCode::ServiceError, "the admission was asked already")),
            (_, Err(e)) => Err(e),
        };
        let submitted = match asked {
            Ok((payload, resource)) => {
                self.submit(k, invoke, wire::ACTION_ADMIT, &resource, &payload, left)?
            }
            Err(e) => Err(e),
        };
        match submitted {
            Ok(request) => {
                self.calls.insert(request, Call::Admit { process });
                Ok(())
            }
            Err(e) => self.abandon(k, process, e),
        }
    }

    /// The answer to a call the service submitted.
    pub(crate) fn answer(
        &mut self,
        k: &mut impl Kernel,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        match self.calls.remove(&request) {
            None => Ok(()),
            Some(Call::Forward { process, dispatch, observation }) => {
                if let Some(p) = self.procs.get_mut(&process) {
                    p.forwards = p.forwards.saturating_sub(1);
                }
                let Some(dispatch) = dispatch else { return Ok(()) };
                // An interrupted observation changed nothing: not an unknown outcome.
                let result = result.map_err(|e| match e.code {
                    ErrorCode::UnknownOutcome if observation => Error::new(
                        ErrorCode::ServiceError,
                        format!("the run's process did not answer: {}", e.message),
                    ),
                    _ => e,
                });
                k.reply(dispatch, result)
            }
            Some(Call::Admit { process }) => self.admitted(k, process, result),
            Some(Call::Forget { project }) => self.lean_forgot(k, &project, request, result),
            Some(Call::Begin { process }) => match result {
                Ok(_) => Ok(()),
                Err(e) => {
                    // A run stopped meanwhile stays stopped.
                    if let Some(entry) =
                        self.runs.of_process(process).filter(|r| r.phase == Phase::Running { process })
                    {
                        let why = format!("the run's process could not begin: {:?}: {}", e.code, e.message);
                        entry.end(RunState::Failed, Some(why), Some(process));
                    }
                    self.kill(k, process)
                }
            },
        }
    }

    /// The answer to `admit`: the run is queued and its caller told so, or
    /// the admission is refused with the process's words.
    fn admitted(
        &mut self,
        k: &mut impl Kernel,
        process: ProcessId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let (running, queued) = (self.running(), self.runs.queued());
        let Some(entry) = self.runs.of_process(process) else { return Ok(()) };
        let Phase::Admitting { held, fresh, .. } = entry.phase else { return Ok(()) };
        let admitted =
            result.and_then(|payload| wire::decode::<wire::Admitted>(&payload)).and_then(|admitted| {
                if admitted.status.run == entry.plan.name {
                    Ok(admitted)
                } else {
                    Err(Error::new(ErrorCode::ServiceError, "the run's process admitted another run"))
                }
            });
        let admitted = match admitted {
            Ok(admitted) => admitted,
            Err(e) => return self.abandon(k, process, e),
        };
        self.admissions += 1;
        let start = StartResult {
            run: entry.plan.name.clone(),
            problems: admitted.status.problems,
            queued_ahead: (running + queued + 1).saturating_sub(self.options.max_running_runs) as u64,
        };
        entry.admitted(admitted.status, Phase::Queued { process, seq: self.admissions });
        if !fresh {
            self.runs.move_last(process);
        }
        let answer = if fresh {
            Payload::encode(&start)
        } else {
            Payload::encode(&ResumeResult { start, uncertain_model_calls: admitted.uncertain_model_calls })
        };
        if let Some(held) = held {
            k.reply(held, answer)?;
        }
        self.schedule(k)
    }

    /// Begins the runs first in line while fewer than `--max-running-runs`
    /// run; a run's slot is free once the kernel confirms its process gone.
    fn schedule(&mut self, k: &mut impl Kernel) -> Result<(), ChannelError> {
        while self.running() < self.options.max_running_runs {
            let Some(process) = self.runs.first_queued() else { return Ok(()) };
            if let Some(entry) = self.runs.of_process(process) {
                entry.phase = Phase::Running { process };
                entry.status.state = RunState::Running;
            }
            let Some(p) = self.procs.get_mut(&process) else { return Ok(()) };
            p.begun = true;
            let invoke = p.invoke;
            let asked =
                match (wire::encode(&wire::Begin { v: wire::PROTOCOL }), ResourcePath::new([p.key.as_str()]))
                {
                    (Ok(payload), Ok(resource)) => {
                        self.submit(k, invoke, wire::ACTION_BEGIN, &resource, &payload, BEGIN_DEADLINE_MS)?
                    }
                    (Err(e), _) | (_, Err(e)) => Err(e),
                };
            match asked {
                Ok(request) => {
                    self.calls.insert(request, Call::Begin { process });
                }
                Err(e) => {
                    if let Some(entry) = self.runs.of_process(process) {
                        let why = format!("the run's process could not begin: {:?}: {}", e.code, e.message);
                        entry.end(RunState::Failed, Some(why), Some(process));
                    }
                    self.kill(k, process)?;
                }
            }
        }
        Ok(())
    }

    /// A worker's `explore` call, forwarded to the process whose key the
    /// resource names, with the worker's time; the answer comes back through
    /// [`Self::answer`]. At most `w × concurrency` calls of a run are in
    /// progress: a run's workers make one at a time, so more is a fault, and
    /// it is refused, never queued.
    fn explore(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let target = explore::task_of(&d.resource).ok().and_then(|(key, _, _)| {
            let (&process, _) = self.procs.iter().find(|(_, p)| p.key == key && !p.exited)?;
            let entry = self.runs.of_process(process)?;
            if entry.phase != (Phase::Running { process }) {
                return None;
            }
            Some((process, entry.plan.limits.w.saturating_mul(entry.plan.concurrency)))
        });
        let open = target.and_then(|(process, cap)| {
            self.procs.get(&process).filter(|p| !p.exited).map(|p| (process, p.invoke, p.forwards, cap))
        });
        let Some((process, invoke, forwards, cap)) = open else {
            return k.reply(d.id, Err(invalid("that task is not open")));
        };
        if forwards >= cap {
            let busy =
                format!("the run has {forwards} calls of its workers in progress, as many as it can have");
            return k.reply(d.id, Err(Error::new(ErrorCode::ServiceError, busy)));
        }
        let forward = wire::Forward {
            v: wire::PROTOCOL,
            caller: d.caller,
            action: d.action.clone(),
            payload: d.payload.clone(),
        };
        let submitted = match wire::encode(&forward) {
            Ok(payload) => {
                self.submit(k, invoke, wire::ACTION_FORWARD, &d.resource, &payload, d.remaining_ms)?
            }
            Err(e) => Err(e),
        };
        match submitted {
            Ok(request) => {
                let observation =
                    matches!(d.action.as_str(), explore::ACTION_ASSIGNMENT | explore::ACTION_READ_PLANS);
                self.calls.insert(request, Call::Forward { process, dispatch: Some(d.id), observation });
                if let Some(p) = self.procs.get_mut(&process) {
                    p.forwards += 1;
                }
                Ok(())
            }
            Err(e) => k.reply(d.id, Err(e)),
        }
    }

    /// `discovery stop`: kills the run's process and answers at once. A run
    /// still being admitted has its `start` or `resume` answered that it did
    /// not happen, as a `service_error`: a provider's `cancelled` reaches its
    /// caller as one anyway.
    fn stop(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        if d.resource.segments().len() == 1 {
            return self.stop_project(k, d);
        }
        let (project, run) = match run_of(&d.resource) {
            Ok((project, run)) => (project.to_owned(), run.to_owned()),
            Err(e) => return k.reply(d.id, Err(e)),
        };
        let phase = self.runs.get(&project, &run).map(|r| r.phase.clone());
        match phase {
            None | Some(Phase::Cancelled { .. }) => {
                return k.reply(d.id, Err(invalid(format!("no such run {run:?}"))));
            }
            Some(Phase::Admitting { process, .. }) => {
                let why = format!(
                    "a `stop` came in while run {run:?} was being admitted; it was not queued. Call again if it \
                     is still wanted"
                );
                self.abandon(k, process, Error::new(ErrorCode::ServiceError, why))?;
            }
            Some(Phase::Queued { process, .. } | Phase::Running { process }) => {
                // A run whose process has reported its end keeps that end.
                let ended = self.procs.get(&process).is_some_and(|p| p.ended);
                if let Some(entry) = self.runs.get_mut(&project, &run).filter(|_| !ended) {
                    entry.end(RunState::Stopped, None, Some(process));
                }
                self.kill(k, process)?;
            }
            Some(Phase::Ended { .. }) => {}
        }
        k.reply(d.id, Payload::encode(&serde_json::json!({})))
    }

    /// The kernel cancelled a call the service holds, because its caller gave
    /// up: a `start` or `resume` being admitted is abandoned, a worker's call
    /// forwarded is cancelled too. The kernel keeps a cancelled dispatch in
    /// flight until its provider replies, so the service replies at once.
    pub(crate) fn cancel_dispatch(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
    ) -> Result<(), ChannelError> {
        if let Some(Phase::Admitting { process, .. }) = self.runs.holding(dispatch).map(|r| r.phase.clone()) {
            self.abandon(k, process, cancelled("the call was cancelled"))?;
            return Ok(());
        }
        let forwarded = self.calls.iter_mut().find_map(|(request, call)| match call {
            Call::Forward { dispatch: held @ Some(_), .. } if *held == Some(dispatch) => {
                *held = None;
                Some(*request)
            }
            _ => None,
        });
        if let Some(request) = forwarded {
            k.cancel(request)?;
            k.reply(dispatch, Err(cancelled("the call was cancelled")))?;
        }
        self.forget_cancelled(k, dispatch)
    }

    /// A run's process exited: the endpoint is released; a run it was
    /// admitting has its call answered; a run it ran without a last report
    /// ended `failed`, and its journals may be open.
    pub(crate) fn child_exited(
        &mut self,
        k: &mut impl Kernel,
        process: ProcessId,
    ) -> Result<(), ChannelError> {
        let Some(p) = self.procs.get_mut(&process).filter(|p| !p.exited) else { return Ok(()) };
        p.exited = true;
        let (provide, invoke, ended) = (p.provide, p.invoke, p.ended);
        k.close(provide)?.ok();
        k.close(invoke)?.ok();
        let Some(entry) = self.runs.of_process(process) else { return Ok(()) };
        match entry.phase {
            Phase::Admitting { held, fresh, .. } => {
                entry.phase = if fresh {
                    Phase::Cancelled { process }
                } else {
                    Phase::Ended { process: Some(process) }
                };
                if let Some(held) = held {
                    let gone = Error::new(
                        ErrorCode::ServiceError,
                        "the run's process ended before it admitted the run",
                    );
                    k.reply(held, Err(gone))?;
                }
            }
            Phase::Queued { .. } | Phase::Running { .. } if ended => {
                entry.phase = Phase::Ended { process: Some(process) };
                entry.live.clear();
            }
            Phase::Queued { .. } | Phase::Running { .. } => {
                entry.end(RunState::Failed, Some("the run's process ended".to_owned()), Some(process));
            }
            Phase::Ended { .. } | Phase::Cancelled { .. } => {}
        }
        Ok(())
    }

    /// The kernel confirmed a run's process and its whole subtree gone: the
    /// run may be resumed, a start that did not happen is forgotten with
    /// whatever its process created, and a running slot may be free.
    pub(crate) fn child_reaped(
        &mut self,
        k: &mut impl Kernel,
        process: ProcessId,
    ) -> Result<(), ChannelError> {
        self.child_exited(k, process)?;
        let Some(proc) = self.procs.remove(&process) else { return Ok(()) };
        let cancelled = match self.runs.of_process(process) {
            Some(entry) if matches!(entry.phase, Phase::Cancelled { .. }) => {
                Some((entry.project.clone(), entry.plan.name.clone()))
            }
            Some(entry) => {
                entry.phase = Phase::Ended { process: None };
                None
            }
            None => None,
        };
        if let Some(run) = cancelled.and_then(|(project, run)| self.runs.remove(&project, &run)) {
            // Only what that process created: a directory of the same name
            // from anyone else is left alone.
            let owner = proc.endpoint.as_str().as_bytes();
            for dir in [&run.plan.dir, &run.plan.workers_dir] {
                let owned = observe::read_regular_beneath(&self.options.out, &dir.join(OWNER_FILE), 256)
                    .is_ok_and(|o| o.as_deref() == Some(owner));
                if owned && let Err(e) = std::fs::remove_dir_all(dir) {
                    eprintln!("nucleus-discovery: cannot remove {}: {e}", dir.display());
                }
            }
        }
        // A project being forgotten goes on once its runs' processes are gone.
        self.settle_forgetting(k, &proc.project)?;
        self.schedule(k)
    }
}

/// Provides `discovery`, `explore` and the report endpoint and serves them
/// until the kernel terminates this process. Every run's process is a child
/// of this one and the kernel's to tear down with it.
pub fn serve(options: &Options) -> Result<(), String> {
    options.check_paths()?;
    if options.project.is_some() {
        return Err("--project is a run's process's: the service serves every project".into());
    }
    if let Some(projects) = &options.projects
        && !projects.is_dir()
    {
        return Err(format!("--projects {}: not a directory this service can see", projects.display()));
    }
    let discovery_name = EndpointName::new(options.discovery_endpoint.as_str()).map_err(|e| e.to_string())?;
    let explore_name = EndpointName::new(options.explore_endpoint.as_str()).map_err(|e| e.to_string())?;
    let explore_decl = explore::interface(explore_name).map_err(|e| e.to_string())?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = Authority::from(kernel.handles(), options)?;
    // What an agent is told of the workers' network is what the workers'
    // shell of a run that does not choose declares of itself, as the kernel
    // lists it: this service states that shell's word, never one of its own.
    let tools = kernel.tools().map_err(|e| e.to_string())?.unwrap_or_default();
    let declared = |endpoint: Option<&str>| {
        tools
            .iter()
            .find(|tool| Some(tool.endpoint.as_str()) == endpoint)
            .and_then(|tool| shell::network_of(&tool.endpoint_description))
    };
    let default_shell = if options.worker_network {
        if authority.network_shell.is_none() {
            return Err("--worker-network on needs `exec` on --network-shell-endpoint, the workers' shell \
                        that has network"
                .into());
        }
        options.network_shell_endpoint.as_deref()
    } else {
        Some(options.shell_endpoint.as_str())
    };
    let choice = authority.shell.is_some() && authority.network_shell.is_some();
    // Where a task's files are, as this service has them: in the VM those
    // are the VM's own paths, which an agent's shell can read.
    let environment = format!(
        "{} A task's `problems_file` and `prompt_template_file` are relative to this service's tasks \
         directory, {}, and its `policy` to its policies directory, {}.",
        discovery::environment(declared(default_shell), choice),
        options.tasks_dir.display(),
        options.policies_dir.display()
    );
    let discovery_decl =
        discovery::interface(discovery_name, &options.ceilings(), environment).map_err(|e| e.to_string())?;
    let report = BTreeSet::from([ActionName::new(wire::ACTION_REPORT).map_err(|e| e.to_string())?]);
    let created = kernel
        .create_endpoint(authority.create, report)
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot allocate the report endpoint: {:?}: {}", e.code, e.message))?;
    let report_decl = wire::report_interface(created.endpoint.clone()).map_err(|e| e.to_string())?;
    // Dispatches do not name their endpoint; the action routes them.
    let mut seen = BTreeSet::new();
    for decl in [&discovery_decl, &explore_decl, &report_decl] {
        if let Some(shared) = decl.actions.keys().find(|a| !seen.insert((*a).clone())) {
            return Err(format!("two of the service's interfaces declare `{}`", shared.as_str()));
        }
    }
    for (handle, declared) in [
        (created.provide.handle, report_decl),
        (authority.provide_discovery, discovery_decl),
        (authority.provide_explore, explore_decl),
    ] {
        let name = declared.endpoint.clone();
        kernel
            .provide(handle, declared)
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("cannot provide `{name}`: {:?}: {}", e.code, e.message))?;
    }
    let mut runs = Registry::default();
    history::restore(options, &mut runs);
    let service = Service::new(options.clone(), authority, created.endpoint, created.invoke.handle, runs);
    let service = Rc::new(RefCell::new(service));
    // Calls are served as they arrive, also while `next_answer` waits. While
    // the loop below holds the service for an answer (its kernel requests can
    // meet new events), an event waits in the backlog for the next turn.
    let serving = Rc::clone(&service);
    kernel.set_event_handler(move |kernel, event| {
        let Ok(mut service) = serving.try_borrow_mut() else { return Ok(false) };
        match event {
            Event::Dispatch(d) => service.dispatch(kernel, d)?,
            Event::CancelDispatch(dispatch) => service.cancel_dispatch(kernel, *dispatch)?,
            Event::ChildExited(child) => service.child_exited(kernel, *child)?,
            Event::ChildReaped { child, cleanup_confirmed: true, .. } => {
                service.child_reaped(kernel, *child)?
            }
            Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(false),
        }
        Ok(true)
    });
    loop {
        let outcome = match kernel.next_answer() {
            Ok(Some((request, result))) => service.borrow_mut().answer(&mut kernel, request, result),
            Ok(None) => Ok(()),
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => Err(e),
        };
        outcome.map_err(|e| e.to_string())?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{task, world};
    use serde_json::{Value, json};

    #[derive(Debug, PartialEq)]
    enum Sent {
        Submit(RequestId, &'static str, String),
        Cancel(RequestId),
        Reply(DispatchId, Result<Value, ErrorCode>),
        Kill(ProcessId),
        Spawn(ProcessId),
    }

    /// A kernel that records what the service asks of it and answers every
    /// request at once: fresh endpoints, fresh processes, fresh request ids.
    #[derive(Default)]
    struct Recorder {
        next: u64,
        sent: Vec<Sent>,
        /// The shell each spawned run's process was told (`--shell-endpoint`,
        /// the last one given) and the handles its grants came from.
        shells: Vec<(Option<String>, Vec<HandleId>)>,
    }

    impl Recorder {
        fn fresh(&mut self) -> u64 {
            self.next += 1;
            self.next
        }

        /// What was sent since the last look.
        fn take(&mut self) -> Vec<Sent> {
            std::mem::take(&mut self.sent)
        }
    }

    const ACTIONS: [&str; 4] =
        [wire::ACTION_ADMIT, wire::ACTION_BEGIN, wire::ACTION_FORWARD, lean::ACTION_FORGET];

    impl Kernel for Recorder {
        fn submit(
            &mut self,
            _: HandleId,
            action: &str,
            resource: &ResourcePath,
            _: &Payload,
            _: u64,
        ) -> Reply<RequestId> {
            let request = RequestId(self.fresh());
            let action = ACTIONS.into_iter().find(|a| *a == action).unwrap();
            self.sent.push(Sent::Submit(request, action, resource.segments().join("/")));
            Ok(Ok(request))
        }
        fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
            self.sent.push(Sent::Cancel(request));
            Ok(())
        }
        fn reply(
            &mut self,
            dispatch: DispatchId,
            result: Result<Payload, Error>,
        ) -> Result<(), ChannelError> {
            let result = result.map(|p| p.decode::<Value>().unwrap()).map_err(|e| e.code);
            self.sent.push(Sent::Reply(dispatch, result));
            Ok(())
        }
        fn kill(&mut self, child: ProcessId) -> Reply<()> {
            self.sent.push(Sent::Kill(child));
            Ok(Ok(()))
        }
        fn close(&mut self, _: HandleId) -> Reply<()> {
            Ok(Ok(()))
        }
        fn rebind(&mut self, _: HandleId) -> Reply<Generation> {
            Ok(Ok(Generation(1)))
        }
        fn create_endpoint(&mut self, _: HandleId, actions: BTreeSet<ActionName>) -> Reply<CreatedEndpoint> {
            let n = self.fresh();
            let endpoint = EndpointName::new(format!("dynamic.{n}")).unwrap();
            let info = |handle, actions| HandleInfo {
                handle: HandleId(handle),
                endpoint: endpoint.clone(),
                actions,
                resource: ResourcePath::root(),
                delegable: true,
                expires_in_ms: None,
                max_calls: None,
            };
            let provide = info(
                u32::try_from(n * 2).unwrap(),
                BTreeSet::from([ActionName::new(ACTION_PROVIDE).unwrap()]),
            );
            Ok(Ok(CreatedEndpoint {
                endpoint: endpoint.clone(),
                provide,
                invoke: info(u32::try_from(n * 2 + 1).unwrap(), actions),
            }))
        }
        fn spawn_executable(
            &mut self,
            _: HandleId,
            program: ChildProgram,
            grants: Vec<Grant>,
        ) -> Reply<ProcessId> {
            let process = ProcessId(self.fresh());
            assert_eq!(program.args[..3], ["run", "--protocol", &wire::PROTOCOL.to_string()]);
            let told =
                program.args.windows(2).rev().find(|a| a[0] == "--shell-endpoint").map(|a| a[1].clone());
            self.shells.push((told, grants.iter().map(|g| g.from).collect()));
            self.sent.push(Sent::Spawn(process));
            Ok(Ok(process))
        }
    }

    fn call(id: u64, caller: u64, action: &str, resource: &[&str], payload: Value) -> Dispatch {
        Dispatch {
            id: DispatchId(id),
            endpoint: nucleus_protocol::EndpointName::new("discovery").unwrap(),
            caller: ProcessId(caller),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            payload: Payload::encode(&payload).unwrap(),
            remaining_ms: 600_000,
        }
    }

    struct World {
        dir: PathBuf,
        service: Service,
        kernel: Recorder,
        next_dispatch: u64,
    }

    impl World {
        /// A service in a scratch directory of its own, `name`.
        fn new(name: &str, max_running_runs: usize) -> Self {
            let (dir, mut options) = world(name);
            options.run_executable = Some(options.prover_executable.clone());
            options.max_running_runs = max_running_runs;
            let model = HandleInfo {
                handle: HandleId(5),
                endpoint: EndpointName::new(model::ENDPOINT).unwrap(),
                actions: BTreeSet::from([ActionName::new(model::ACTION_COMPLETE).unwrap()]),
                resource: ResourcePath::new(["deepseek"]).unwrap(),
                delegable: true,
                expires_in_ms: None,
                max_calls: None,
            };
            let authority = Authority {
                provide_discovery: HandleId(1),
                provide_explore: HandleId(2),
                create: HandleId(3),
                spawn: HandleId(4),
                model,
                streaming: None,
                lean: HandleId(6),
                register: HandleId(7),
                forget: Some(HandleId(10)),
                explore: HandleId(8),
                shell: None,
                network_shell: None,
            };
            let report = EndpointName::new("dynamic.999").unwrap();
            let service = Service::new(options, authority, report, HandleId(9), Registry::default());
            World { dir, service, kernel: Recorder::default(), next_dispatch: 0 }
        }

        fn serve(&mut self, caller: u64, action: &str, resource: &[&str], payload: Value) -> DispatchId {
            self.next_dispatch += 1;
            let d = call(self.next_dispatch, caller, action, resource, payload);
            self.service.dispatch(&mut self.kernel, &d).unwrap();
            d.id
        }

        /// The key of the run `process` runs.
        fn key(&self, process: ProcessId) -> String {
            self.service.procs[&process].key.clone()
        }

        /// `start` of a one-problem run of project `p` with `w` 2: its
        /// process, and the `admit` it was asked once ready.
        fn start(&mut self, run: &str) -> (DispatchId, ProcessId, RequestId) {
            self.start_in("p", run)
        }

        fn start_in(&mut self, project: &str, run: &str) -> (DispatchId, ProcessId, RequestId) {
            let task = Task {
                name: run.into(),
                problems: Some(vec!["putnam_1962_b2".into()]),
                problems_file: None,
                prompt_template: Some("{{ statement }}".into()),
                prompt_template_file: None,
                policy: None,
                concurrency: 1,
                ..task()
            };
            self.kernel.take();
            let start =
                self.serve(1, discovery::ACTION_START, &[project], serde_json::to_value(&task).unwrap());
            let sent = self.kernel.take();
            let [Sent::Spawn(process)] = sent[..] else { panic!("one process per run: {sent:?}") };
            let key = self.key(process);
            self.serve(process.0, wire::ACTION_REPORT, &[&key], json!({"v": wire::PROTOCOL}));
            let sent = self.kernel.take();
            let admit = sent.iter().find_map(|s| match s {
                Sent::Submit(request, wire::ACTION_ADMIT, resource) if *resource == key => Some(*request),
                _ => None,
            });
            (start, process, admit.expect("asked to admit once ready"))
        }

        fn admitted(&mut self, run: &str, admit: RequestId) {
            let plan = self.service.runs.get("p", run).unwrap().plan.clone();
            let admitted = wire::Admitted {
                v: wire::PROTOCOL,
                status: Progress::new(plan).status(Spend::default()),
                uncertain_model_calls: 0,
            };
            self.service.answer(&mut self.kernel, admit, wire::encode(&admitted)).unwrap();
        }

        /// A run started, admitted and begun or queued.
        fn admit(&mut self, run: &str) -> (DispatchId, ProcessId) {
            let (start, process, admit) = self.start(run);
            self.admitted(run, admit);
            (start, process)
        }

        fn state(&self, run: &str) -> RunState {
            self.service.runs.get("p", run).unwrap().status.state
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The service never waits on a run's process, speaks for a run only
    /// through the process it started for it, and frees a run's slot only
    /// once the kernel confirms that process gone.
    #[test]
    fn one_runs_process_never_holds_up_another_run_and_a_stop_kills_exactly_its_process() {
        let mut w = World::new("service_runs", 2);
        let (start_a, r_a) = w.admit("a");
        let key_a = w.key(r_a);
        let sent = w.kernel.take();
        assert!(
            sent.contains(&Sent::Reply(start_a, Ok(json!({"run": "a", "problems": 1, "queued_ahead": 0}))))
        );
        assert!(
            sent.iter().any(|s| matches!(s, Sent::Submit(_, wire::ACTION_BEGIN, r) if *r == key_a)),
            "{sent:?}"
        );
        let (_, r_b) = w.admit("b");
        let (_, r_e) = w.admit("e");
        let (_, r_f) = w.admit("f");
        assert_eq!((w.state("e"), w.state("f")), (RunState::Queued, RunState::Queued), "two run at once");
        w.kernel.take();

        // Run c's admission is never answered, and nothing waits for it:
        // a's worker is forwarded, `status` and `list` are answered, and a
        // `stop` of c is exactly the kill of c's process.
        let (start_c, r_c, _) = w.start("c");
        let a_task = [key_a.as_str(), "putnam_1962_b2", "t000001"];
        let forwarded = w.serve(501, explore::ACTION_READ_PLANS, &a_task, json!({}));
        let status = w.serve(1, discovery::ACTION_STATUS, &["p", "a"], json!({}));
        let list = w.serve(1, discovery::ACTION_LIST, &[], json!({}));
        let sent = w.kernel.take();
        let forward_a = format!("{key_a}/putnam_1962_b2/t000001");
        assert!(
            sent.iter().any(|s| matches!(s, Sent::Submit(_, wire::ACTION_FORWARD, r) if *r == forward_a)),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .any(|s| matches!(s, Sent::Reply(d, Ok(s)) if *d == status && s["state"] == "running")),
            "{sent:?}"
        );
        let listed = sent
            .iter()
            .find_map(|s| match s {
                Sent::Reply(d, Ok(listed)) if *d == list => Some(listed),
                _ => None,
            })
            .unwrap();
        let mut runs: Vec<&str> =
            listed["runs"].as_array().unwrap().iter().map(|r| r["run"].as_str().unwrap()).collect();
        runs.sort_unstable();
        assert_eq!(runs, ["a", "b", "e", "f"], "a start being admitted is not shown");
        assert!(listed["runs"].as_array().unwrap().iter().all(|r| r["project"] == "p"), "{listed}");
        let stop = w.serve(1, discovery::ACTION_STOP, &["p", "c"], json!({}));
        let sent = w.kernel.take();
        for expected in [
            Sent::Reply(start_c, Err(ErrorCode::ServiceError)),
            Sent::Kill(r_c),
            Sent::Reply(stop, Ok(json!({}))),
        ] {
            assert!(sent.contains(&expected), "{expected:?} in {sent:?}");
        }
        assert!(
            !sent.iter().any(|s| matches!(s, Sent::Kill(p) if *p != r_c)),
            "exactly c's process: {sent:?}"
        );
        assert_ne!(forwarded, stop);

        // The forward cap refuses only the busy run: a has w × concurrency = 2 calls in progress.
        w.serve(502, explore::ACTION_READ_PLANS, &a_task, json!({}));
        let refused = w.serve(503, explore::ACTION_READ_PLANS, &a_task, json!({}));
        let key_b = w.key(r_b);
        w.serve(601, explore::ACTION_ASSIGNMENT, &[&key_b, "putnam_1962_b2", "t000001"], json!({}));
        let sent = w.kernel.take();
        assert!(sent.contains(&Sent::Reply(refused, Err(ErrorCode::ServiceError))), "{sent:?}");
        assert_eq!(sent.iter().filter(|s| matches!(s, Sent::Submit(_, wire::ACTION_FORWARD, _))).count(), 2);

        // Only a run's own process reports for it.
        let running = |w: &World| wire::Report {
            v: wire::PROTOCOL,
            status: Some(
                Progress::new(w.service.runs.get("p", "a").unwrap().plan.clone()).status(Spend::default()),
            ),
            started: 0,
            live: vec![],
        };
        let mut report = serde_json::to_value(running(&w)).unwrap();
        report["status"]["state"] = json!("running");
        let forged = w.serve(r_b.0, wire::ACTION_REPORT, &[&key_a], report.clone());
        let own = w.serve(r_a.0, wire::ACTION_REPORT, &[&key_a], report);
        let sent = w.kernel.take();
        assert!(sent.contains(&Sent::Reply(forged, Err(ErrorCode::PermissionDenied))), "{sent:?}");
        assert!(sent.iter().any(|s| matches!(s, Sent::Reply(d, Ok(_)) if *d == own)), "{sent:?}");

        // Run a's process ends with no last report: a has failed, b runs
        // on, and a cannot be resumed until the kernel confirms the cleanup.
        w.service.child_exited(&mut w.kernel, r_a).unwrap();
        assert_eq!(w.state("a"), RunState::Failed);
        assert_eq!(
            w.service.runs.get("p", "a").unwrap().status.error.as_deref(),
            Some("the run's process ended")
        );
        assert_eq!((w.state("b"), w.state("e")), (RunState::Running, RunState::Queued));
        let early = w.serve(1, discovery::ACTION_RESUME, &["p", "a"], json!({}));
        assert!(w.kernel.take().contains(&Sent::Reply(early, Err(ErrorCode::ServiceError))));

        // Its slot goes to the run first in line once the kernel
        // confirms the cleanup — e, admitted before f — and a may resume.
        w.service.child_reaped(&mut w.kernel, r_a).unwrap();
        let sent = w.kernel.take();
        let key_e = w.key(r_e);
        assert!(
            sent.iter().any(|s| matches!(s, Sent::Submit(_, wire::ACTION_BEGIN, r) if *r == key_e)),
            "{sent:?}"
        );
        assert_eq!(w.state("f"), RunState::Queued);
        w.serve(1, discovery::ACTION_RESUME, &["p", "a"], json!({}));
        let resumed = spawned(&w.kernel.take()).expect("a resumes in a process of its own");
        assert_eq!(w.key(resumed), key_a, "and keeps its key: Lean keeps its statements");
        assert!(r_f.0 > r_b.0);

        // A run's name is its project's: another project starts a run of
        // the same name, and each project sees only its own.
        let (_, r_qa, _) = w.start_in("q", "a");
        assert_ne!(w.key(r_qa), key_a);
        let only_q = w.serve(1, discovery::ACTION_LIST, &["q"], json!({}));
        let unknown = w.serve(1, discovery::ACTION_STATUS, &["q", "b"], json!({}));
        let sent = w.kernel.take();
        assert!(
            sent.contains(&Sent::Reply(only_q, Ok(json!({"runs": []})))),
            "q's a is still being admitted"
        );
        assert!(sent.contains(&Sent::Reply(unknown, Err(ErrorCode::InvalidInput))), "{sent:?}");

        // A tool-loop task whose template has no `context_reset` mode is
        // refused before any process starts or Lean registers anything.
        w.service.authority.shell = Some((HandleId(10), ResourcePath::root()));
        let old_template = Task { name: "h".into(), concurrency: 1, ..task() };
        let refused =
            w.serve(1, discovery::ACTION_START, &["p"], serde_json::to_value(&old_template).unwrap());
        let sent = w.kernel.take();
        assert!(sent.contains(&Sent::Reply(refused, Err(ErrorCode::InvalidInput))), "{sent:?}");
        assert_eq!(spawned(&sent), None, "no process started");
        w.service.authority.shell = None;

        // A process of another build is refused as such and killed.
        let (start_g, r_g, _) = {
            let task = Task { name: "g".into(), concurrency: 1, ..task() };
            let start = w.serve(1, discovery::ACTION_START, &["p"], serde_json::to_value(&task).unwrap());
            (start, spawned(&w.kernel.take()).unwrap(), ())
        };
        let key_g = w.key(r_g);
        let other =
            w.serve(r_g.0, wire::ACTION_REPORT, &[&key_g], json!({"v": wire::PROTOCOL + 1, "later": true}));
        let sent = w.kernel.take();
        for expected in [
            Sent::Reply(other, Err(ErrorCode::ServiceError)),
            Sent::Reply(start_g, Err(ErrorCode::ServiceError)),
            Sent::Kill(r_g),
        ] {
            assert!(sent.contains(&expected), "{expected:?} in {sent:?}");
        }
    }

    /// The process `sent` spawned, if any.
    fn spawned(sent: &[Sent]) -> Option<ProcessId> {
        sent.iter().find_map(|s| match s {
            Sent::Spawn(process) => Some(*process),
            _ => None,
        })
    }

    /// `forget` stops the project's runs and answers only once the kernel
    /// confirmed their processes gone, the records are removed and `lean`
    /// dropped each run's statements; another project's run goes on, and a
    /// start in the project meanwhile is refused.
    #[test]
    fn a_forgotten_project_s_runs_go_once_their_processes_are_gone() {
        let mut w = World::new("service_forget", 2);
        let (_, r_a) = w.admit("a");
        let key_a = w.key(r_a);
        let (start_b, r_b, _) = w.start_in("q", "b");
        let records = w.service.options.out.join("p");
        assert!(records.is_dir());
        w.kernel.take();
        let forget = w.serve(1, discovery::ACTION_FORGET, &["p"], json!({}));
        assert_eq!(w.kernel.take(), [Sent::Kill(r_a)], "no answer while a's process may still write");
        let task = serde_json::to_value(Task { name: "c".into(), concurrency: 1, ..task() }).unwrap();
        let refused = w.serve(1, discovery::ACTION_START, &["p"], task);
        assert_eq!(w.kernel.take(), [Sent::Reply(refused, Err(ErrorCode::ServiceError))]);
        w.service.child_exited(&mut w.kernel, r_a).unwrap();
        w.service.child_reaped(&mut w.kernel, r_a).unwrap();
        let sent = w.kernel.take();
        let [Sent::Submit(request, lean::ACTION_FORGET, ref key)] = sent[..] else { panic!("{sent:?}") };
        assert_eq!(*key, key_a, "the run's statements, by its key");
        assert!(!records.exists() && w.service.runs.get("p", "a").is_none(), "its records and its entry");
        w.service.answer(&mut w.kernel, request, Payload::encode(&json!({}))).unwrap();
        assert_eq!(w.kernel.take(), [Sent::Reply(forget, Ok(json!({})))]);
        assert!(w.service.procs.contains_key(&r_b), "q's run is another project's");
        assert_ne!(start_b, forget);
        // Nothing left to forget is answered at once; anything but a project is refused.
        let again = w.serve(1, discovery::ACTION_FORGET, &["p"], json!({}));
        let bad = w.serve(1, discovery::ACTION_FORGET, &["p", "a"], json!({}));
        assert_eq!(
            w.kernel.take(),
            [Sent::Reply(again, Ok(json!({}))), Sent::Reply(bad, Err(ErrorCode::InvalidInput))]
        );
    }

    #[test]
    fn a_project_stop_waits_for_reaping_and_keeps_its_runs_and_records() {
        let mut w = World::new("service_archive", 2);
        let (_, process) = w.admit("a");
        let (_, other, _) = w.start_in("q", "b");
        let records = w.service.options.out.join("p");
        w.kernel.take();
        let stop = w.serve(1, discovery::ACTION_STOP, &["p"], json!({}));
        assert_eq!(w.kernel.take(), [Sent::Kill(process)]);
        w.service.child_exited(&mut w.kernel, process).unwrap();
        assert!(w.kernel.take().is_empty(), "an exit does not establish cleanup");
        w.service.child_reaped(&mut w.kernel, process).unwrap();
        assert_eq!(w.kernel.take(), [Sent::Reply(stop, Ok(json!({})))]);
        assert!(records.is_dir() && w.service.runs.get("p", "a").is_some());
        assert!(w.service.procs.contains_key(&other));
    }

    /// With projects, only a project that has a home has runs, and the
    /// directory of its runs is the service's own to make.
    #[test]
    fn with_projects_only_a_project_that_exists_has_runs() {
        let mut w = World::new("service_projects", 1);
        let projects = w.service.options.out.join("homes");
        std::fs::create_dir(&projects).unwrap();
        w.service.options.projects = Some(projects.clone());
        let task = serde_json::to_value(Task { name: "r".into(), concurrency: 1, ..task() }).unwrap();
        let refused = w.serve(1, discovery::ACTION_START, &["p"], task.clone());
        assert_eq!(w.kernel.take(), [Sent::Reply(refused, Err(ErrorCode::InvalidInput))]);
        assert!(!w.service.options.out.join("p").exists(), "no directory for a project that is not there");
        std::fs::create_dir(projects.join("p")).unwrap();
        w.serve(1, discovery::ACTION_START, &["p"], task);
        assert!(matches!(w.kernel.take()[..], [Sent::Spawn(_)]));
        assert!(w.service.options.out.join("p").is_dir());
    }

    /// A run's workers have network only when its task says so (or the
    /// service's default does), and then only in the workers' shell that has
    /// it, which the service must hold: the run's process is granted that
    /// shell and no other, and told its name. The recorded task keeps the
    /// choice.
    #[test]
    fn a_runs_workers_get_the_shell_with_network_only_when_its_task_chooses_it() {
        let mut w = World::new("service_network", 2);
        let (offline, online) = (HandleId(910), HandleId(912));
        w.service.authority.shell = Some((offline, ResourcePath::root()));
        w.service.options.network_shell_endpoint = Some("prover-shell-net".into());
        let agent = Task {
            prompt_template: Some("{{ statement }}{% if mode == \"context_reset\" %}reset{% endif %}".into()),
            prompt_template_file: None,
            concurrency: 1,
            ..task()
        };
        let start = |w: &mut World, name: &str, network: Option<bool>| {
            let task = Task { name: name.into(), worker_network: network, ..agent.clone() };
            let id = w.serve(1, discovery::ACTION_START, &["p"], serde_json::to_value(task).unwrap());
            (id, w.kernel.take())
        };
        // Asked for without a shell that has it: refused, nothing started.
        let (refused, sent) = start(&mut w, "a", Some(true));
        assert_eq!(sent, [Sent::Reply(refused, Err(ErrorCode::InvalidInput))]);
        assert!(w.kernel.shells.is_empty());
        w.service.authority.network_shell = Some((online, ResourcePath::root()));
        for (name, asked, shell, told) in [
            ("b", None, offline, None),
            ("c", Some(false), offline, None),
            ("d", Some(true), online, Some("prover-shell-net")),
        ] {
            let (_, sent) = start(&mut w, name, asked);
            assert!(matches!(sent[..], [Sent::Spawn(_)]), "{name}: {sent:?}");
            let (endpoint, granted) = w.kernel.shells.last().unwrap();
            assert_eq!(endpoint.as_deref(), told, "{name}");
            let other = if shell == offline { online } else { offline };
            assert!(granted.contains(&shell) && !granted.contains(&other), "{name}: {granted:?}");
        }
        let recorded =
            |w: &World, name: &str| w.service.runs.get("p", name).unwrap().plan.document.worker_network;
        assert_eq!((recorded(&w, "b"), recorded(&w, "d")), (Some(false), Some(true)));
    }

    /// What the service has decided about a run, a late word from its
    /// process does not undo; what the service removes is only its own
    /// process's; and past its bound on calls in progress it refuses a call
    /// rather than end its channel with every run.
    #[test]
    fn a_runs_end_stands_and_the_service_removes_and_asks_only_what_it_may() {
        let mut w = World::new("service_ends", 1);
        // A `begin` that fails after a stop: the run stays stopped.
        let (_, r_x) = w.admit("x");
        let begin = w.kernel.take().into_iter().find_map(|s| match s {
            Sent::Submit(request, wire::ACTION_BEGIN, _) => Some(request),
            _ => None,
        });
        w.serve(1, discovery::ACTION_STOP, &["p", "x"], json!({}));
        let gone = Err(Error::new(ErrorCode::ServiceError, "the process is gone"));
        w.service.answer(&mut w.kernel, begin.unwrap(), gone).unwrap();
        assert_eq!(w.state("x"), RunState::Stopped);
        w.service.child_reaped(&mut w.kernel, r_x).unwrap();

        // Past the bound, a worker's call is refused and nothing is sent to its run.
        let (_, r_y) = w.admit("y");
        for n in 0..MAX_CALLS_IN_PROGRESS as u64 {
            w.service.calls.insert(RequestId(1_000_000 + n), Call::Admit { process: ProcessId(0) });
        }
        w.kernel.take();
        let key_y = w.key(r_y);
        let refused =
            w.serve(501, explore::ACTION_READ_PLANS, &[&key_y, "putnam_1962_b2", "t000001"], json!({}));
        assert_eq!(w.kernel.take(), [Sent::Reply(refused, Err(ErrorCode::ServiceError))]);
        w.service.calls.retain(|request, _| request.0 < 1_000_000);

        // A stop after the run's process reported its end keeps that end.
        let plan = w.service.runs.get("p", "y").unwrap().plan.clone();
        let status = Progress::new(plan).status(Spend::default());
        let mut done = serde_json::to_value(wire::Report {
            v: wire::PROTOCOL,
            status: Some(status),
            started: 1,
            live: vec![],
        })
        .unwrap();
        done["status"]["state"] = json!("done");
        w.serve(r_y.0, wire::ACTION_REPORT, &[&key_y], done);
        w.serve(1, discovery::ACTION_STOP, &["p", "y"], json!({}));
        assert_eq!(w.state("y"), RunState::Done);
        assert!(w.kernel.take().contains(&Sent::Kill(r_y)));

        // A start that did not happen: its process's directory goes, a
        // directory of the same name that anyone else made stays.
        let (_, r_z, _) = w.start("z");
        let (owner, out) = (w.service.procs[&r_z].endpoint.clone(), w.service.options.out.join("p"));
        std::fs::create_dir(out.join("z")).unwrap();
        std::fs::create_dir(out.join("z.workers")).unwrap();
        std::fs::write(out.join("z.workers").join(OWNER_FILE), owner.as_str()).unwrap();
        w.serve(1, discovery::ACTION_STOP, &["p", "z"], json!({}));
        w.service.child_reaped(&mut w.kernel, r_z).unwrap();
        assert!(out.join("z").is_dir() && !out.join("z.workers").exists());
    }
}
