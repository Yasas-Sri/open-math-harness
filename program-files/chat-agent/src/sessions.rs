//! The sessions service: this program's process that starts every session's
//! agent. A session belongs to a project and its agent runs as that project,
//! which a process of the runtime's own identity cannot be; so the service
//! runs no session itself. Like the discovery service with its runs, it
//! splits the program's authority among the program's own processes: each
//! agent gets what the service holds, narrowed to its session, and the
//! service uses nothing of it but to start, stop and describe them.
//!
//! An agent is this program started from `--child-executable` with the
//! operator's `--child-arg`s, the model its `create` names as its own
//! (`--model`, after the operator's, whose place it takes), and its session's
//! execution context and tool bindings. A `create` that names no model is
//! refused unless the operator's arguments name one. With `--projects`, it is told its
//! session's working directory in its project's folder (`--workdir`, where
//! it reads attached files), by the folder's real path, which is also its
//! path in this service's sandbox. When the service holds `kernel spawn_as`,
//! the agent runs as its project: an ordinary process of the projects'
//! Linux user, in no sandbox, in the project's cgroup, and so are the
//! plugins it starts. Without, it is a sandboxed child that sees its
//! project's folder at that same path. Every session works in its
//! project's repository, `repo/` of the folder; capabilities on the endpoints
//! named by `--repo-scope` reach that repository only (`["projects", p,
//! "repo"]` from `["projects"]`), those named by `--project-scope` its
//! project, and those named by `--session-scope` its session (`[p, s]` from
//! `[]`).
//!
//! An agent runs at the reasoning effort its `create` named: the service adds
//! `--reasoning-effort` for it, or nothing for `default`, so the operator's
//! `--child-arg`s may not carry a reasoning setting.
//!
//! Every agent gets `conversation history` on its own session, which it
//! keeps: it continues the session's conversation from its log at every
//! start (conversation interface, "Continuing a conversation"), so an
//! interrupt, a stop or a runtime update never starts it over. The service
//! holds `history` to hand on and fails at startup without it.
//!
//! A compromised sessions service exposes every session's history and tools
//! and its own subtree, and can start an agent as any project (`spawn_as`).
//! In the VM it holds model grants on every model id, which each agent gets:
//! it can call any model, with no cap on its number of calls. It sees the projects directory read-only, to name a project, and
//! opens no project's folder: each is the projects' user's, mode 0700.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use nucleus_conversation_interface as conversation;
use nucleus_memory_interface as memory;
use nucleus_model_interface as model;
use nucleus_projects_interface::{Dir, valid_name};
use nucleus_protocol::{
    ACTION_CREATE_ENDPOINT, ACTION_PROVIDE, ACTION_SPAWN_AS, ACTION_SPAWN_EXECUTABLE, ActionName, ChildBind,
    ChildProgram, EndpointName, Error, ErrorCode, Grant, HandleId, HandleInfo, IdentityRequest,
    KERNEL_ENDPOINT, Payload, ProcessId, ResourcePath,
};
use nucleus_sdk::{ChannelError, Connection, Event, ToolBinding};
use nucleus_sessions_interface::{
    ACTION_CREATE, ACTION_FORGET, ACTION_INTERRUPT, ACTION_LIST, ACTION_STOP, CreateRequest, EffortSetting,
    GrantInfo, ListRequest, ListResponse, ReasoningEffort, SessionInfo, interface,
};
use serde_json::json;

use crate::plugins::ExecutionContext;

/// Sessions with a live agent at once.
pub const MAX_SESSIONS: usize = 32;
/// The agent's option for the reasoning effort its session was created with.
pub(crate) const EFFORT_FLAG: &str = "--reasoning-effort";

/// The reminder actions a session's agent gets on its own session, when this
/// service holds them delegably. Never `wake`: a session's agent does not
/// wake itself at once. They are a grant of their own, apart from `receive`
/// and `post`, and not delegable: the agent keeps them, and its plugins do
/// not get them.
const OWN_REMINDER_ACTIONS: [&str; 3] =
    [conversation::ACTION_REMIND, conversation::ACTION_REMINDERS, conversation::ACTION_CANCEL_REMINDER];

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// The agent program, in this process's namespace.
    pub executable: String,
    /// What every agent starts with, before its session's own arguments.
    pub args: Vec<String>,
    /// Paths of this namespace every sandboxed agent sees at the same path,
    /// and whether writable. An agent that runs as its project's user sees
    /// the VM's files and takes none.
    pub binds: Vec<(String, bool)>,
    /// The projects' folders: their real path, at which this service's
    /// sandbox has them too.
    pub projects: Option<String>,
    pub repo_scoped: BTreeSet<String>,
    pub project_scoped: BTreeSet<String>,
    pub session_scoped: BTreeSet<String>,
    /// What each agent is told about where it runs, before its session's
    /// workspace, and the tool bindings it gets, before its session's scope.
    pub context: Option<ExecutionContext>,
    pub bindings: Vec<ToolBinding>,
    pub endpoint: EndpointName,
}

impl Options {
    /// The sessions service's options, when `options` asks for one.
    pub fn from_agent(options: &crate::Options) -> Result<Option<Self>, String> {
        let Some(executable) = options.child_executable.clone() else { return Ok(None) };
        Ok(Some(Self {
            executable,
            args: options.child_args.clone(),
            binds: options.child_binds.clone(),
            projects: options.projects.clone(),
            repo_scoped: options.repo_scoped.iter().cloned().collect(),
            project_scoped: options.project_scoped.iter().cloned().collect(),
            session_scoped: options.session_scoped.iter().cloned().collect(),
            context: options.execution_context.clone(),
            bindings: options.tool_bindings.clone(),
            endpoint: EndpointName::new(nucleus_sessions_interface::ENDPOINT).map_err(|e| e.to_string())?,
        }))
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

/// The project a session's agent works in: its repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub project: String,
}

impl Scope {
    fn dir(&self) -> Result<Dir, Error> {
        Dir::repository(&self.project)
    }

