//! Routing the window's and the session agents' calls to the projects'
//! hosts. One thread serves every endpoint: a call is planned
//! ([`Router::dispatch`]), forwarded with the SDK's `submit` to the host of
//! its project and interface (started on demand, as the project), and
//! answered when the host answers ([`Router::answer`]). A host says when it serves (`ready`, on a private
//! endpoint); calls that arrive before wait for it. Hosts with no call for a
//! while are stopped, never one with a call in flight.
//!
//! **Projects are folders**: with projects' processes, a
//! creation starts the project's host to make the folder and the
//! repository in it, as the projects' user, and a failure deletes the
//! project again. A deletion is two steps here, `delete` and `purge`; what
//! the other services keep of the project they forget between them, asked by
//! the caller (the UI service), never by this service, which starts before
//! them and uses none of them. `delete` gives the folder its deleting name
//! (none of the project starts from then on), forgets its jobs and stops its
//! hosts. `purge` has the kernel end every process of the project with its
//! cgroup (`stop_project`), and the project's host remove the folder; it is
//! answered once the folder is gone. A project's Stop is that `stop_project`
//! alone. The kernel answers a stop when nothing of the project runs, as it
//! answers forwarded calls. How many projects have processes at once is the
//! supervisor's to bound: it refuses a process of one project too many.
//!
//! **Background jobs** (module `jobs`): a session's agent's
//! command can go on running after its call, in the host it runs in, which
//! is then not stopped while the job runs; the session is woken when it
//! ends.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use nucleus_projects_interface as projects;
use nucleus_projects_interface::{Dir, Project, ProjectList};
use nucleus_protocol::{
    ACTION_PROVIDE, ActionName, ChildProgram, CreatedEndpoint, DispatchId, EndpointName, Error, ErrorCode,
    Generation, Grant, HandleId, IdentityRequest, Payload, ProcessId, ProjectStopped, RequestId,
    ResourcePath,
};
use nucleus_sdk::{ACTION_READY, ChannelError, Dispatch, Reply};

use crate::hosts::{self, HostConfig, Kind};
use crate::registry::Registry;

mod jobs;

/// Kept back from the caller's deadline so the answer reaches it in time.
const MARGIN: Duration = Duration::from_millis(250);
/// How long the host is given to remove a deleted project's folder.
const REMOVE_MS: u64 = 600_000;
/// Most calls forwarded at once. The router is one caller to the kernel for
/// all of them: the composition's per-caller limit must allow this many.
/// Room for every active project (the composition's `max_active` of them) to
/// have about ten calls on their way.
pub const MAX_FORWARDS: usize = 1024;

/// What the router asks of the kernel; the SDK connection in service, a
/// recorder in tests.
pub trait Kernel {
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError>;
    fn submit(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline_ms: Option<u64>,
    ) -> Reply<RequestId>;
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError>;
    fn create_endpoint(
        &mut self,
        authority: HandleId,
        actions: BTreeSet<ActionName>,
    ) -> Reply<CreatedEndpoint>;
    fn spawn(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        identity: Option<IdentityRequest>,
    ) -> Reply<ProcessId>;
    fn kill(&mut self, child: ProcessId) -> Reply<()>;
    fn rebind(&mut self, handle: HandleId) -> Reply<Generation>;
    fn close(&mut self, handle: HandleId) -> Reply<()>;
    /// Asks for every process of a project to be ended; the answer comes as
    /// a submitted call's does.
    fn stop_project(&mut self, authority: HandleId, project: &str) -> Reply<RequestId>;
}

/// The router's own authority.
pub struct Authority {
    /// `kernel create_endpoint`: each host's endpoint.
    pub create: HandleId,
    /// `kernel spawn_executable` on the host executables.
    pub spawn: HandleId,
    /// `kernel spawn_as` and `kernel stop_project` on every project; `None`
    /// in a composition without projects' processes, where hosts run as the
    /// router's own identity and folders are its plain directories.
    pub spawn_as: Option<HandleId>,
    pub stop: Option<HandleId>,
    /// The private endpoint hosts say `ready` at, and the router's invoking
    /// handle on it, from which each host's `ready` is narrowed.
    pub ready: (EndpointName, HandleId),
    /// `conversation wake` on every session: a background job's end wakes
    /// its session. Required where `jobs` is served.
    pub wake: Option<HandleId>,
}

/// What happens with a host's answer before the caller gets it.
#[derive(Clone, Debug)]
enum Then {
    Verbatim,
    Created {
        project: String,
    },
    // A piece of a job's output, its stop, a person's move of a command to
    // the background (module `jobs`).
    JobOutput {
        job: String,
    },
    JobStop {
        job: String,
    },
    JobDetach {
        job: String,
    },
    /// The removal of a deleted project's folder, the router's own call.
    Removed {
        project: String,
    },
}