    /// What a capability on `endpoint` held on `resource` becomes for an agent.
    fn narrow(
        &self,
        options: &Options,
        endpoint: &str,
        resource: &ResourcePath,
    ) -> Result<ResourcePath, Error> {
        let mut segments = resource.segments().to_vec();
        if options.repo_scoped.contains(endpoint) {
            segments.push(self.project.clone());
            segments.extend(self.dir()?.in_home().into_iter().map(str::to_owned));
        } else if options.project_scoped.contains(endpoint) {
            segments.push(self.project.clone());
        } else {
            return Ok(resource.clone());
        }
        ResourcePath::new(segments)
    }
}

/// The service's authority, from which each agent's is narrowed.
struct Template {
    spawn: HandleId,
    spawn_as: Option<HandleId>,
    conversation: HandleId,
    /// `conversation history`, delegable: each agent reads its own
    /// session's log with it, and no other.
    history: HandleId,
    /// The delegable conversation handles holding reminder actions: which of
    /// [`OWN_REMINDER_ACTIONS`] each holds, on which scope.
    reminders: Vec<(HandleId, GrantInfo)>,
    /// The memory service and the actions this service holds on it, when the
    /// operator granted one. Narrowed per session rather than forwarded.
    memory: Option<(HandleId, BTreeSet<ActionName>)>,
    shared: Vec<(HandleId, GrantInfo)>,
}

fn held(handles: &[HandleInfo], endpoint: &str, action: &str, delegable: bool) -> Result<HandleId, String> {
    handles
        .iter()
        .find(|h| {
            h.endpoint.as_str() == endpoint
                && (h.delegable || !delegable)
                && h.actions.iter().any(|a| a.as_str() == action)
        })
        .map(|h| h.handle)
        .ok_or_else(|| {
            let kind = if delegable { "a delegable" } else { "a" };
            format!("started without {kind} `{endpoint}` capability for `{action}`")
        })
}

/// The kernel's resource for spawning the executable at `path`.
fn executable_resource(path: &str) -> Result<ResourcePath, String> {
    ResourcePath::new(std::iter::once("executable").chain(path.split('/').filter(|s| !s.is_empty())))
        .map_err(|e| format!("--child-executable {path}: {e}"))
}

impl Template {
    fn new(options: &Options, handles: &[HandleInfo]) -> Result<Self, String> {
        let kernel = |action: &str, resource: &ResourcePath| {
            handles.iter().find(|h| {
                h.endpoint.as_str() == KERNEL_ENDPOINT
                    && h.actions.iter().any(|a| a.as_str() == action)
                    && resource.is_within(&h.resource)
            })
        };
        let spawn = kernel(ACTION_SPAWN_EXECUTABLE, &executable_resource(&options.executable)?)
            .map(|h| h.handle)
            .ok_or("started without authority to spawn the agent program")?;
        let identity = ResourcePath::new(["identity"]).map_err(|e| e.to_string())?;
        let spawn_as = handles
            .iter()
            .find(|h| {
                h.endpoint.as_str() == KERNEL_ENDPOINT
                    && h.actions.iter().any(|a| a.as_str() == ACTION_SPAWN_AS)
                    && h.resource.is_within(&identity)
            })
            .map(|h| h.handle);
        if spawn_as.is_some() && options.projects.is_none() {
            return Err("kernel spawn_as needs --projects: an agent runs as its project".into());
        }
        // An agent that runs as its project's user starts its programs as
        // that user, in no sandbox, and knows it only from its context:
        // without one it would ask for sandboxes the kernel refuses.
        if spawn_as.is_some() && options.context.is_none() {
            return Err("kernel spawn_as needs --execution-context: an agent that runs as its project's \
                        user is told so there"
                .into());
        }
        // The person's Stop withdraws a session's pending reminders, so a
        // service that lets agents set them must clear them too.
        let holding = |action: &'static str| {
            handles.iter().filter(move |h| {
                h.endpoint.as_str() == conversation::ENDPOINT
                    && h.actions.iter().any(|a| a.as_str() == action)
            })
        };
        if holding(conversation::ACTION_REMIND).any(|r| {
            !holding(conversation::ACTION_CLEAR_REMINDERS).any(|c| r.resource.is_within(&c.resource))
        }) {
            return Err("started with `conversation remind` but without `clear_reminders` on the same \
                        sessions: a stopped session's reminders could not be withdrawn"
                .into());
        }
        // Every delegable capability this service holds is forwarded to each
        // agent, narrowed to its session where the operator said so (the
        // scopes), except `conversation` and `memory` (narrowed to the one
        // session, below). Only explicitly delegable executable and endpoint
        // creation pass through from the kernel. A new service needs no code
        // here: grant this service a delegable capability for it, and each
        // agent gets it. No call budget is added: a session's agent has no
        // cap on its model calls.
        let memory = handles
            .iter()
            .find(|h| h.delegable && h.endpoint.as_str() == memory::ENDPOINT)
            .map(|h| (h.handle, h.actions.clone()));
        let reminders = handles
            .iter()
            .filter(|h| h.delegable && h.endpoint.as_str() == conversation::ENDPOINT)
            .map(|h| {
                let actions = h.actions.iter().filter(|a| OWN_REMINDER_ACTIONS.contains(&a.as_str()));
                let (endpoint, resource) = (h.endpoint.clone(), h.resource.clone());
                (h.handle, GrantInfo { endpoint, actions: actions.cloned().collect(), resource })
            })
            .filter(|(_, info)| !info.actions.is_empty())
            .collect();
        let mut shared = Vec::new();
        for handle in handles {
            let ep = handle.endpoint.as_str();
            if !handle.delegable || ep == conversation::ENDPOINT || ep == memory::ENDPOINT {
                continue;
            }
            let actions: BTreeSet<_> = handle
                .actions
                .iter()
                .filter(|action| {
                    ep != KERNEL_ENDPOINT
                        || matches!(action.as_str(), ACTION_SPAWN_EXECUTABLE | ACTION_CREATE_ENDPOINT)
                })
                .cloned()
                .collect();
            if actions.is_empty() {
                continue;
            }
            let info =
                GrantInfo { endpoint: handle.endpoint.clone(), actions, resource: handle.resource.clone() };
            shared.push((handle.handle, info));
        }
        // A scope names what this service holds to hand on, checked at
        // startup so a misconfiguration fails here, not at `create`.
        for endpoint in
            options.repo_scoped.iter().chain(&options.project_scoped).chain(&options.session_scoped)
        {
            if !shared.iter().any(|(_, info)| info.endpoint.as_str() == endpoint) {
                return Err(format!(
                    "a scope names `{endpoint}`, which this service holds nothing delegable on"
                ));
            }
        }
        // A session's own endpoint has no environment to bind: its tools
        // name nothing but the session, which the resource already is.
        if options.bindings.iter().any(|b| options.session_scoped.contains(b.endpoint.as_str())) {
            return Err("a tool binding names a --session-scope endpoint".into());
        }
        Ok(Self {
            spawn,
            spawn_as,
            conversation: held(handles, conversation::ENDPOINT, conversation::ACTION_RECEIVE, true)?,
            history: held(handles, conversation::ENDPOINT, conversation::ACTION_HISTORY, true)?,
            reminders,
            memory,
            shared,
        })
    }