/// The dispatch of the router's own calls to a host, which no caller waits
/// for: the kernel numbers dispatches from 1.
const OWN: DispatchId = DispatchId(0);

/// A call for a host.
#[derive(Debug)]
struct Call {
    dispatch: DispatchId,
    action: String,
    resource: ResourcePath,
    payload: Payload,
    deadline: Instant,
    then: Then,
    /// A registered agent's command: the job it may become (module `jobs`).
    job: Option<String>,
}

type HostKey = (String, Kind);

struct Host {
    process: ProcessId,
    invoke: HandleId,
    provide: HandleId,
    ready_key: String,
    ready: bool,
    queued: Vec<Call>,
    inflight: usize,
    last_used: Instant,
}

struct Forward {
    dispatch: DispatchId,
    host: HostKey,
    then: Then,
    job: Option<String>,
}

/// A project being created, and who asked.
struct Creation {
    dispatch: DispatchId,
    deadline: Instant,
}

/// A project stop on its way: a person's, or a purge's.
enum Stopping {
    Stop(DispatchId),
    Purge(String),
}

pub struct Router {
    registry: Registry,
    config: HostConfig,
    authority: Authority,
    idle: Duration,
    hosts: BTreeMap<HostKey, Host>,
    ready_keys: HashMap<String, HostKey>,
    next_key: u64,
    forwards: HashMap<RequestId, Forward>,
    by_dispatch: HashMap<DispatchId, Vec<RequestId>>,
    /// Projects being created; projects being purged, with the `purge`
    /// calls waiting for each (none for this service's own).
    creating: BTreeMap<String, Creation>,
    purging: BTreeMap<String, Vec<DispatchId>>,
    /// Project stops the kernel has not answered yet.
    stops: HashMap<RequestId, Stopping>,
    /// Background jobs (module `jobs`), and those whose sessions are still
    /// to be told how they ended, at this start.
    book: jobs::Book,
    untold: Vec<String>,
}

/// Where a project's discovery workers' workspaces are in its home.
const RUNS: &str = "runs";

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

fn ms(until: Instant) -> u64 {
    u64::try_from(until.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX)
}

/// What a dispatch comes to before anything is sent.
enum Plan {
    Answer(Result<Payload, Error>),
    Create { project: String },
    Stop { project: String },
    Delete { project: String },
    Purge { project: String },
    Forward { project: String, kind: Kind, call: Call },
}

impl Router {
    pub fn new(registry: Registry, config: HostConfig, authority: Authority, idle: Duration) -> Self {
        Self {
            registry,
            config,
            authority,
            idle,
            hosts: BTreeMap::new(),
            ready_keys: HashMap::new(),
            next_key: 1,
            forwards: HashMap::new(),
            by_dispatch: HashMap::new(),
            creating: BTreeMap::new(),
            purging: BTreeMap::new(),
            stops: HashMap::new(),
            book: jobs::Book::default(),
            untold: Vec::new(),
        }
    }

    /// Serves one dispatch at any of the router's endpoints.
    pub fn dispatch(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        if d.endpoint == self.authority.ready.0 {
            return self.ready(k, d);
        }
        if d.endpoint.as_str() == nucleus_shell_interface::jobs::ENDPOINT {
            return self.jobs_call(k, d);
        }
        let deadline = Instant::now() + Duration::from_millis(d.remaining_ms);
        match self.plan(d, deadline) {
            Plan::Answer(result) => k.reply(d.id, result),
            Plan::Create { project } => self.create(k, d.id, deadline, project),
            Plan::Stop { project } => self.stop(k, d.id, project),
            Plan::Delete { project } => self.delete(k, d.id, project),
            Plan::Purge { project } => self.purge(k, d.id, project),
            Plan::Forward { project, kind, mut call } => {
                if (kind == Kind::Shell || kind.workers())
                    && let Err(e) = self.tag_exec(d, kind, &project, &mut call)
                {
                    return k.reply(d.id, Err(e));
                }
                self.forward(k, &project, kind, call)
            }
        }
    }

    fn plan(&self, d: &Dispatch, deadline: Instant) -> Plan {
        match self.planned(d, deadline) {
            Ok(plan) => plan,
            Err(e) => Plan::Answer(Err(e)),
        }
    }