    /// The model a `create` names, a provider/model id; with none, the
    /// operator's (`None`: the agent's arguments name it), refused when the
    /// operator names none either.
    fn model<'a>(options: &Options, requested: Option<&'a str>) -> Result<Option<&'a str>, Error> {
        match requested {
            Some(id) if model::resource_for(id).is_ok_and(|r| r.segments().len() >= 2) => Ok(Some(id)),
            Some(id) => Err(invalid(format!("`{id}` is not a provider/model id"))),
            None if options.args.iter().any(|arg| arg == "--model") => Ok(None),
            None => Err(invalid("a `create` names the model its agent runs")),
        }
    }

    /// The delegations for one session. The kernel rejects the spawn if any of
    /// them is wider than what this service holds.
    fn grants(
        &self,
        options: &Options,
        session: &ResourcePath,
        scope: &Scope,
    ) -> Result<Vec<(HandleId, GrantInfo)>, Error> {
        let on_session = |actions: &[&str]| -> Result<GrantInfo, Error> {
            Ok(GrantInfo {
                endpoint: EndpointName::new(conversation::ENDPOINT)?,
                actions: actions.iter().map(|a| ActionName::new(*a)).collect::<Result<_, _>>()?,
                resource: session.clone(),
            })
        };
        let mut all = vec![
            (self.conversation, on_session(&[conversation::ACTION_RECEIVE, conversation::ACTION_POST])?),
            // Its own session's log, which it continues from at every start.
            (self.history, on_session(&[conversation::ACTION_HISTORY])?),
        ];
        all.extend(
            self.reminders
                .iter()
                .filter(|(_, held)| session.is_within(&held.resource))
                .map(|(handle, held)| (*handle, GrantInfo { resource: session.clone(), ..held.clone() })),
        );
        if let Some((handle, actions)) = &self.memory {
            all.push((
                *handle,
                GrantInfo {
                    endpoint: EndpointName::new(memory::ENDPOINT)?,
                    actions: actions.clone(),
                    resource: session.clone(),
                },
            ));
        }
        for (handle, info) in &self.shared {
            let resource = if options.session_scoped.contains(info.endpoint.as_str()) {
                ResourcePath::new(info.resource.segments().iter().chain(session.segments()).cloned())?
            } else {
                scope.narrow(options, info.endpoint.as_str(), &info.resource)?
            };
            all.push((*handle, GrantInfo { resource, ..info.clone() }));
        }
        Ok(all)
    }

    /// The program a session's agent is started as, and the identity.
    fn program(
        &self,
        options: &Options,
        scope: &Scope,
        model: Option<&str>,
        effort: Option<ReasoningEffort>,
    ) -> Result<(ChildProgram, Option<IdentityRequest>), Error> {
        let mut args = options.args.clone();
        if let Some(effort) = effort {
            args.extend([EFFORT_FLAG.to_owned(), effort.as_str().to_owned()]);
        }
        // The agent runs the model its `create` named unless a message names
        // another; the last `--model` it parses is its own.
        if let Some(model) = model {
            args.extend(["--model".to_owned(), model.to_owned()]);
        }
        // With projects, the agent works in its project's folder; with
        // projects' processes, as its project, in no sandbox.
        let home = options.projects.as_ref().map(|projects| format!("{projects}/{}", scope.project));
        let identity = home.as_ref().and_then(|_| {
            self.spawn_as.map(|authority| IdentityRequest { authority, project: scope.project.clone() })
        });
        let mut network = false;
        if let Some(base) = &options.context {
            let mut context = base.clone();
            if let Some(home) = &home {
                let dir = scope.dir()?;
                context.workspace = dir.id();
                context.workspace_dir = format!("{home}/{}", dir.path());
                context.sandboxed = Some(identity.is_none());
            }
            // A project's process has its user's network, and is told so.
            if identity.is_some() {
                context.network = Some(true);
            }
            network = context.network == Some(true);
            args.extend(["--execution-context".to_owned(), encode(&context)?]);
        }
        if !options.bindings.is_empty() {
            let mut bindings = options.bindings.clone();
            for binding in &mut bindings {
                let narrowed = scope.narrow(options, binding.endpoint.as_str(), &binding.resource)?;
                if narrowed != binding.resource && options.repo_scoped.contains(binding.endpoint.as_str()) {
                    binding.workspace = Some(scope.dir()?.id());
                }
                binding.resource = narrowed;
            }
            args.extend(["--tool-bindings".to_owned(), encode(&bindings)?]);
        }
        let mut binds = Vec::new();
        if let Some(home) = &home {
            args.extend(["--workdir".to_owned(), format!("{home}/{}", scope.dir()?.path())]);
            if identity.is_none() {
                binds.push(ChildBind { path: home.clone(), mount: None, writable: true });
            }
        }
        if identity.is_none() {
            binds.extend(options.binds.iter().map(|(path, writable)| ChildBind {
                path: path.clone(),
                mount: None,
                writable: *writable,
            }));
        }
        // As its project's user the agent is an ordinary process with the
        // VM's network; a sandboxed one has it when its context says so.
        let network = network || identity.is_some();
        Ok((ChildProgram { executable: options.executable.clone(), args, binds, network }, identity))
    }
}

/// A JSON argument of the agent's command line.
fn encode(value: &impl serde::Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))
}

/// The delegations a session's agent is spawned with.
fn spawn_grants(planned: &[(HandleId, GrantInfo)]) -> Vec<Grant> {
    planned
        .iter()
        .map(|(from, info)| Grant {
            from: *from,
            actions: info.actions.clone(),
            resource: info.resource.clone(),
            ttl_ms: None,
            // Working sessions may distribute the authority this service
            // explicitly marked delegable, except their conversation, whose
            // messages, log and reminders they keep, and their jobs, which
            // say who the session's agent is. Budgets, expiry and resource
            // scope remain linked to this service's capability.
            delegable: info.endpoint.as_str() != conversation::ENDPOINT
                && info.endpoint.as_str() != nucleus_shell_interface::jobs::ENDPOINT,
            max_calls: None,
        })
        .collect()
}

/// What `sessions create` spawns for `session` working in `scope`, with the
/// model and the reasoning effort it names: the spawn authority, the program,
/// the delegations and the identity. Public so a composition can be checked
/// against it.
pub fn session_spawn(
    options: &Options,
    handles: &[HandleInfo],
    session: &ResourcePath,
    scope: &Scope,
    (model, effort): (Option<&str>, Option<ReasoningEffort>),
) -> Result<(HandleId, ChildProgram, Vec<Grant>, Option<IdentityRequest>), String> {
    let template = Template::new(options, handles)?;
    let model = Template::model(options, model).map_err(|e| e.message)?;
    let planned = template.grants(options, session, scope).map_err(|e| e.message)?;
    let (program, identity) = template.program(options, scope, model, effort).map_err(|e| e.message)?;
    Ok((template.spawn, program, spawn_grants(&planned), identity))
}

type Kernel<R = std::io::Stdin, W = std::io::Stdout> = Connection<R, W>;

/// The sessions whose agents this service started, by project and name.
#[derive(Default)]
struct State {
    sessions: BTreeMap<(String, String), SessionInfo>,
}