    fn planned(&self, d: &Dispatch, deadline: Instant) -> Result<Plan, Error> {
        let call = |action: &str, resource: Vec<&str>, payload: Payload, then: Then| -> Result<Call, Error> {
            Ok(Call {
                dispatch: d.id,
                action: action.to_owned(),
                resource: ResourcePath::new(resource)?,
                payload,
                deadline,
                then,
                job: None,
            })
        };
        let forward =
            |kind: Kind, dir: &Dir, action: &str, payload: Payload, then: Then| -> Result<Plan, Error> {
                Ok(Plan::Forward {
                    project: dir.project().to_owned(),
                    kind,
                    call: call(action, dir.in_home(), payload, then)?,
                })
            };
        let (endpoint, action) = (d.endpoint.as_str(), d.action.as_str());
        match (endpoint, action) {
            (projects::PROJECTS_ENDPOINT, projects::ACTION_LIST) => {
                let request: projects::ListProjects = d.payload.decode()?;
                // Every project, or the one a narrower caller names, if it exists.
                let only = covered(&d.resource)?;
                let (page, more) =
                    self.registry.page(request.after.as_deref(), request.page()?, only.as_deref())?;
                let next = if more { page.last().map(|p| p.name.clone()) } else { None };
                let projects = page
                    .into_iter()
                    .map(|listed| {
                        Ok(Project {
                            deleting: listed.deleting,
                            archived: listed.archived,
                            archiving: listed.archiving,
                            ..Project::named(&listed.name)?
                        })
                    })
                    .collect::<Result<_, Error>>()?;
                Ok(Plan::Answer(Payload::encode(&ProjectList { projects, next })))
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_CREATE) => {
                d.payload.decode::<projects::Empty>()?;
                let project = whole_project(&d.resource)?;
                Ok(Plan::Create { project })
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_ARCHIVE | projects::ACTION_UNARCHIVE) => {
                d.payload.decode::<projects::Empty>()?;
                let project = whole_project(&d.resource)?;
                self.registry.archive(&project, action == projects::ACTION_ARCHIVE)?;
                Ok(Plan::Answer(Payload::encode(&projects::Empty {})))
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_BEGIN_ARCHIVE) => {
                d.payload.decode::<projects::Empty>()?;
                self.registry.begin_archive(&whole_project(&d.resource)?)?;
                Ok(Plan::Answer(Payload::encode(&projects::Empty {})))
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_STOP) => {
                d.payload.decode::<projects::Empty>()?;
                Ok(Plan::Stop { project: whole_project(&d.resource)? })
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_DELETE) => {
                d.payload.decode::<projects::Empty>()?;
                Ok(Plan::Delete { project: whole_project(&d.resource)? })
            }
            (projects::PROJECTS_ENDPOINT, projects::ACTION_PURGE) => {
                d.payload.decode::<projects::Empty>()?;
                Ok(Plan::Purge { project: whole_project(&d.resource)? })
            }
            (projects::FILES_ENDPOINT, _) => {
                let dir = Dir::from_resource(&d.resource)?;
                forward(Kind::Project, &dir, action, d.payload.clone(), Then::Verbatim)
            }
            ("diffs", _) => {
                let project = whole_project(&d.resource)?;
                let call = call(action, vec![], d.payload.clone(), Then::Verbatim)?;
                Ok(Plan::Forward { project, kind: Kind::Diffs, call })
            }
            // A project's skills are its repository's, nowhere else.
            ("skills", _) => {
                let project = projects::project_of(&d.resource)?.to_owned();
                if d.resource.segments()[2..] != [projects::REPOSITORY] {
                    return Err(invalid(format!(
                        "skills are a repository's: [\"{}\", project, \"{}\"]",
                        projects::PROJECTS,
                        projects::REPOSITORY
                    )));
                }
                let call = call(action, vec![projects::REPOSITORY], d.payload.clone(), Then::Verbatim)?;
                Ok(Plan::Forward { project, kind: Kind::Skills, call })
            }
            ("fs" | "shell" | "pdf" | hosts::PROVER_SHELL | hosts::PROVER_SHELL_NET, _) => {
                let project = projects::project_of(&d.resource)?.to_owned();
                let rest: Vec<&str> = d.resource.segments()[2..].iter().map(String::as_str).collect();
                let kind = match endpoint {
                    "fs" => Kind::Fs,
                    "shell" => Kind::Shell,
                    hosts::PROVER_SHELL => Kind::ProverShell,
                    hosts::PROVER_SHELL_NET => Kind::ProverShellNet,
                    _ => Kind::Pdf,
                };
                // The workers' shell works in a project's runs, nowhere else.
                if kind.workers() && rest.first() != Some(&RUNS) {
                    return Err(invalid(format!("{endpoint} works under {RUNS}/ of a project")));
                }
                // A shell host's job actions are this service's alone.
                if (kind == Kind::Shell || kind.workers()) && action != nucleus_shell_interface::ACTION_EXEC {
                    return Err(invalid(format!("{endpoint} has no action {action}")));
                }
                let call = call(action, rest, d.payload.clone(), Then::Verbatim)?;
                Ok(Plan::Forward { project, kind, call })
            }
            _ => Err(invalid(format!("{endpoint} has no action {action}"))),
        }
    }

    /// A new project: its folder and repository, made by its host as the
    /// projects' user (without projects' processes, the folder is this
    /// service's directory). Until it has its repository the project is not
    /// listed and takes no call, and a restart deletes it.
    fn create(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
        deadline: Instant,
        project: String,
    ) -> Result<(), ChannelError> {
        let begun = (|| {
            if self.creating.contains_key(&project)
                || self.purging.contains_key(&project)
                || self.registry.taken(&project)?
            {
                return Err(invalid(format!("a project {project} exists already, or is being deleted")));
            }
            self.registry.mark_creating(&project)
        })();
        if let Err(e) = begun {
            return k.reply(dispatch, Err(e));
        }
        self.creating.insert(project.clone(), Creation { dispatch, deadline });
        if self.authority.spawn_as.is_some() {
            // Its host makes the folder as it starts.
            return self.initialize(k, &project);
        }
        match self.registry.make_home(&project) {
            Ok(()) => self.initialize(k, &project),
            Err(e) => self.creation_ended(k, &project, Err(e)),
        }
    }

    /// The new project's host makes the repository, in the folder it made
    /// as it started, or this service did.
    fn initialize(&mut self, k: &mut impl Kernel, project: &str) -> Result<(), ChannelError> {
        let Some(creation) = self.creating.get(project) else { return Ok(()) };
        let call = Payload::encode(&projects::Empty {}).map(|payload| Call {
            dispatch: creation.dispatch,
            action: projects::ACTION_INIT.to_owned(),
            resource: ResourcePath::root(),
            payload,
            deadline: creation.deadline,
            then: Then::Created { project: project.to_owned() },
            job: None,
        });
        match call {
            Ok(call) => self.send(k, (project.to_owned(), Kind::Project), call),
            Err(e) => self.unmake(k, project, e),
        }
    }

    /// The creation of `project` ended: made, or not (before its folder
    /// existed, or after it went again). `result` is its caller's answer.
    fn creation_ended(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let Some(creation) = self.creating.remove(project) else { return Ok(()) };
        let result = match self.registry.unmark_creating(project) {
            Ok(()) => result,
            Err(e) => result.and(Err(e)),
        };
        k.reply(creation.dispatch, result)
    }

    /// The repository of the new `project` could not be made: its caller is
    /// told why, and the project is deleted, folder and all, as any other.
    fn unmake(&mut self, k: &mut impl Kernel, project: &str, why: Error) -> Result<(), ChannelError> {
        let begun = match self.registry.begin_delete(project) {
            Ok(begun) => begun,
            Err(e) => {
                let message = format!("{}; removing what was made failed: {}", why.message, e.message);
                return self.creation_ended(k, project, Err(Error::new(ErrorCode::ServiceError, message)));
            }
        };
        self.creation_ended(k, project, Err(why))?;
        if begun { self.remove(k, project, None) } else { Ok(()) }
    }

    /// `stop`: every process of the project ends, whoever started it (its
    /// hosts, its sessions' agents, their plugins, its jobs, its swarms'
    /// members, its workers' commands), with its cgroup; answered once none
    /// is left. What keeps records of them sees them end.
    fn stop(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
        project: String,
    ) -> Result<(), ChannelError> {
        let Some(authority) = self.authority.stop else {
            let none = Error::new(ErrorCode::Unavailable, "this runtime runs no project's processes to stop");
            return k.reply(dispatch, Err(none));
        };
        if let Err(e) = self.registry.stoppable(&project) {
            return k.reply(dispatch, Err(e));
        }
        match k.stop_project(authority, &project)? {
            Ok(request) => drop(self.stops.insert(request, Stopping::Stop(dispatch))),
            Err(e) => return k.reply(dispatch, Err(e)),
        }
        Ok(())
    }