/// Room for one more running agent in the VM, refused saying which projects
/// run them.
fn room_for_agent(state: &State) -> Result<(), Error> {
    let mut by_project: BTreeMap<&str, usize> = BTreeMap::new();
    for session in state.sessions.values().filter(|s| s.live) {
        *by_project.entry(session.project.as_str()).or_default() += 1;
    }
    let live: usize = by_project.values().sum();
    if live < MAX_SESSIONS {
        return Ok(());
    }
    let whose: Vec<String> =
        by_project.iter().map(|(project, n)| format!("{n} in project {project}")).collect();
    Err(Error::new(
        ErrorCode::LimitExceeded,
        format!(
            "at most {MAX_SESSIONS} explorations run in this VM at once, and {live} do ({}): stop one that is \
             no longer needed, then start this one",
            whose.join(", ")
        ),
    ))
}

/// The project and name of the session `resource` names: `[project, name]`.
fn session_key(resource: &ResourcePath) -> Result<(String, String), Error> {
    match resource.segments() {
        [project, name] if valid_name(project) => Ok((project.clone(), name.clone())),
        _ => Err(invalid("the resource path must be a project and one session name: [project, session]")),
    }
}

fn serve_one<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    template: &Template,
    options: &Options,
    state: &mut State,
    action: &str,
    resource: &ResourcePath,
    payload: &Payload,
) -> Result<Result<Payload, Error>, ChannelError> {
    match action {
        ACTION_LIST => {
            // Every project's sessions, or one project's.
            let only = match resource.segments() {
                [] => None,
                [project] if valid_name(project) => Some(project),
                _ => return Ok(Err(invalid("`list` is called on the root path or on [project]"))),
            };
            let request = match payload.decode::<ListRequest>() {
                Ok(request) => request,
                Err(e) => return Ok(Err(e)),
            };
            let limit = match request.validate() {
                Ok(limit) => limit,
                Err(e) => return Ok(Err(e)),
            };
            // A page by project and name, after the cursor `<project>/<name>`.
            let after = match request.after.as_deref().map(|after| after.split_once('/')) {
                None => None,
                Some(Some((project, name))) => Some((project.to_owned(), name.to_owned())),
                Some(None) => return Ok(Err(invalid("after is not the next of a list of sessions"))),
            };
            let mut shown = state
                .sessions
                .range((
                    after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                    std::ops::Bound::Unbounded,
                ))
                .filter(|(_, session)| only.is_none_or(|project| session.project == *project));
            let sessions: Vec<SessionInfo> = shown.by_ref().take(limit).map(|(_, s)| s.clone()).collect();
            let next = shown.next().and(sessions.last()).map(|s| format!("{}/{}", s.project, s.name));
            Ok(Payload::encode(&ListResponse { sessions, next }))
        }
        ACTION_CREATE => {
            let (key, request) =
                match session_key(resource).and_then(|key| Ok((key, payload.decode::<CreateRequest>()?))) {
                    Ok(parsed) => parsed,
                    Err(e) => return Ok(Err(e)),
                };
            if state.sessions.get(&key).is_some_and(|s| s.live) {
                return Ok(Err(invalid("that session already has an agent")));
            }
            if let Err(e) = room_for_agent(state) {
                return Ok(Err(e));
            }
            let effort = request.reasoning_effort.and_then(EffortSetting::effort);
            start(kernel, template, options, state, key, resource, (request.model.as_deref(), effort))
        }
        ACTION_STOP => {
            // A project-scoped stop preserves each conversation and closes its turn.
            if let [project] = resource.segments() {
                if !valid_name(project) {
                    return Ok(Err(invalid("invalid project name")));
                }
                if let Err(e) = payload.decode::<nucleus_projects_interface::Empty>() {
                    return Ok(Err(e));
                }
                let keys: Vec<_> = state.sessions.keys().filter(|(p, _)| p == project).cloned().collect();
                for key in keys {
                    let session = ResourcePath::new([key.0.clone(), key.1.clone()]);
                    let session = match session {
                        Ok(session) => session,
                        Err(e) => return Ok(Err(e)),
                    };
                    let Some(info) = state.sessions.get_mut(&key) else { continue };
                    if info.live {
                        if let Err(e) = ended(kernel, info.process)? {
                            return Ok(Err(e));
                        }
                        info.live = false;
                    }
                    if let Err(e) = close_turn(kernel, template, &session)? {
                        return Ok(Err(e));
                    }
                    if let Err(e) = clear_reminders(kernel, &session)? {
                        return Ok(Err(e));
                    }
                }
                return Ok(Payload::encode(&json!({})));
            }
            let key = match session_key(resource) {
                Ok(key) => key,
                Err(e) => return Ok(Err(e)),
            };
            let Some(session) = state.sessions.get_mut(&key) else {
                return Ok(Err(invalid("no session by that name")));
            };
            if session.live {
                if let Err(error) = ended(kernel, session.process)? {
                    return Ok(Err(error));
                }
                session.live = false;
            }
            if let Err(error) = close_turn(kernel, template, resource)? {
                return Ok(Err(error));
            }
            // A stop, unlike an interrupt, also withdraws the session's
            // pending reminders, once its agent is gone; with no agent, what
            // one left pending is withdrawn all the same.
            Ok(clear_reminders(kernel, resource)?.and_then(|()| Payload::encode(&json!({}))))
        }
        ACTION_INTERRUPT => {
            let key = match session_key(resource) {
                Ok(key) => key,
                Err(e) => return Ok(Err(e)),
            };
            // No agent runs this session (never started by this process, e.g.
            // after a restart, or stopped): nothing to cancel, but a turn it
            // left open is closed, so the window stops showing it as running.
            let Some(session) = state.sessions.get_mut(&key).filter(|s| s.live) else {
                return Ok(close_turn(kernel, template, resource)?.and_then(|()| Payload::encode(&json!({}))));
            };
            // A turn cannot be cancelled from here: end the agent and start it
            // again on the same model and effort, with the turn closed so it
            // is not resumed.
            let (process, model, effort) = (session.process, session.model.clone(), session.reasoning_effort);
            if let Err(error) = ended(kernel, process)? {
                return Ok(Err(error));
            }
            session.live = false;
            // Never restart before the stop decision is durable. If posting
            // fails, report it with the killed agent correctly marked gone.
            if let Err(error) = close_turn(kernel, template, resource)? {
                return Ok(Err(error));
            }
            Ok(start(kernel, template, options, state, key, resource, (model.as_deref(), effort))?
                .and_then(|_| Payload::encode(&json!({}))))
        }
        ACTION_FORGET => {
            let project = match resource.segments() {
                [project] if valid_name(project) => project.clone(),
                _ => return Ok(Err(invalid("`forget` is called on [project]"))),
            };
            if let Err(e) = payload.decode::<nucleus_projects_interface::Empty>() {
                return Ok(Err(e));
            }
            Ok(forget(kernel, state, &project)?.and_then(|()| Payload::encode(&json!({}))))
        }
        _ => Ok(Err(invalid("unknown action"))),
    }
}