    /// `delete`: the project's folder takes its deleting name, from when on
    /// none of it starts, its job records go (so that their end wakes no
    /// session) and its hosts stop. Answered at once: `deleting`, or
    /// `deleted` when nothing of the project is left. What the other services
    /// keep of it is its caller's to have them forget, before `purge`.
    fn delete(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
        project: String,
    ) -> Result<(), ChannelError> {
        if self.creating.contains_key(&project) {
            let busy = format!("{project} is being created; delete it once its creation has ended");
            return k.reply(dispatch, Err(Error::new(ErrorCode::Unavailable, busy)));
        }
        let state = if self.purging.contains_key(&project) {
            projects::DeletionState::Deleting
        } else {
            match self.registry.begin_delete(&project) {
                Ok(true) => match self.begin(k, &project)? {
                    Ok(()) => projects::DeletionState::Deleting,
                    Err(e) => return k.reply(dispatch, Err(e)),
                },
                Ok(false) => projects::DeletionState::Deleted,
                Err(e) => return k.reply(dispatch, Err(e)),
            }
        };
        k.reply(dispatch, Payload::encode(&projects::Deletion { state }))
    }

    /// `purge`: every process of a project being deleted ends, then its
    /// folder goes ([`Self::remove`]); answered once it is gone, or with why
    /// not, and the project is left being deleted for `purge` again. Asked
    /// while one is under way, answered with it.
    fn purge(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
        project: String,
    ) -> Result<(), ChannelError> {
        if self.creating.contains_key(&project) {
            let busy = format!("{project} is being created; delete it once its creation has ended");
            return k.reply(dispatch, Err(Error::new(ErrorCode::Unavailable, busy)));
        }
        if let Some(waiting) = self.purging.get_mut(&project) {
            waiting.push(dispatch);
            return Ok(());
        }
        match self.registry.begin_delete(&project) {
            Ok(true) => self.remove(k, &project, Some(dispatch)),
            Ok(false) => k.reply(dispatch, Payload::encode(&projects::Empty {})),
            Err(e) => k.reply(dispatch, Err(e)),
        }
    }

    /// The first step of a deletion once the folder has its deleting name:
    /// `project`'s jobs are forgotten, so that their end wakes no session,
    /// and its hosts stopped.
    fn begin(&mut self, k: &mut impl Kernel, project: &str) -> Result<Result<(), Error>, ChannelError> {
        if let Err(e) = self.forget_jobs(project) {
            return Ok(Err(e));
        }
        let hosts: Vec<ProcessId> =
            self.hosts.iter().filter(|(key, _)| key.0 == project).map(|(_, host)| host.process).collect();
        for process in hosts {
            // A refusal means it is gone already. It is forgotten now, so no
            // call of a project of the same name made later reaches it.
            let _ = k.kill(process)?;
            self.child_exited(k, process)?;
        }
        Ok(Ok(()))
    }

    /// Removes `project`, whose folder has its deleting name: its first step
    /// again ([`Self::begin`]), then every process of it ends with its
    /// cgroup and its host removes the folder ([`Self::stopped`]); without
    /// projects' processes, this service removes the folder. `dispatch` is
    /// the `purge` waiting for it, if any.
    fn remove(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
        dispatch: Option<DispatchId>,
    ) -> Result<(), ChannelError> {
        self.purging.insert(project.to_owned(), dispatch.into_iter().collect());
        if let Err(e) = self.begin(k, project)? {
            return self.removed(k, project, Err(e));
        }
        let Some(authority) = self.authority.stop else {
            let result = self.registry.remove_deleted(project);
            return self.removed(k, project, result);
        };
        match k.stop_project(authority, project)? {
            Ok(request) => drop(self.stops.insert(request, Stopping::Purge(project.to_owned()))),
            Err(e) => return self.removed(k, project, Err(e)),
        }
        Ok(())
    }

    /// `project`'s removal ended: the `purge` calls waiting for it are
    /// answered. A failure leaves it being deleted, for `purge` again.
    fn removed(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
        result: Result<(), Error>,
    ) -> Result<(), ChannelError> {
        if let Err(e) = &result {
            eprintln!("nucleus-projects: removing {project} failed: {}", e.message);
        }
        for dispatch in self.purging.remove(project).unwrap_or_default() {
            let answer = result.clone().and_then(|()| Payload::encode(&projects::Empty {}));
            k.reply(dispatch, answer)?;
        }
        Ok(())
    }