/// `forget`: ends every agent of `project`'s sessions and drops them. Its
/// conversation is forgotten after this, so no turn is closed in it.
fn forget<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    state: &mut State,
    project: &str,
) -> Result<Result<(), Error>, ChannelError> {
    let ours: Vec<(String, String)> = state.sessions.keys().filter(|(p, _)| p == project).cloned().collect();
    for key in ours {
        if let Some(session) = state.sessions.get(&key).filter(|s| s.live)
            && let Err(error) = ended(kernel, session.process)?
        {
            return Ok(Err(error));
        }
        state.sessions.remove(&key);
    }
    Ok(Ok(()))
}

/// Starts the agent of the session `resource` names with the chosen model
/// and reasoning effort, in its project's repository, and records it.
fn start<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    template: &Template,
    options: &Options,
    state: &mut State,
    key: (String, String),
    resource: &ResourcePath,
    (model, effort): (Option<&str>, Option<ReasoningEffort>),
) -> Result<Result<Payload, Error>, ChannelError> {
    let scope = Scope { project: key.0.clone() };
    if let Some(projects) = &options.projects
        && !std::fs::symlink_metadata(format!("{projects}/{}", scope.project)).is_ok_and(|meta| meta.is_dir())
    {
        return Ok(Err(invalid(format!("there is no project {}", scope.project))));
    }
    let planned = (|| {
        let model = Template::model(options, model)?;
        let planned = template.grants(options, resource, &scope)?;
        let (program, identity) = template.program(options, &scope, model, effort)?;
        Ok::<_, Error>((model.map(str::to_owned), planned, program, identity))
    })();
    let (model, planned, program, identity) = match planned {
        Ok(planned) => planned,
        Err(e) => return Ok(Err(e)),
    };
    let grants = spawn_grants(&planned);
    Ok(kernel.spawn_executable_as(template.spawn, program, grants, identity)?.and_then(|process| {
        let info = SessionInfo {
            project: key.0.clone(),
            name: key.1.clone(),
            live: true,
            process,
            grants: planned.into_iter().map(|p| p.1).collect(),
            model,
            reasoning_effort: effort,
        };
        state.sessions.insert(key, info.clone());
        Payload::encode(&info)
    }))
}

/// Ends an agent: killed, or confirmed gone already.
fn ended<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    process: ProcessId,
) -> Result<Result<(), Error>, ChannelError> {
    Ok(match kernel.kill(process)? {
        Err(error) if error.code == ErrorCode::PermissionDenied => {
            // A child may exit between our last lifecycle event and this
            // request. Confirm its absence from a fresh owned child listing;
            // never turn a live-child denial into success.
            match kernel.children(1)? {
                Ok(children) if !children.iter().any(|child| child.process == process) => Ok(()),
                _ => Err(error),
            }
        }
        result => result,
    })
}

/// Withdraws every pending reminder of the session at `resource`: the person
/// stopped it. The conversation records each cancellation with
/// `clear_reminders`, which only this service holds, so the page shows it as
/// the person's. Nothing to do where this service holds no
/// `clear_reminders`, and so gave no agent `remind` (`Template::new`).
fn clear_reminders<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    resource: &ResourcePath,
) -> Result<Result<(), Error>, ChannelError> {
    let action = conversation::ACTION_CLEAR_REMINDERS;
    let Some(handle) = kernel.find_handle_for(conversation::ENDPOINT, action, resource) else {
        return Ok(Ok(()));
    };
    Ok(kernel.call(handle, action, resource, &json!({}), None)?.map(|_| ()).map_err(|error| {
        Error::new(error.code, format!("the session's reminders were not cancelled: {}", error.message))
    }))
}

/// Records `interrupted` in the session's conversation, which ends a turn left
/// open, and pauses reminders even if the turn finished just before Stop.
fn close_turn<R: Read, W: Write>(
    kernel: &mut Kernel<R, W>,
    template: &Template,
    resource: &ResourcePath,
) -> Result<Result<(), Error>, ChannelError> {
    let interrupted = json!({"kind": "interrupted"});
    Ok(kernel
        .call(template.conversation, conversation::ACTION_POST, resource, &interrupted, None)?
        .map(|_| ()))
}