    /// The kernel's answer to a project stop: a person's is answered; a
    /// purge's goes on with the folder, which the project's host removes
    /// as the projects' user, started for the folder's deleting name.
    fn stopped(
        &mut self,
        k: &mut impl Kernel,
        stopping: Stopping,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        let result = result.and_then(|p| p.decode::<ProjectStopped>());
        match stopping {
            Stopping::Stop(dispatch) => {
                k.reply(dispatch, result.and_then(|_| Payload::encode(&projects::Empty {})))
            }
            Stopping::Purge(project) => {
                if let Err(e) = result {
                    return self.removed(k, &project, Err(e));
                }
                let call = Payload::encode(&projects::Empty {}).map(|payload| Call {
                    dispatch: OWN,
                    action: projects::ACTION_DELETE.to_owned(),
                    resource: ResourcePath::root(),
                    payload,
                    deadline: Instant::now() + Duration::from_millis(REMOVE_MS),
                    then: Then::Removed { project: project.clone() },
                    job: None,
                });
                match call {
                    Ok(call) => self.send(k, (project, Kind::Project), call),
                    Err(e) => self.removed(k, &project, Err(e)),
                }
            }
        }
    }

    /// Projects whose creation a restart interrupted: each is deleted, as a
    /// failed creation is. Called once, at the start.
    pub fn delete_interrupted(&mut self, k: &mut impl Kernel) -> Result<(), ChannelError> {
        let interrupted = match self.registry.interrupted() {
            Ok(interrupted) => interrupted,
            Err(e) => {
                eprintln!("nucleus-projects: interrupted creations: {}", e.message);
                return Ok(());
            }
        };
        for project in interrupted {
            let begun = self.registry.begin_delete(&project);
            match begun.and_then(|begun| self.registry.unmark_creating(&project).map(|()| begun)) {
                Ok(true) => self.remove(k, &project, None)?,
                // Its folder was never made.
                Ok(false) => {}
                Err(e) => eprintln!(
                    "nucleus-projects: deleting {project}, whose creation was interrupted: {}",
                    e.message
                ),
            }
        }
        Ok(())
    }

    fn forward(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
        kind: Kind,
        call: Call,
    ) -> Result<(), ChannelError> {
        if let Err(e) = self.registry.exists(project) {
            return self.fail(k, call, e);
        }
        self.send(k, (project.to_owned(), kind), call)
    }

    /// Sends `call` to the host `key`, started when it does not run, or
    /// queues it until the host is ready.
    fn send(&mut self, k: &mut impl Kernel, key: HostKey, call: Call) -> Result<(), ChannelError> {
        if self.forwards.len() >= MAX_FORWARDS {
            let busy =
                Error::new(ErrorCode::LimitExceeded, "the projects service forwards too many calls at once");
            return self.fail(k, call, busy);
        }
        if !self.hosts.contains_key(&key) {
            match self.start(k, &key)? {
                Ok(host) => drop(self.hosts.insert(key.clone(), host)),
                Err(e) => return self.fail(k, call, e),
            }
        }
        let Some(host) = self.hosts.get_mut(&key) else { return Ok(()) };
        host.last_used = Instant::now();
        if !host.ready {
            host.queued.push(call);
            return Ok(());
        }
        self.submit(k, key, call)
    }

    fn submit(&mut self, k: &mut impl Kernel, key: HostKey, call: Call) -> Result<(), ChannelError> {
        let Some(host) = self.hosts.get_mut(&key) else {
            return self.fail(k, call, Error::new(ErrorCode::Unavailable, "the project's host ended"));
        };
        let left = call.deadline.checked_sub(MARGIN).map(ms).unwrap_or(0);
        if left == 0 {
            return self.fail(
                k,
                call,
                Error::new(ErrorCode::Timeout, "the call ran out of time waiting for its host"),
            );
        }
        match k.submit(host.invoke, &call.action, &call.resource, &call.payload, Some(left))? {
            Ok(request) => {
                host.inflight += 1;
                self.by_dispatch.entry(call.dispatch).or_default().push(request);
                self.forwards.insert(
                    request,
                    Forward { dispatch: call.dispatch, host: key, then: call.then, job: call.job },
                );
                Ok(())
            }
            Err(e) => self.fail(k, call, e),
        }
    }

    /// Answers `call` with `error`, undoing what its continuation holds.
    fn fail(&mut self, k: &mut impl Kernel, call: Call, error: Error) -> Result<(), ChannelError> {
        if let Some(job) = &call.job {
            self.untag(k, job)?;
        }
        match call.then {
            Then::JobOutput { .. } | Then::JobStop { .. } | Then::JobDetach { .. } => {
                if let Some(Err(e)) = self.job_reply(k, call.then, Err(error))? {
                    return k.reply(call.dispatch, Err(e));
                }
                return Ok(());
            }
            Then::Created { project } => return self.unmake(k, &project, error),
            Then::Removed { project } => return self.removed(k, &project, Err(error)),
            _ => {}
        }
        k.reply(call.dispatch, Err(error))
    }

    /// Starts the host `key`, as its project when projects have processes of
    /// their own: for a project being created, the host that makes its
    /// folder; for one being deleted, the host that removes it.
    fn start(&mut self, k: &mut impl Kernel, key: &HostKey) -> Result<Result<Host, Error>, ChannelError> {
        let actions = match key.1.actions(&self.config) {
            Ok(actions) => actions,
            Err(e) => return Ok(Err(e)),
        };
        let created = match k.create_endpoint(self.authority.create, actions)? {
            Ok(created) => created,
            Err(e) => return Ok(Err(e)),
        };
        let ready_key = self.next_key.to_string();
        self.next_key += 1;
        let plain = self.authority.spawn_as.is_some();
        let folder = if self.purging.contains_key(&key.0) {
            self.registry.home_path(&format!("{}{}", crate::registry::DELETING, key.0))
        } else {
            self.registry.home_path(&key.0)
        };
        let create = plain && key.1 == Kind::Project && self.creating.contains_key(&key.0);
        let program = hosts::program(key.1, &self.config, &folder, &created.endpoint, (plain, create));
        let grants = (|| {
            Ok::<_, Error>(vec![
                Grant {
                    from: created.provide.handle,
                    actions: [ActionName::new(ACTION_PROVIDE)?].into(),
                    resource: ResourcePath::new(["endpoint", created.endpoint.as_str()])?,
                    ttl_ms: None,
                    delegable: false,
                    max_calls: None,
                },
                Grant {
                    from: self.authority.ready.1,
                    actions: [ActionName::new(ACTION_READY)?].into(),
                    resource: ResourcePath::new([ready_key.as_str()])?,
                    ttl_ms: None,
                    delegable: false,
                    max_calls: None,
                },
            ])
        })();
        let identity =
            self.authority.spawn_as.map(|authority| IdentityRequest { authority, project: key.0.clone() });
        let spawned = match grants {
            Ok(grants) => k.spawn(self.authority.spawn, program, grants, identity)?,
            Err(e) => Err(e),
        };
        match spawned {
            Ok(process) => {
                self.ready_keys.insert(ready_key.clone(), key.clone());
                Ok(Ok(Host {
                    process,
                    invoke: created.invoke.handle,
                    provide: created.provide.handle,
                    ready_key,
                    ready: false,
                    queued: Vec::new(),
                    inflight: 0,
                    last_used: Instant::now(),
                }))
            }
            Err(e) => {
                let _ = k.close(created.invoke.handle)?;
                let _ = k.close(created.provide.handle)?;
                Ok(Err(self.naming_active(e)))
            }
        }
    }

    /// The refusal of one active project too many, with the projects whose
    /// hosts run here (agents and jobs run elsewhere too).
    fn naming_active(&self, mut e: Error) -> Error {
        if e.code != ErrorCode::LimitExceeded {
            return e;
        }
        let active: BTreeSet<&str> = self.hosts.keys().map(|(project, _)| project.as_str()).collect();
        if !active.is_empty() {
            let named: Vec<&str> = active.iter().take(12).copied().collect();
            let more = active.len().saturating_sub(named.len());
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            e.message = format!("{}. Projects with hosts running: {}{more}", e.message, named.join(", "));
        }
        e
    }

    /// A host says it serves: from its own process only. Its queued calls go.
    fn ready(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let key = d.resource.segments().first().and_then(|key| self.ready_keys.get(key)).cloned();
        let Some(key) =
            key.filter(|key| self.hosts.get(key).is_some_and(|h| h.process == d.caller && !h.ready))
        else {
            return k
                .reply(d.id, Err(Error::new(ErrorCode::PermissionDenied, "not a host waiting to be ready")));
        };
        k.reply(d.id, Payload::encode(&serde_json::json!({})))?;
        let Some(host) = self.hosts.get_mut(&key) else { return Ok(()) };
        // Its endpoint has a new generation now that the host provides it.
        let invoke = host.invoke;
        if let Err(e) = k.rebind(invoke)? {
            let queued = std::mem::take(&mut host.queued);
            for call in queued {
                self.fail(k, call, e.clone())?;
            }
            return Ok(());
        }
        host.ready = true;
        for call in std::mem::take(&mut host.queued) {
            self.submit(k, key.clone(), call)?;
        }
        Ok(())
    }