/// Provides the sessions endpoint and serves it until told to stop.
pub fn serve<R: Read, W: Write>(kernel: &mut Kernel<R, W>, options: Options) -> Result<(), String> {
    let template = Template::new(&options, kernel.handles())?;
    let endpoint = ResourcePath::new(["endpoint", options.endpoint.as_str()]).map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle_for(KERNEL_ENDPOINT, ACTION_PROVIDE, &endpoint)
        .ok_or("started without authority to provide the sessions endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let mut state = State::default();
    loop {
        let event = match kernel.next_event() {
            Ok(event) => event,
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        match event {
            Event::Dispatch(d) => {
                let result = serve_one(
                    kernel,
                    &template,
                    &options,
                    &mut state,
                    d.action.as_str(),
                    &d.resource,
                    &d.payload,
                )
                .map_err(|e| e.to_string())?;
                kernel.reply(d.id, result).map_err(|e| e.to_string())?;
            }
            Event::ChildExited(child) => {
                for session in state.sessions.values_mut().filter(|s| s.process == child) {
                    session.live = false;
                }
            }
            Event::Terminate => return Ok(()),
            Event::CancelDispatch(_) | Event::ChildReaped { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use nucleus_protocol::{
        ChildInfo, FromKernel, RequestId, ResponseBody, ToKernel, read_frame, write_frame,
    };

    fn handle(id: u32, endpoint: &str, actions: &[&str], resource: &[&str], delegable: bool) -> HandleInfo {
        HandleInfo {
            handle: HandleId(id),
            endpoint: EndpointName::new(endpoint).unwrap(),
            actions: actions.iter().map(|a| ActionName::new(*a).unwrap()).collect(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            delegable,
            expires_in_ms: None,
            max_calls: None,
        }
    }

    fn options(projects: bool) -> Options {
        Options {
            executable: "/programs/nucleus-chat-agent".into(),
            args: vec!["--personality".into(), "pragmatic".into()],
            binds: vec![("/programs".into(), false)],
            projects: projects.then(|| "/projects".into()),
            repo_scoped: if projects { ["fs".to_owned()].into() } else { BTreeSet::new() },
            project_scoped: BTreeSet::new(),
            session_scoped: BTreeSet::new(),
            context: None,
            bindings: Vec::new(),
            endpoint: EndpointName::new("sessions").unwrap(),
        }
    }

    fn handles() -> Vec<HandleInfo> {
        vec![
            handle(1, KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], &["executable"], true),
            handle(2, "conversation", &["receive", "post", "history"], &[], true),
            handle(3, "conversation", &["list", "clear_reminders"], &[], false),
            handle(4, "fs", &["read", "create"], &["projects"], true),
            handle(5, "web", &["fetch"], &[], true),
        ]
    }

    fn session() -> ResourcePath {
        ResourcePath::new(["alpha", "s1"]).unwrap()
    }

    /// The model a test's `create` names.
    const MODEL: Option<&str> = Some("a/b");

    /// The arguments a session's agent parses.
    fn agent_args(program: &ChildProgram) -> Vec<String> {
        program.args.clone()
    }

    /// A session's agent runs the model its `create` names. One that names none
    /// gets the operator's, and is refused when the operator names none: the
    /// VM's composition does not.
    #[test]
    fn an_agent_runs_the_model_its_create_names() {
        let scope = Scope { project: "alpha".into() };
        let spawn =
            |options: &Options, model| session_spawn(options, &handles(), &session(), &scope, (model, None));
        let (_, program, _, _) = spawn(&options(false), Some("z-ai/glm-5.3-flash")).unwrap();
        assert_eq!(crate::Options::parse(agent_args(&program)).unwrap().model, "z-ai/glm-5.3-flash");
        assert!(spawn(&options(false), None).unwrap_err().contains("names the model"));
        assert!(spawn(&options(false), Some("glm")).unwrap_err().contains("provider/model"));
        let mut operator = options(false);
        operator.args.extend(["--model".to_owned(), "a/b".to_owned()]);
        let (_, program, _, _) = spawn(&operator, None).unwrap();
        assert_eq!(crate::Options::parse(agent_args(&program)).unwrap().model, "a/b");
        let (_, program, _, _) = spawn(&operator, Some("c/d")).unwrap();
        assert_eq!(
            crate::Options::parse(agent_args(&program)).unwrap().model,
            "c/d",
            "the create's over the operator's"
        );
    }

    fn template() -> Template {
        Template::new(&options(false), &handles()).unwrap()
    }

    fn empty() -> Payload {
        Payload::encode(&json!({})).unwrap()
    }

    fn channel(
        replies: Vec<Result<ResponseBody, Error>>,
    ) -> (Kernel<std::io::Cursor<Vec<u8>>, std::io::PipeWriter>, std::io::PipeReader) {
        let mut input = Vec::new();
        write_frame(&mut input, &FromKernel::Hello { process: ProcessId(1), handles: Vec::new() }).unwrap();
        for (index, result) in replies.into_iter().enumerate() {
            write_frame(&mut input, &FromKernel::Response { request: RequestId(index as u64 + 1), result })
                .unwrap();
        }
        let (output, writer) = std::io::pipe().unwrap();
        (Connection::open(std::io::Cursor::new(input), writer).unwrap(), output)
    }

    fn running(live: bool) -> State {
        let info = SessionInfo {
            project: "alpha".into(),
            name: "s1".into(),
            live,
            process: ProcessId(2),
            grants: Vec::new(),
            model: None,
            reasoning_effort: None,
        };
        State { sessions: [(("alpha".to_owned(), "s1".to_owned()), info)].into() }
    }

    /// A session's agent gets its own session's conversation, the scoped
    /// endpoints narrowed to its project's repository, the others as held,
    /// and runs as its project; without projects' processes it is sandboxed
    /// with its project's folder in its namespace.
    #[test]
    fn an_agent_gets_its_session_its_repository_and_runs_as_its_project() {
        let projects = tempfile::tempdir().unwrap();
        std::fs::create_dir(projects.path().join("alpha")).unwrap();
        let mut options = options(true);
        options.session_scoped.insert("swarm".to_owned());
        let root = projects.path().to_str().unwrap().to_owned();
        options.projects = Some(root.clone());
        // What it is told of where it runs is this, with its own working
        // directory and whether it is in a sandbox.
        options.context = Some(ExecutionContext {
            env: "compute".into(),
            workspace: "default".into(),
            workspace_dir: "/elsewhere".into(),
            programs_dir: "/programs".into(),
            chat_executable: "/programs/nucleus-chat-agent".into(),
            mcp_executable: "/programs/nucleus-mcp".into(),
            network: Some(true),
            sandboxed: None,
            python_executable: None,
        });
        let mut held = handles();
        held.push(handle(6, KERNEL_ENDPOINT, &[ACTION_SPAWN_AS], &["identity"], false));
        held.push(handle(7, "swarm", &["start", "status", "verdict", "stop"], &[], true));
        let scope = Scope { project: "alpha".into() };
        let (spawn, program, grants, identity) =
            session_spawn(&options, &held, &session(), &scope, (MODEL, Some(ReasoningEffort::High))).unwrap();
        assert_eq!(spawn, HandleId(1));
        let line = |g: &Grant| format!("{} {:?}", g.from.0, g.resource.segments());
        let lines: Vec<String> = grants.iter().map(line).collect();
        assert!(lines.contains(&r#"2 ["alpha", "s1"]"#.to_owned()), "{lines:?}");
        assert!(lines.contains(&r#"4 ["projects", "alpha", "repo"]"#.to_owned()), "{lines:?}");
        assert!(lines.contains(&"5 []".to_owned()), "{lines:?}");
        assert!(
            lines.contains(&r#"7 ["alpha", "s1"]"#.to_owned()),
            "its swarms are its session's: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.starts_with("3 ")), "clear_reminders stays here");
        let home = format!("{root}/alpha");
        assert_eq!(identity.map(|i| i.project).as_deref(), Some("alpha"));
        // As the projects' user it is in no sandbox: no binds, and its
        // working directory by its real path, which it is told.
        assert!(program.binds.is_empty(), "{:?}", program.binds);
        let workdir = format!("{home}/repo");
        assert!(program.args.windows(2).any(|a| a == ["--workdir", workdir.as_str()]), "{:?}", program.args);

        // It is told that path, and that it and the programs it starts are
        // in no sandbox.
        let told = |held: &[HandleInfo]| {
            let (_, program, _, _) =
                session_spawn(&options, held, &session(), &scope, (MODEL, None)).unwrap();
            let context = crate::Options::parse(agent_args(&program)).unwrap().execution_context.unwrap();
            (context.workspace_dir, context.sandboxed)
        };
        let without_identities: Vec<HandleInfo> = held
            .iter()
            .filter(|h| !h.actions.iter().any(|a| a.as_str() == ACTION_SPAWN_AS))
            .cloned()
            .collect();
        assert_eq!(told(&held), (workdir.clone(), Some(false)));
        assert_eq!(told(&without_identities), (workdir, Some(true)));
        // Without projects' processes the agent is a sandboxed child that
        // sees its project's folder where this service does.
        let (_, sandboxed, _, identity) =
            session_spawn(&options, &without_identities, &session(), &scope, (MODEL, None)).unwrap();
        assert!(identity.is_none());
        let binds: Vec<_> =
            sandboxed.binds.iter().map(|b| (b.path.as_str(), b.mount.as_deref(), b.writable)).collect();
        assert_eq!(binds, [(home.as_str(), None, true), ("/programs", None, false)]);
        // The effort its `create` named is the agent's own argument; `default` adds none.
        assert_eq!(
            crate::Options::parse(agent_args(&program)).unwrap().reasoning_effort,
            Some(ReasoningEffort::High)
        );
        let (_, plain, _, _) = session_spawn(&options, &held, &session(), &scope, (MODEL, None)).unwrap();
        assert_eq!(crate::Options::parse(agent_args(&plain)).unwrap().reasoning_effort, None);
    }

    #[test]
    fn stopping_a_known_inactive_session_is_idempotent_but_unknown_is_rejected() {
        let (mut kernel, _output) = channel(vec![Ok(ResponseBody::Payload(empty()))]);
        let mut state = running(false);
        let stop = |kernel: &mut Kernel<_, _>, state: &mut State, resource: &[&str]| {
            let resource = ResourcePath::new(resource.iter().copied()).unwrap();
            serve_one(kernel, &template(), &options(false), state, ACTION_STOP, &resource, &empty()).unwrap()
        };
        assert!(stop(&mut kernel, &mut state, &["alpha", "s1"]).is_ok());
        assert_eq!(
            stop(&mut kernel, &mut state, &["alpha", "unknown"]).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(stop(&mut kernel, &mut state, &["Alpha"]).unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn a_failed_stop_record_is_reported_without_restarting_the_killed_agent() {
        for action in [ACTION_INTERRUPT, ACTION_STOP] {
            let error = Error::new(ErrorCode::ServiceError, "cannot persist the stop decision");
            let (mut kernel, mut output) = channel(vec![Ok(ResponseBody::Done), Err(error.clone())]);
            let mut state = running(true);
            let result = serve_one(
                &mut kernel,
                &template(),
                &options(false),
                &mut state,
                action,
                &session(),
                &empty(),
            )
            .unwrap();
            assert_eq!(result.unwrap_err(), error);
            assert!(!state.sessions[&("alpha".to_owned(), "s1".to_owned())].live);
            drop(kernel);
            assert!(matches!(read_frame::<_, ToKernel>(&mut output).unwrap(), ToKernel::Kill { .. }));
            let ToKernel::Call { action, .. } = read_frame::<_, ToKernel>(&mut output).unwrap() else {
                panic!("the stopped turn must be recorded");
            };
            assert_eq!(action.as_str(), conversation::ACTION_POST);
            assert!(read_frame::<_, ToKernel>(&mut output).is_err(), "no new agent or further work");
        }
    }

    #[test]
    fn a_project_stop_closes_inactive_turns_and_reports_a_failed_close() {
        for close in [Ok(ResponseBody::Payload(empty())), Err(invalid("cannot close turn"))] {
            let success = close.is_ok();
            let (mut kernel, mut output) = channel(vec![close, Ok(ResponseBody::Payload(empty()))]);
            let mut state = running(false);
            let mut other = state.sessions[&("alpha".into(), "s1".into())].clone();
            other.project = "beta".into();
            other.live = true;
            state.sessions.insert(("beta".into(), "s1".into()), other);
            let result = serve_one(
                &mut kernel,
                &template(),
                &options(false),
                &mut state,
                ACTION_STOP,
                &ResourcePath::new(["alpha"]).unwrap(),
                &empty(),
            )
            .unwrap();
            assert_eq!(result.is_ok(), success);
            assert!(state.sessions[&("beta".into(), "s1".into())].live);
            let ToKernel::Call { action, resource, .. } = read_frame::<_, ToKernel>(&mut output).unwrap()
            else {
                panic!()
            };
            assert_eq!(action.as_str(), conversation::ACTION_POST);
            assert_eq!(resource, session());
        }
    }

    /// The list is paged by project and name, every session once; past its
    /// running agents a start is refused saying which projects run them.
    #[test]
    fn sessions_are_listed_page_by_page_and_the_agent_limit_says_whose() {
        let (mut kernel, _output) = channel(Vec::new());
        let mut state = State::default();
        for n in 0..MAX_SESSIONS + 3 {
            let (project, name) = (if n % 2 == 0 { "alpha" } else { "beta" }, format!("s{n:02}"));
            let mut info = running(true).sessions.into_values().next().unwrap();
            (info.project, info.name, info.live) = (project.into(), name.clone(), n < MAX_SESSIONS);
            state.sessions.insert((project.to_owned(), name), info);
        }
        let mut list = |state: &mut State, request: serde_json::Value| {
            let payload = Payload::encode(&request).unwrap();
            let listed = serve_one(
                &mut kernel,
                &template(),
                &options(false),
                state,
                ACTION_LIST,
                &ResourcePath::root(),
                &payload,
            );
            listed.unwrap().map(|p| p.decode::<ListResponse>().unwrap())
        };
        let (mut seen, mut after) = (Vec::new(), None::<String>);
        loop {
            let page = list(&mut state, json!({"limit": 10, "after": after})).unwrap();
            seen.extend(page.sessions.iter().map(|s| (s.project.clone(), s.name.clone())));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, state.sessions.keys().cloned().collect::<Vec<_>>(), "by project and name, all once");
        assert_eq!(list(&mut state, json!({"limit": 0})).unwrap_err().code, ErrorCode::InvalidInput);
        let refused = room_for_agent(&state).unwrap_err();
        assert_eq!(refused.code, ErrorCode::LimitExceeded);
        assert!(["alpha", "beta"].iter().all(|p| refused.message.contains(p)), "{}", refused.message);
    }

    /// A session no agent runs here (started before a restart) may still have a
    /// turn open: interrupting it closes that turn instead of refusing.
    #[test]
    fn interrupting_a_session_without_an_agent_closes_its_open_turn() {
        let posted = Ok(ResponseBody::Payload(Payload::encode(&json!({})).unwrap()));
        let (mut kernel, mut output) = channel(vec![posted]);
        let resource = ResourcePath::new(["alpha", "before-restart"]).unwrap();
        let result = serve_one(
            &mut kernel,
            &template(),
            &options(false),
            &mut State::default(),
            ACTION_INTERRUPT,
            &resource,
            &empty(),
        )
        .unwrap();
        assert!(result.is_ok(), "{result:?}");
        let frame = read_frame::<_, ToKernel>(&mut output).unwrap();
        let ToKernel::Call { action, resource: posted_on, payload, .. } = frame else { panic!("{frame:?}") };
        assert_eq!(action.as_str(), conversation::ACTION_POST);
        assert_eq!(posted_on, resource);
        assert_eq!(payload.decode::<serde_json::Value>().unwrap(), json!({"kind": "interrupted"}));
    }

    /// `forget` ends the project's running agents, closes no turn (the
    /// conversation is forgotten next), drops the project's sessions and
    /// leaves another project's alone.
    #[test]
    fn forgetting_a_project_ends_its_agents_and_drops_its_sessions() {
        let mut state = running(true);
        let stopped = SessionInfo {
            name: "s2".into(),
            live: false,
            process: ProcessId(3),
            ..state.sessions[&("alpha".to_owned(), "s1".to_owned())].clone()
        };
        let other = SessionInfo { project: "beta".into(), process: ProcessId(4), ..stopped.clone() };
        state.sessions.insert(("alpha".into(), "s2".into()), stopped);
        state.sessions.insert(("beta".into(), "s2".into()), other);
        let (mut kernel, mut output) = channel(vec![Ok(ResponseBody::Done)]);
        let alpha = ResourcePath::new(["alpha"]).unwrap();
        let result =
            serve_one(&mut kernel, &template(), &options(false), &mut state, ACTION_FORGET, &alpha, &empty())
                .unwrap();
        assert_eq!(result.unwrap().decode::<serde_json::Value>().unwrap(), json!({}));
        assert_eq!(
            state.sessions.keys().cloned().collect::<Vec<_>>(),
            [("beta".to_owned(), "s2".to_owned())]
        );
        drop(kernel);
        assert_eq!(
            read_frame::<_, ToKernel>(&mut output).unwrap(),
            ToKernel::Kill { request: RequestId(1), child: ProcessId(2) }
        );
        assert!(read_frame::<_, ToKernel>(&mut output).is_err(), "nothing else: no turn closed");
        // A refused kill of a live agent leaves the project's sessions as they were.
        let mut state = running(true);
        let denial = Error::new(ErrorCode::PermissionDenied, "not a child of this process");
        let still = ChildInfo {
            process: ProcessId(2),
            program: "chat".into(),
            provides: Vec::new(),
            handles: 0,
            children: 0,
        };
        let (mut kernel, _output) =
            channel(vec![Err(denial.clone()), Ok(ResponseBody::Children(vec![still]))]);
        let result =
            serve_one(&mut kernel, &template(), &options(false), &mut state, ACTION_FORGET, &alpha, &empty())
                .unwrap();
        assert_eq!(result.unwrap_err(), denial);
        assert!(state.sessions[&("alpha".to_owned(), "s1".to_owned())].live);
        for bad in [ResourcePath::root(), session()] {
            let (mut kernel, _output) = channel(Vec::new());
            let refused = serve_one(
                &mut kernel,
                &template(),
                &options(false),
                &mut state,
                ACTION_FORGET,
                &bad,
                &empty(),
            );
            assert!(refused.unwrap().is_err());
        }
    }

    #[test]
    fn a_child_kill_denial_requires_fresh_confirmed_absence_before_success() {
        let denial = Error::new(ErrorCode::PermissionDenied, "not a child of this process");
        let child = ChildInfo {
            process: ProcessId(2),
            program: "chat".into(),
            provides: Vec::new(),
            handles: 0,
            children: 0,
        };
        for (listing, confirmed_absent) in [
            (Ok(ResponseBody::Children(Vec::new())), true),
            (Ok(ResponseBody::Children(vec![child])), false),
            (Err(Error::new(ErrorCode::Unavailable, "listing unavailable")), false),
        ] {
            let mut state = running(true);
            let posted = Ok(ResponseBody::Payload(Payload::encode(&json!({"seq": 0})).unwrap()));
            let cleared = Ok(ResponseBody::Payload(Payload::encode(&json!({})).unwrap()));
            let (mut kernel, mut output) = channel(vec![Err(denial.clone()), listing, posted, cleared]);
            let result = serve_one(
                &mut kernel,
                &template(),
                &options(false),
                &mut state,
                ACTION_STOP,
                &session(),
                &empty(),
            )
            .unwrap();
            assert_eq!(result.is_ok(), confirmed_absent);
            if !confirmed_absent {
                assert_eq!(result.unwrap_err(), denial);
            }
            assert_eq!(state.sessions[&("alpha".to_owned(), "s1".to_owned())].live, !confirmed_absent);
            // A confirmed stop closes the session's turn; an unconfirmed one changes nothing.
            drop(kernel);
            let posted = std::iter::from_fn(|| read_frame::<_, ToKernel>(&mut output).ok())
                .any(|frame| matches!(frame, ToKernel::Call { .. }));
            assert_eq!(posted, confirmed_absent);
        }
    }

    #[test]
    fn a_session_is_a_project_and_one_name() {
        assert_eq!(session_key(&session()).unwrap(), ("alpha".to_owned(), "s1".to_owned()));
        for bad in [
            ResourcePath::root(),
            ResourcePath::new(["s1"]).unwrap(),
            ResourcePath::new(["A", "s1"]).unwrap(),
        ] {
            assert!(session_key(&bad).is_err(), "{bad:?}");
        }
    }

    /// A session's agent may remind itself on its own session when this
    /// service holds that to hand on, cannot hand it on further, and is never
    /// given `wake`. It reads its own session's log, which it continues from,
    /// and cannot hand that on either; a service that could not give it that
    /// starts no agent.
    #[test]
    fn a_session_agent_gets_the_reminder_actions_held_for_it_and_never_wake() {
        let scope = Scope { project: "alpha".into() };
        let conversation_grant = |held: &[&str]| -> Vec<(u32, Vec<String>, bool)> {
            let handles = [
                handle(1, KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], &["executable"], true),
                handle(2, "conversation", held, &[], true),
                handle(3, "conversation", &["remind"], &["other", "s"], true),
                handle(4, "conversation", &["clear_reminders", "list"], &[], false),
            ];
            let (_, _, grants, _) =
                session_spawn(&options(false), &handles, &session(), &scope, (MODEL, None)).unwrap();
            let own = grants.iter().filter(|g| g.resource == session());
            let actions = |g: &Grant| g.actions.iter().map(|a| a.as_str().to_owned()).collect();
            own.map(|g| (g.from.0, actions(g), g.delegable)).collect()
        };
        let every = ["receive", "post", "history", "remind", "reminders", "cancel_reminder", "wake"];
        // Its messages, its log and its reminders it keeps.
        assert_eq!(
            conversation_grant(&every),
            [
                (2, ["post", "receive"].map(String::from).to_vec(), false),
                (2, ["history"].map(String::from).to_vec(), false),
                (2, ["cancel_reminder", "remind", "reminders"].map(String::from).to_vec(), false)
            ]
        );
        assert_eq!(
            conversation_grant(&["receive", "post", "history"]),
            [
                (2, ["post", "receive"].map(String::from).to_vec(), false),
                (2, ["history"].map(String::from).to_vec(), false)
            ],
            "a composition without reminders still starts sessions; one on another session is not handed on"
        );
        let without_history = [
            handle(1, KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], &["executable"], true),
            handle(2, "conversation", &["receive", "post"], &[], true),
            handle(4, "conversation", &["list"], &[], false),
        ];
        assert!(
            Template::new(&options(false), &without_history).is_err(),
            "it would start agents that forget"
        );
    }
}