    /// The answer to a forwarded call, turned into the caller's.
    pub fn answer(
        &mut self,
        k: &mut impl Kernel,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        if let Some(stopping) = self.stops.remove(&request) {
            return self.stopped(k, stopping, result);
        }
        let Some(forward) = self.forwards.remove(&request) else {
            self.job_answer(k, request, result)?;
            return Ok(());
        };
        if let Some(requests) = self.by_dispatch.get_mut(&forward.dispatch) {
            requests.retain(|r| *r != request);
            if requests.is_empty() {
                self.by_dispatch.remove(&forward.dispatch);
            }
        }
        if let Some(host) = self.hosts.get_mut(&forward.host) {
            host.inflight = host.inflight.saturating_sub(1);
            host.last_used = Instant::now();
        }
        // A registered agent's command that went on as a job is answered there.
        if let Some(job) = &forward.job
            && self.exec_answered(k, job, &forward.host, &result)?
        {
            return Ok(());
        }
        let reply = match forward.then {
            then @ (Then::JobOutput { .. } | Then::JobStop { .. } | Then::JobDetach { .. }) => {
                match self.job_reply(k, then, result)? {
                    Some(reply) => reply,
                    None => return Ok(()),
                }
            }
            Then::Verbatim => result,
            Then::Created { project } => {
                return match result {
                    Ok(_) => {
                        let answer = Project::named(&project).and_then(|p| Payload::encode(&p));
                        self.creation_ended(k, &project, answer)
                    }
                    Err(e) => self.unmake(k, &project, e),
                };
            }
            // The host served a folder that is gone now: it goes too.
            Then::Removed { project } => {
                if let Some(host) = self.hosts.get(&forward.host).map(|h| h.process) {
                    let _ = k.kill(host)?;
                    self.child_exited(k, host)?;
                }
                return self.removed(k, &project, result.map(drop));
            }
        };
        k.reply(forward.dispatch, reply)
    }

    /// The caller gave up: its forwarded calls are withdrawn (their answers,
    /// when they come, end it), or it never left the queue.
    pub fn cancel(&mut self, k: &mut impl Kernel, dispatch: DispatchId) -> Result<(), ChannelError> {
        if let Some(requests) = self.by_dispatch.get(&dispatch) {
            for request in requests.clone() {
                k.cancel(request)?;
            }
            return Ok(());
        }
        for host in self.hosts.values_mut() {
            if let Some(at) = host.queued.iter().position(|c| c.dispatch == dispatch) {
                let call = host.queued.remove(at);
                return self.fail(
                    k,
                    call,
                    Error::new(ErrorCode::Cancelled, "cancelled before it was forwarded"),
                );
            }
        }
        Ok(())
    }

    /// A host ended: what waited for it is refused; what it was asked is
    /// answered by the kernel.
    pub fn child_exited(&mut self, k: &mut impl Kernel, process: ProcessId) -> Result<(), ChannelError> {
        let Some(key) = self.hosts.iter().find(|(_, h)| h.process == process).map(|(key, _)| key.clone())
        else {
            return Ok(());
        };
        let Some(host) = self.hosts.remove(&key) else { return Ok(()) };
        self.ready_keys.remove(&host.ready_key);
        self.jobs_host_ended(k, &key)?;
        for call in host.queued {
            self.fail(
                k,
                call,
                Error::new(ErrorCode::Unavailable, "the project's host ended before it served"),
            )?;
        }
        let _ = k.close(host.invoke)?;
        let _ = k.close(host.provide)?;
        Ok(())
    }

    /// Stops hosts that served nothing for the idle time and have no call in
    /// flight or waiting.
    pub fn sweep(&mut self, k: &mut impl Kernel) -> Result<(), ChannelError> {
        let idle: Vec<ProcessId> = self
            .hosts
            .iter()
            .filter(|(key, host)| {
                host.ready
                    && host.inflight == 0
                    && host.queued.is_empty()
                    && host.last_used.elapsed() >= self.idle
                    && !self.holds_jobs(key)
            })
            .map(|(_, host)| host.process)
            .collect();
        for process in idle {
            // Its exit event cleans up; a refusal means it is gone already.
            let _ = k.kill(process)?;
        }
        self.jobs_sweep(k)
    }
}

/// The project a resource names when it names exactly the project.
fn whole_project(resource: &ResourcePath) -> Result<String, Error> {
    match resource.segments() {
        [root, project] if root == projects::PROJECTS && projects::valid_name(project) => Ok(project.clone()),
        _ => Err(invalid(format!("{resource} names no project: expected [\"projects\", project]"))),
    }
}

/// Which projects a resource covers, for `list`: every
/// project (`["projects"]`) or one (`["projects", p]`).
fn covered(resource: &ResourcePath) -> Result<Option<String>, Error> {
    match resource.segments() {
        [root] if root == projects::PROJECTS => Ok(None),
        [root, project] if root == projects::PROJECTS && projects::valid_name(project) => {
            Ok(Some(project.clone()))
        }
        _ => Err(invalid(format!("{resource}: expected [\"projects\"] or [\"projects\", project]"))),
    }
}
