//! The kernel state machine.
//!
//! [`Kernel`] is deterministic and performs no I/O. A supervisor feeds it
//! authenticated inputs and the current time, then carries out the
//! [`Output`]s it drains. Every method returns promptly, so the supervisor may
//! hold a lock around each call without ever awaiting a process under it.
//!
//! Inputs: [`Kernel::boot_process`] and [`Kernel::begin_shutdown`] carry
//! operator authority (the launcher's boot and shutdown); `handle_message`,
//! `process_exited`, `process_disconnected`, `process_reaped`,
//! `project_stopped`, `refuse` and `tick` come from the supervisor. Outputs
//! are drained in order with [`Kernel::drain_outputs`].
//!
//! Ordering rules:
//! - Inputs are applied one at a time in arrival order.
//! - Authority is checked at admission and again at dispatch. An expiry, close,
//!   or holder termination applied before a queued call is dispatched rejects
//!   that call; nothing reaches the provider.
//! - A call already dispatched is not recalled by losing authority. It ends by
//!   reply, cancellation, deadline, or provider loss.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use nucleus_protocol::{
    ACTION_CREATE_ENDPOINT, ACTION_PROVIDE, ACTION_SPAWN, ACTION_SPAWN_AS, ACTION_SPAWN_EXECUTABLE,
    ACTION_STOP_PROJECT, ActionName, CREATED_ENDPOINT_PREFIX, ChildInfo, ChildProgram, CreatedEndpoint,
    DispatchId, EffectClass, EndpointName, Error, ErrorCode, FromKernel, Generation, Grant, HandleId,
    HandleInfo, IDENTITY_RESOURCE, IdentityRequest, InterfaceDecl, KERNEL_ENDPOINT, MAX_ENDPOINT_ACTIONS,
    Payload, ProcessId, ProgramName, ProjectStopped, RequestId, ResourcePath, ResponseBody, ToKernel,
    valid_project,
};

use crate::capability::{CapId, CapSpec, CapTable};
use crate::config::Config;
use crate::event::SecurityEvent;
use crate::namespace::Namespace;

/// Work the supervisor must carry out, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Deliver `msg` on `to`'s channel.
    Send {
        to: ProcessId,
        msg: FromKernel,
    },
    /// Start the host process for `process` and attach its channel. If that
    /// fails, report it with [`Kernel::process_exited`]. A configured program
    /// always runs sandboxed, as the runtime's own identity.
    StartProcess {
        process: ProcessId,
        parent: Option<ProcessId>,
        program: ProgramName,
    },
    /// Start `process` from an executable in `parent`'s namespace. The kernel
    /// has already checked the request is within `parent`'s namespace; the
    /// supervisor resolves the guest paths against `parent`'s real binds
    /// (rejecting a symlink that escapes) and confines the child. `identity`:
    /// the project it runs as, an ordinary process of the projects' user in
    /// no sandbox, in the project's cgroup; `None` is the runtime's own,
    /// sandboxed. On failure, report it with [`Kernel::process_exited`].
    StartExecutable {
        process: ProcessId,
        parent: ProcessId,
        program: Box<nucleus_protocol::ChildProgram>,
        identity: Option<String>,
    },
    /// Make sure the host process and its host descendants are gone and release
    /// the channel. All separately supervised logical children have already
    /// been reaped. The kernel withdrew the whole subtree's authority before
    /// emitting any stop; the supervisor must not stop its logical parent early.
    StopProcess {
        process: ProcessId,
    },
    /// Kill every process of `project` with its cgroup, off the kernel's
    /// lock, and report with [`Kernel::project_stopped`] once none is left,
    /// or why not.
    StopProject {
        process: ProcessId,
        request: RequestId,
        project: String,
    },
    Event(SecurityEvent),
}

/// Project stops the supervisor carries out at once, across the runtime.
pub const MAX_PROJECT_STOPS: usize = 8;

struct Handle {
    cap: CapId,
    /// Endpoint generation this handle was bound to.
    generation: Generation,
}

enum Pending {
    Queued(EndpointName),
    Inflight(EndpointName, DispatchId),
}

/// How a newly created process is to be started by the supervisor.
enum Launch {
    Configured(ProgramName),
    Executable { parent: ProcessId, program: ChildProgram },
}

struct Process {
    parent: Option<ProcessId>,
    /// The configured program name, or the executable's guest path. Reported to
    /// the owner in a [`ToKernel::Children`] listing; not authority.
    program: String,
    /// What this process can see. Used only to attenuate its own
    /// `SpawnExecutable` requests.
    namespace: Namespace,
    /// The project it runs as; `None` is the runtime's own. Set at spawn,
    /// inherited by every child, never changed.
    identity: Option<String>,
    children: BTreeSet<ProcessId>,
    handles: BTreeMap<HandleId, Handle>,
    /// Handle numbers are not reused within a process, so a closed number can
    /// never come to mean something else.
    next_handle: u32,
    pending: BTreeMap<RequestId, Pending>,
}

struct Queued {
    caller: ProcessId,
    request: RequestId,
    cap: CapId,
    action: ActionName,
    resource: ResourcePath,
    payload: Payload,
    deadline_at: u64,
}

struct Inflight {
    /// `None` once the caller cancelled or terminated; the reply is dropped.
    caller: Option<(ProcessId, RequestId)>,
    effect: EffectClass,
    deadline_at: u64,
}

struct Provider {
    process: ProcessId,
    interface: InterfaceDecl,
    authority: CapId,
}

struct OwnedEndpoint {
    owner: ProcessId,
    provide: CapId,
    invoke: CapId,
    actions: BTreeSet<ActionName>,
}

struct PendingReap {
    parent: Option<ProcessId>,
    ordinary: bool,
    /// The host may exit before separately supervised children. Keep its
    /// reservation and receipt until those cleanup obligations are gone too.
    host_success: Option<bool>,
    /// Physical stop is authorized only after all logical children are reaped.
    stop_requested: bool,
    /// A cooperative exit waits for descendants before it may close its channel.
    exit_request: Option<RequestId>,
}

#[derive(Default)]
struct Endpoint {
    /// 0 means never provided.
    generation: u64,
    provider: Option<Provider>,
    queue: VecDeque<Queued>,
    inflight: BTreeMap<DispatchId, Inflight>,
}

/// An interrupted observation did nothing; anything else may have happened.
/// A dispatched call is interrupted by the loss or replacement of its
/// provider, its deadline, or its caller's cancellation; the kernel never
/// retries it.
fn interrupted(effect: EffectClass, if_observation: ErrorCode) -> ErrorCode {
    match effect {
        EffectClass::Observation => if_observation,
        EffectClass::ReversibleMutation | EffectClass::Compensatable | EffectClass::Irreversible => {
            ErrorCode::UnknownOutcome
        }
    }
}

fn err(code: ErrorCode, message: &str) -> Error {
    Error::new(code, message)
}

/// See the module documentation.
pub struct Kernel {
    config: Config,
    shutting_down: bool,
    processes: BTreeMap<ProcessId, Process>,
    caps: CapTable,
    endpoints: BTreeMap<EndpointName, Endpoint>,
    owned_endpoints: BTreeMap<EndpointName, OwnedEndpoint>,
    next_endpoint: u64,
    pending_reaps: BTreeMap<ProcessId, PendingReap>,
    next_process: u64,
    next_dispatch: u64,
    outputs: Vec<Output>,
    /// Project stops the supervisor is carrying out, by who asked.
    project_stops: BTreeSet<(ProcessId, RequestId)>,
}

impl Kernel {
    /// Fails on an invalid configuration rather than running with a missing bound.
    pub fn new(config: Config) -> Result<Self, Error> {
        config.validate()?;
        Ok(Self {
            config,
            shutting_down: false,
            processes: BTreeMap::new(),
            caps: CapTable::default(),
            endpoints: BTreeMap::new(),
            owned_endpoints: BTreeMap::new(),
            next_endpoint: 1,
            pending_reaps: BTreeMap::new(),
            next_process: 1,
            next_dispatch: 1,
            outputs: Vec::new(),
            project_stops: BTreeSet::new(),
        })
    }

    /// Takes the outputs produced since the last drain.
    pub fn drain_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outputs)
    }

    /// True while `process` holds any authority.
    pub fn is_live(&self, process: ProcessId) -> bool {
        self.processes.contains_key(&process)
    }

    /// Authority is withdrawn, but this logical ownership obligation remains
    /// until the host and every descendant have confirmed physical cleanup.
    pub fn is_closing(&self, process: ProcessId) -> bool {
        self.pending_reaps.contains_key(&process)
    }

    /// True only for an identity allocated by this kernel whose host and whole
    /// subtree have confirmed cleanup. IDs are never reused: an absent future
    /// identity is not mistaken for a reaped process, and no tombstone table is
    /// needed. Unstarted hosts still require the supervisor's explicit receipt.
    pub fn cleanup_confirmed(&self, process: ProcessId) -> bool {
        process.0 > 0 && process.0 < self.next_process && !self.is_live(process) && !self.is_closing(process)
    }

    /// Supervisor cleanup obligations, including physically exited ancestors
    /// still waiting on descendants. An empty list alone says nothing about
    /// still-live principals; callers must stop admission first.
    pub fn closing_processes(&self) -> Vec<ProcessId> {
        self.pending_reaps.keys().copied().collect()
    }

    /// Number of live processes. Zero after shutdown.
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }

    /// The live provider of `endpoint` and its generation.
    pub fn provider_of(&self, endpoint: &EndpointName) -> Option<(ProcessId, Generation)> {
        let ep = self.endpoints.get(endpoint)?;
        ep.provider.as_ref().map(|p| (p.process, Generation(ep.generation)))
    }

    // ---- operator interface -------------------------------------------------

    /// Starts a top-level process holding root capabilities. This is operator
    /// authority: it is reachable from boot configuration, never from a message.
    pub fn boot_process(
        &mut self,
        program: ProgramName,
        grants: Vec<CapSpec>,
        now: u64,
    ) -> Result<ProcessId, Error> {
        self.check_can_start(&program, grants.len())?;
        let namespace = self.config.namespaces.get(&program).cloned().unwrap_or_default();
        let process = self.insert_process(None, program.as_str().to_owned(), namespace, None);
        for spec in grants {
            let cap = self.caps.mint_root(process, spec, now);
            self.install_handle(process, cap);
        }
        self.announce_process(process, None, Launch::Configured(program), now);
        Ok(process)
    }

    /// Stops admitting work and tears everything down, newest roots first, so
    /// a client booted after a service it uses goes before that service.
    pub fn begin_shutdown(&mut self, now: u64) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.outputs.push(Output::Event(SecurityEvent::ShutdownStarted));
        let roots: Vec<ProcessId> =
            self.processes.iter().filter(|(_, p)| p.parent.is_none()).map(|(id, _)| *id).collect();
        for root in roots.into_iter().rev() {
            self.teardown(root, now);
        }
    }

    // ---- supervisor interface -----------------------------------------------

    /// The host process behind `process` is gone, for whatever reason.
    pub fn process_exited(&mut self, process: ProcessId, now: u64) {
        self.teardown(process, now);
    }

    /// A clean transport EOF withdraws authority immediately. Only the
    /// supervisor's later host-exit observation can establish success.
    pub fn process_disconnected(&mut self, process: ProcessId, now: u64) {
        self.teardown_with_outcome(process, now, true);
    }

    /// Records confirmed host cleanup (or that the host was never started).
    /// `success` describes its exit status, not cleanup: a confirmed kill passes
    /// false. Unconfirmed cleanup must never call this method. The parent only
    /// receives a receipt after every separately supervised descendant is also
    /// reaped, including children already logically dead before this teardown.
    /// Duplicate observations and dead parents are harmless.
    pub fn process_reaped(&mut self, process: ProcessId, success: bool) {
        let Some(pending) = self.pending_reaps.get_mut(&process) else { return };
        pending.host_success.get_or_insert(success);
        self.advance_teardown();
    }

    /// Applies deadlines. Call periodically.
    pub fn tick(&mut self, now: u64) {
        self.sweep_endpoints(now);
        let names: Vec<EndpointName> = self.endpoints.keys().cloned().collect();
        for name in names {
            let Some(ep) = self.endpoints.get_mut(&name) else { continue };
            let provider = ep.provider.as_ref().map(|p| p.process);

            let (expired, waiting): (VecDeque<Queued>, VecDeque<Queued>) =
                std::mem::take(&mut ep.queue).into_iter().partition(|q| now >= q.deadline_at);
            ep.queue = waiting;

            let late: Vec<DispatchId> =
                ep.inflight.iter().filter(|(_, f)| now >= f.deadline_at).map(|(d, _)| *d).collect();
            let late: Vec<(DispatchId, Inflight)> =
                late.into_iter().filter_map(|d| ep.inflight.remove(&d).map(|f| (d, f))).collect();

            for q in expired {
                self.finish(
                    q.caller,
                    q.request,
                    Err(err(ErrorCode::Timeout, "deadline passed before dispatch")),
                );
            }
            for (dispatch, flight) in late {
                if let Some(to) = provider {
                    self.outputs.push(Output::Send { to, msg: FromKernel::CancelDispatch { dispatch } });
                }
                if let Some((caller, request)) = flight.caller {
                    let code = interrupted(flight.effect, ErrorCode::Timeout);
                    self.finish(caller, request, Err(err(code, "deadline passed after dispatch")));
                }
            }
            self.pump(&name, now);
        }
    }

    /// Applies one message. `from` must come from the channel the message
    /// arrived on; it is the only source of caller identity.
    pub fn handle_message(&mut self, from: ProcessId, msg: ToKernel, now: u64) {
        self.sweep_endpoints(now);
        if !self.processes.contains_key(&from) {
            if self.is_closing(from) {
                // Closing principals remain physically alive while their
                // children drain, but may not admit any new work or authority.
                let request = match msg {
                    ToKernel::Call { request, .. }
                    | ToKernel::Spawn { request, .. }
                    | ToKernel::SpawnExecutable { request, .. }
                    | ToKernel::Children { request, .. }
                    | ToKernel::Tools { request }
                    | ToKernel::Provide { request, .. }
                    | ToKernel::CreateEndpoint { request, .. }
                    | ToKernel::Rebind { request, .. }
                    | ToKernel::Kill { request, .. }
                    | ToKernel::Exit { request }
                    | ToKernel::Close { request, .. }
                    | ToKernel::StopProject { request, .. } => Some(request),
                    ToKernel::Cancel { .. } | ToKernel::Reply { .. } => None,
                };
                if let Some(request) = request {
                    self.respond(from, request, Err(err(ErrorCode::ShuttingDown, "process is closing")));
                }
                return;
            }
            self.outputs.push(Output::Event(SecurityEvent::ProtocolViolation {
                process: from,
                detail: "message from a process with no authority",
            }));
            return;
        }
        match msg {
            ToKernel::Call { request, handle, action, resource, payload, deadline_ms } => {
                let result = self.admit(from, request, handle, &action, &resource, payload, deadline_ms, now);
                if let Err(error) = result {
                    self.deny(from, handle, Some(action), Some(resource), &error);
                    self.respond(from, request, Err(error));
                }
            }
            ToKernel::Cancel { request } => self.cancel(from, request),
            ToKernel::Reply { dispatch, result } => self.reply(from, dispatch, result, now),
            ToKernel::Spawn { request, authority, program, grants } => {
                let result = self.spawn(from, authority, program, grants, now);
                self.conclude(from, request, authority, result);
            }
            ToKernel::SpawnExecutable { request, authority, program, grants, identity } => {
                let result = self.spawn_executable(from, authority, program, grants, identity, now);
                self.conclude(from, request, authority, result);
            }
            ToKernel::Provide { request, authority, interface } => {
                let result = self.provide(from, authority, interface, now);
                self.conclude(from, request, authority, result);
            }
            ToKernel::CreateEndpoint { request, authority, actions } => {
                let result = self.create_endpoint(from, authority, actions, now);
                self.conclude(from, request, authority, result);
            }
            ToKernel::Rebind { request, handle } => {
                let result = self.rebind(from, handle, now);
                self.conclude(from, request, handle, result);
            }
            ToKernel::Kill { request, child } => {
                let result = self.kill(from, child, now);
                if let Err(error) = &result {
                    self.outputs.push(Output::Event(SecurityEvent::Denied {
                        process: from,
                        endpoint: None,
                        action: None,
                        resource: None,
                        reason: error.code,
                    }));
                }
                self.respond(from, request, result);
            }
            ToKernel::Exit { request } => {
                self.withdraw_subtree(from, now, true);
                if let Some(pending) = self.pending_reaps.get_mut(&from) {
                    pending.exit_request = Some(request);
                }
                self.advance_teardown();
            }
            ToKernel::Children { request, depth } => {
                let result = self.children(from, depth);
                self.respond(from, request, result);
            }
            ToKernel::Tools { request } => {
                let result = self.tools(from, now);
                self.respond(from, request, result);
            }
            ToKernel::Close { request, handle } => {
                let result = self.close(from, handle, now);
                self.conclude(from, request, handle, result);
            }
            ToKernel::StopProject { request, authority, project } => {
                match self.stop_project(from, authority, &project, now) {
                    Ok(()) => {
                        self.project_stops.insert((from, request));
                        self.outputs.push(Output::StopProject { process: from, request, project });
                    }
                    Err(error) => self.conclude(from, request, authority, Err(error)),
                }
            }
        }
        self.sweep_endpoints(now);
    }

    /// How a project stop the supervisor carried out ended: `Ok` once
    /// nothing of the project runs. Answers the process that asked, if it is
    /// still there.
    pub fn project_stopped(&mut self, process: ProcessId, request: RequestId, result: Result<(), Error>) {
        if !self.project_stops.remove(&(process, request)) {
            return;
        }
        if self.processes.contains_key(&process) {
            let result = result.and_then(|()| Payload::encode(&ProjectStopped {}).map(ResponseBody::Payload));
            self.respond(process, request, result);
        }
    }

    /// Answers `request` of `process` with `error`: a request the supervisor
    /// refused before the kernel saw it, for a limit of the host the kernel
    /// does not count (the projects with processes, counted by their
    /// cgroups).
    pub fn refuse(&mut self, process: ProcessId, request: RequestId, error: Error) {
        if self.processes.contains_key(&process) {
            self.respond(process, request, Err(error));
        }
    }

    /// Admits a stop of every process of a project: only a process of the
    /// runtime's own identity, holding `stop_project` on `["identity",
    /// <project>]`, in a deployment with projects, and only a few at once.
    fn stop_project(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        project: &str,
        now: u64,
    ) -> Result<(), Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        if self.processes.get(&from).is_some_and(|p| p.identity.is_some()) {
            return Err(err(ErrorCode::PermissionDenied, "a project's process stops no project"));
        }
        if !valid_project(project) {
            return Err(err(ErrorCode::InvalidInput, "a project is named by a project name"));
        }
        let cap = self.handle_of(from, authority)?.cap;
        let action = ActionName::new(ACTION_STOP_PROJECT)?;
        self.caps.authorize(
            cap,
            KERNEL_ENDPOINT,
            &action,
            &ResourcePath::new([IDENTITY_RESOURCE, project])?,
            now,
        )?;
        if !self.config.projects {
            return Err(err(ErrorCode::Unavailable, "this runtime runs no project's processes"));
        }
        if self.project_stops.len() >= MAX_PROJECT_STOPS {
            return Err(err(ErrorCode::LimitExceeded, "too many project stops at once; try again shortly"));
        }
        Ok(())
    }

    // ---- responses and events -----------------------------------------------

    fn respond(&mut self, to: ProcessId, request: RequestId, result: Result<ResponseBody, Error>) {
        self.outputs.push(Output::Send { to, msg: FromKernel::Response { request, result } });
    }

    fn conclude(
        &mut self,
        from: ProcessId,
        request: RequestId,
        handle: HandleId,
        result: Result<ResponseBody, Error>,
    ) {
        if let Err(error) = &result {
            self.deny(from, handle, None, None, error);
        }
        self.respond(from, request, result);
    }

    /// Completes an outstanding call: clears the caller's bookkeeping and responds.
    fn finish(&mut self, caller: ProcessId, request: RequestId, result: Result<ResponseBody, Error>) {
        if let Some(process) = self.processes.get_mut(&caller) {
            process.pending.remove(&request);
            self.respond(caller, request, result);
        }
    }

    fn deny(
        &mut self,
        process: ProcessId,
        handle: HandleId,
        action: Option<ActionName>,
        resource: Option<ResourcePath>,
        error: &Error,
    ) {
        let authority_related = matches!(
            error.code,
            ErrorCode::PermissionDenied
                | ErrorCode::UnknownHandle
                | ErrorCode::StaleHandle
                | ErrorCode::Revoked
                | ErrorCode::BudgetExhausted
        );
        if !authority_related {
            return;
        }
        let endpoint = self
            .processes
            .get(&process)
            .and_then(|p| p.handles.get(&handle))
            .and_then(|h| self.caps.endpoint_of(h.cap).cloned());
        self.outputs.push(Output::Event(SecurityEvent::Denied {
            process,
            endpoint,
            action,
            resource,
            reason: error.code,
        }));
    }

    // ---- processes and handles ----------------------------------------------

    fn check_can_start(&self, program: &ProgramName, handles: usize) -> Result<(), Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        if !self.config.programs.contains(program) {
            return Err(err(ErrorCode::InvalidInput, "program is not configured"));
        }
        if self.processes.len() + self.pending_reaps.len() >= self.config.max_processes {
            return Err(err(ErrorCode::LimitExceeded, "process limit reached"));
        }
        if handles > self.config.max_handles_per_process {
            return Err(err(ErrorCode::LimitExceeded, "too many handles for one process"));
        }
        Ok(())
    }

    fn insert_process(
        &mut self,
        parent: Option<ProcessId>,
        program: String,
        namespace: Namespace,
        identity: Option<String>,
    ) -> ProcessId {
        let process = ProcessId(self.next_process);
        self.next_process += 1;
        self.processes.insert(
            process,
            Process {
                parent,
                program,
                namespace,
                identity,
                children: BTreeSet::new(),
                handles: BTreeMap::new(),
                next_handle: 0,
                pending: BTreeMap::new(),
            },
        );
        if let Some(parent) = parent.and_then(|p| self.processes.get_mut(&p)) {
            parent.children.insert(process);
        }
        process
    }

    fn install_handle(&mut self, process: ProcessId, cap: CapId) {
        let generation = self
            .caps
            .endpoint_of(cap)
            .and_then(|name| self.endpoints.get(name))
            .map_or(0, |ep| ep.generation);
        if let Some(p) = self.processes.get_mut(&process) {
            let id = HandleId(p.next_handle);
            p.next_handle += 1;
            p.handles.insert(id, Handle { cap, generation: Generation(generation) });
        }
    }

    fn announce_process(&mut self, process: ProcessId, parent: Option<ProcessId>, launch: Launch, now: u64) {
        let handles = self.describe_handles(process, now);
        let display = match &launch {
            Launch::Configured(program) => program.as_str().to_owned(),
            Launch::Executable { program, .. } => program.executable.clone(),
        };
        let identity = self.processes.get(&process).and_then(|p| p.identity.clone());
        self.outputs.push(Output::Event(SecurityEvent::ProcessStarted {
            process,
            parent,
            program: display,
            identity: identity.clone(),
        }));
        self.outputs.push(match launch {
            Launch::Configured(program) => Output::StartProcess { process, parent, program },
            Launch::Executable { parent, program } => {
                Output::StartExecutable { process, parent, program: Box::new(program), identity }
            }
        });
        self.outputs.push(Output::Send { to: process, msg: FromKernel::Hello { process, handles } });
    }

    fn describe_handles(&self, process: ProcessId, now: u64) -> Vec<HandleInfo> {
        let Some(p) = self.processes.get(&process) else { return Vec::new() };
        p.handles
            .iter()
            .filter_map(|(id, h)| {
                let cap = self.caps.validate(h.cap, now).ok()?;
                Some(HandleInfo {
                    handle: *id,
                    endpoint: cap.endpoint.clone(),
                    actions: cap.actions.clone(),
                    resource: cap.resource.clone(),
                    delegable: cap.delegable,
                    expires_in_ms: cap.expires_at.map(|at| at.saturating_sub(now)),
                    max_calls: cap.max_calls,
                })
            })
            .collect()
    }

    /// Looks `handle` up in `from`'s own table. Another process's handle number
    /// or an invented one can only miss or hit something `from` already holds.
    fn handle_of(&self, from: ProcessId, handle: HandleId) -> Result<&Handle, Error> {
        self.processes
            .get(&from)
            .and_then(|p| p.handles.get(&handle))
            .ok_or_else(|| err(ErrorCode::UnknownHandle, "no such handle"))
    }

    /// Withdraws everything `process` owns, dependents first: its children
    /// (newest first), its outstanding calls, the endpoints it provides, then
    /// its capabilities (which invalidates anything delegated from them). An
    /// exit or crash, a failed start, a slow consumer, `Kill`, the operator's
    /// terminate and shutdown all end here; a clean disconnect and `Exit`
    /// withdraw the same way, marked ordinary. A process already withdrawn is
    /// left as it is.
    fn teardown(&mut self, process: ProcessId, now: u64) {
        self.teardown_with_outcome(process, now, false);
    }

    fn teardown_with_outcome(&mut self, process: ProcessId, now: u64, ordinary: bool) {
        self.withdraw_subtree(process, now, ordinary);
        self.advance_teardown();
    }

    /// Withdraw the entire logical subtree before requesting any physical stop.
    /// Pending ownership edges survive removal from the live authority table.
    fn withdraw_subtree(&mut self, process: ProcessId, now: u64, ordinary: bool) {
        let Some(children) = self.processes.get(&process).map(|p| p.children.clone()) else { return };
        for child in children.into_iter().rev() {
            self.withdraw_subtree(child, now, false);
        }
        let Some(removed) = self.processes.remove(&process) else { return };

        for (request, pending) in removed.pending {
            match pending {
                Pending::Queued(name) => {
                    if let Some(ep) = self.endpoints.get_mut(&name) {
                        ep.queue.retain(|q| !(q.caller == process && q.request == request));
                    }
                }
                Pending::Inflight(name, dispatch) => {
                    let Some(ep) = self.endpoints.get_mut(&name) else { continue };
                    if let Some(flight) = ep.inflight.get_mut(&dispatch) {
                        flight.caller = None;
                    }
                    if let Some(to) = ep.provider.as_ref().map(|p| p.process) {
                        self.outputs.push(Output::Send { to, msg: FromKernel::CancelDispatch { dispatch } });
                    }
                }
            }
        }

        let provided: Vec<EndpointName> = self
            .endpoints
            .iter()
            .filter(|(_, ep)| ep.provider.as_ref().is_some_and(|p| p.process == process))
            .map(|(name, _)| name.clone())
            .collect();
        for name in provided {
            self.drop_provider(&name, false);
        }

        self.caps.remove_held_by(process);
        self.sweep_endpoints(now);
        self.sweep_queues(now);

        self.pending_reaps.insert(
            process,
            PendingReap {
                parent: removed.parent,
                ordinary,
                host_success: None,
                stop_requested: false,
                exit_request: None,
            },
        );
        if let Some(parent) = removed.parent
            && let Some(p) = self.processes.get_mut(&parent)
        {
            p.children.remove(&process);
            self.outputs.push(Output::Send { to: parent, msg: FromKernel::ChildExited { child: process } });
        }
        self.outputs.push(Output::Event(SecurityEvent::ProcessStopped { process }));
    }

    /// A leaf may stop; its parent becomes a leaf only on confirmed host reap.
    /// Missing cleanup evidence keeps the edge and budget reserved indefinitely:
    /// neither elapsed time nor a parent's own crash proves descendant cleanup.
    fn advance_teardown(&mut self) {
        loop {
            let parents: BTreeSet<_> = self.pending_reaps.values().filter_map(|p| p.parent).collect();
            let leaves: Vec<_> =
                self.pending_reaps.keys().rev().filter(|id| !parents.contains(id)).copied().collect();
            let mut completed = false;
            for process in leaves {
                let Some(pending) = self.pending_reaps.get_mut(&process) else { continue };
                if let Some(success) = pending.host_success {
                    let parent = pending.parent;
                    let ordinary = pending.ordinary;
                    self.pending_reaps.remove(&process);
                    completed = true;
                    if let Some(parent) = parent.filter(|id| self.is_live(*id)) {
                        self.outputs.push(Output::Send {
                            to: parent,
                            msg: FromKernel::ChildReaped {
                                child: process,
                                success: success && ordinary,
                                cleanup_confirmed: true,
                            },
                        });
                    }
                } else if !pending.stop_requested {
                    pending.stop_requested = true;
                    if let Some(request) = pending.exit_request.take() {
                        self.respond(process, request, Ok(ResponseBody::Done));
                    }
                    self.outputs.push(Output::StopProcess { process });
                }
            }
            if !completed {
                break;
            }
        }
    }

    // ---- endpoints ----------------------------------------------------------

    /// An allocated object remains owned by its creator, even while a child
    /// provides it. Removing either root capability destroys the object; the
    /// reserved generated name cannot later be claimed through broad provide.
    /// Any provider that no longer holds valid `provide` authority is unbound.
    fn sweep_endpoints(&mut self, now: u64) {
        let dead: Vec<_> = self
            .owned_endpoints
            .iter()
            .filter_map(|(name, owned)| {
                (self.caps.validate(owned.provide, now).is_err()
                    || self.caps.validate(owned.invoke, now).is_err())
                .then_some(name.clone())
            })
            .collect();
        if !dead.is_empty() {
            self.sweep_queues(now);
        }
        for name in dead {
            self.drop_provider(&name, true);
            self.endpoints.remove(&name);
            if let Some(owned) = self.owned_endpoints.remove(&name) {
                self.caps.remove(owned.provide);
                self.caps.remove(owned.invoke);
                self.outputs.push(Output::Event(SecurityEvent::EndpointReleased {
                    endpoint: name,
                    owner: owned.owner,
                }));
            }
        }
        // A provider whose `provide` authority expired or was closed
        // is withdrawn exactly as if it had exited; for an allocated endpoint
        // the creator retains the allocation and may start a replacement.
        let withdrawn: Vec<_> = self
            .endpoints
            .iter()
            .filter(|(_, endpoint)| {
                endpoint
                    .provider
                    .as_ref()
                    .is_some_and(|provider| self.caps.validate(provider.authority, now).is_err())
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in withdrawn {
            self.drop_provider(&name, true);
        }
    }

    fn owns_invocation(&self, name: &EndpointName, cap: CapId) -> bool {
        !name.as_str().starts_with(CREATED_ENDPOINT_PREFIX)
            || self.owned_endpoints.get(name).is_some_and(|owned| self.caps.belongs_to(cap, owned.invoke))
    }

    /// Detaches the provider. Queued calls were never delivered and fail as
    /// `unavailable`; delivered calls fail according to their effect class.
    /// Nothing is carried over to a later provider.
    fn drop_provider(&mut self, name: &EndpointName, provider_still_running: bool) {
        let Some(ep) = self.endpoints.get_mut(name) else { return };
        let Some(old) = ep.provider.take() else { return };
        let generation = Generation(ep.generation);
        let queue = std::mem::take(&mut ep.queue);
        let inflight = std::mem::take(&mut ep.inflight);

        for q in queue {
            self.finish(
                q.caller,
                q.request,
                Err(err(ErrorCode::Unavailable, "provider went away before dispatch")),
            );
        }
        for (dispatch, flight) in inflight {
            if provider_still_running {
                self.outputs
                    .push(Output::Send { to: old.process, msg: FromKernel::CancelDispatch { dispatch } });
            }
            if let Some((caller, request)) = flight.caller {
                let code = interrupted(flight.effect, ErrorCode::Unavailable);
                self.finish(caller, request, Err(err(code, "provider went away after dispatch")));
            }
        }
        self.outputs.push(Output::Event(SecurityEvent::EndpointDown { endpoint: name.clone(), generation }));
    }

    /// Rejects queued calls whose authority has gone since admission.
    fn sweep_queues(&mut self, now: u64) {
        let names: Vec<EndpointName> = self.endpoints.keys().cloned().collect();
        for name in names {
            let Some(ep) = self.endpoints.get_mut(&name) else { continue };
            let caps = &self.caps;
            let (keep, reject): (VecDeque<Queued>, VecDeque<Queued>) =
                std::mem::take(&mut ep.queue).into_iter().partition(|q| caps.validate(q.cap, now).is_ok());
            ep.queue = keep;
            for q in reject {
                self.outputs.push(Output::Event(SecurityEvent::Denied {
                    process: q.caller,
                    endpoint: Some(name.clone()),
                    action: Some(q.action),
                    resource: Some(q.resource),
                    reason: ErrorCode::Revoked,
                }));
                self.finish(
                    q.caller,
                    q.request,
                    Err(err(ErrorCode::Revoked, "authority withdrawn while queued")),
                );
            }
        }
    }

    // ---- calls --------------------------------------------------------------

    /// Admits a `Call` from `from`, the process of the channel it came on. In
    /// order: the kernel is not shutting down; the request id is not
    /// outstanding and the caller is within its pending limit; the handle is
    /// in the caller's table; its capability is valid, not on the kernel
    /// endpoint, and covers the action and resource; it derives from the
    /// endpoint's invocation authority; it has budget left; its generation is
    /// the endpoint's current one; the endpoint has a provider whose interface
    /// declares the action; the queue has room. A failure is answered at once
    /// and nothing reaches the provider. An admitted call waits in the
    /// endpoint's queue until [`Self::pump`] dispatches it.
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &mut self,
        from: ProcessId,
        request: RequestId,
        handle: HandleId,
        action: &ActionName,
        resource: &ResourcePath,
        payload: Payload,
        deadline_ms: Option<u64>,
        now: u64,
    ) -> Result<(), Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        let caller =
            self.processes.get(&from).ok_or_else(|| err(ErrorCode::PermissionDenied, "unknown process"))?;
        if caller.pending.contains_key(&request) {
            return Err(err(ErrorCode::InvalidInput, "request id already outstanding"));
        }
        if caller.pending.len() >= self.config.max_pending_per_caller {
            return Err(err(ErrorCode::LimitExceeded, "too many outstanding requests"));
        }
        let held = self.handle_of(from, handle)?;
        let (cap_id, bound) = (held.cap, held.generation);
        let name = self.caps.validate(cap_id, now)?.endpoint.clone();
        if name.as_str() == KERNEL_ENDPOINT {
            return Err(err(ErrorCode::InvalidInput, "kernel authority is not callable"));
        }
        self.caps.authorize(cap_id, name.as_str(), action, resource, now)?;
        if !self.owns_invocation(&name, cap_id) {
            return Err(err(
                ErrorCode::PermissionDenied,
                "handle is not derived from this endpoint's invocation authority",
            ));
        }
        if !self.caps.has_budget(cap_id) {
            return Err(err(ErrorCode::BudgetExhausted, "call budget spent"));
        }

        let ep = self
            .endpoints
            .get_mut(&name)
            .ok_or_else(|| err(ErrorCode::Unavailable, "endpoint has no provider"))?;
        if bound.0 != ep.generation {
            return Err(err(ErrorCode::StaleHandle, "endpoint was rebound; rebind the handle explicitly"));
        }
        let provider =
            ep.provider.as_ref().ok_or_else(|| err(ErrorCode::Unavailable, "endpoint has no provider"))?;
        if !provider.interface.actions.contains_key(action) {
            return Err(err(ErrorCode::InvalidInput, "action is not part of the interface"));
        }
        if ep.queue.len() >= self.config.max_queue_per_endpoint {
            return Err(err(ErrorCode::QueueFull, "endpoint queue is full"));
        }

        let wait = deadline_ms.unwrap_or(self.config.default_deadline_ms).min(self.config.max_deadline_ms);
        ep.queue.push_back(Queued {
            caller: from,
            request,
            cap: cap_id,
            action: action.clone(),
            resource: resource.clone(),
            payload,
            deadline_at: now.saturating_add(wait),
        });
        if let Some(p) = self.processes.get_mut(&from) {
            p.pending.insert(request, Pending::Queued(name.clone()));
        }
        self.pump(&name, now);
        Ok(())
    }

    /// Dispatches queued calls while the provider has capacity. Authority is
    /// checked again here, immediately before delivery.
    fn pump(&mut self, name: &EndpointName, now: u64) {
        loop {
            let Some(ep) = self.endpoints.get_mut(name) else { return };
            let Some(provider) = ep.provider.as_ref() else { return };
            if ep.inflight.len() >= self.config.max_inflight_per_endpoint {
                return;
            }
            let Some(q) = ep.queue.pop_front() else { return };
            let to = provider.process;
            let effect = provider.interface.actions.get(&q.action).map(|decl| decl.effect);

            let checked = if now >= q.deadline_at {
                Err(err(ErrorCode::Timeout, "deadline passed before dispatch"))
            } else {
                self.caps
                    .authorize(q.cap, name.as_str(), &q.action, &q.resource, now)
                    .map(|_| ())
                    .and_then(|()| self.caps.charge(q.cap))
                    .and_then(|()| {
                        effect.ok_or_else(|| {
                            err(ErrorCode::InvalidInput, "action is not part of the interface")
                        })
                    })
            };
            let effect = match checked {
                Ok(effect) => effect,
                Err(error) => {
                    self.outputs.push(Output::Event(SecurityEvent::Denied {
                        process: q.caller,
                        endpoint: Some(name.clone()),
                        action: Some(q.action),
                        resource: Some(q.resource),
                        reason: error.code,
                    }));
                    self.finish(q.caller, q.request, Err(error));
                    continue;
                }
            };

            let dispatch = DispatchId(self.next_dispatch);
            self.next_dispatch += 1;
            if let Some(ep) = self.endpoints.get_mut(name) {
                ep.inflight.insert(
                    dispatch,
                    Inflight { caller: Some((q.caller, q.request)), effect, deadline_at: q.deadline_at },
                );
            }
            if let Some(p) = self.processes.get_mut(&q.caller) {
                p.pending.insert(q.request, Pending::Inflight(name.clone(), dispatch));
            }
            self.outputs.push(Output::Send {
                to,
                msg: FromKernel::Dispatch {
                    dispatch,
                    endpoint: name.clone(),
                    caller: q.caller,
                    action: q.action,
                    resource: q.resource,
                    payload: q.payload,
                    remaining_ms: q.deadline_at.saturating_sub(now),
                },
            });
        }
    }

    fn cancel(&mut self, from: ProcessId, request: RequestId) {
        // An unknown id means the response already went out; that race is benign.
        let Some(pending) = self.processes.get_mut(&from).and_then(|p| p.pending.remove(&request)) else {
            return;
        };
        match pending {
            Pending::Queued(name) => {
                if let Some(ep) = self.endpoints.get_mut(&name) {
                    ep.queue.retain(|q| !(q.caller == from && q.request == request));
                }
                self.respond(from, request, Err(err(ErrorCode::Cancelled, "cancelled before dispatch")));
            }
            Pending::Inflight(name, dispatch) => {
                let Some(ep) = self.endpoints.get_mut(&name) else { return };
                let Some(flight) = ep.inflight.get_mut(&dispatch) else { return };
                flight.caller = None;
                let code = interrupted(flight.effect, ErrorCode::Cancelled);
                if let Some(to) = ep.provider.as_ref().map(|p| p.process) {
                    self.outputs.push(Output::Send { to, msg: FromKernel::CancelDispatch { dispatch } });
                }
                self.respond(from, request, Err(err(code, "cancelled after dispatch")));
            }
        }
    }

    fn reply(&mut self, from: ProcessId, dispatch: DispatchId, result: Result<Payload, Error>, now: u64) {
        // Only endpoints `from` provides are searched, so a process cannot
        // complete a dispatch that was delivered to someone else.
        let found = self.endpoints.iter_mut().find_map(|(name, ep)| {
            if ep.provider.as_ref().is_some_and(|p| p.process == from) {
                ep.inflight.remove(&dispatch).map(|flight| (name.clone(), flight))
            } else {
                None
            }
        });
        // A miss is a reply that lost a race with a deadline or cancellation.
        let Some((name, flight)) = found else { return };

        if let Some((caller, request)) = flight.caller {
            let result = match result {
                Ok(payload) => Ok(ResponseBody::Payload(payload)),
                Err(error) => Err(Self::provider_error(error)),
            };
            self.finish(caller, request, result);
        }
        self.pump(&name, now);
    }

    /// A provider may report its own failures but cannot impersonate a kernel
    /// decision such as `stale_handle` or `revoked`.
    fn provider_error(error: Error) -> Error {
        match error.code {
            ErrorCode::InvalidInput
            | ErrorCode::PermissionDenied
            | ErrorCode::UnknownOutcome
            | ErrorCode::ServiceError => error,
            _ => Error::new(ErrorCode::ServiceError, error.message),
        }
    }

    // ---- kernel operations --------------------------------------------------

    fn kernel_authority(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        action: &str,
        kind: &str,
        name: &str,
        now: u64,
    ) -> Result<(), Error> {
        let cap = self.handle_of(from, authority)?.cap;
        let action = ActionName::new(action)?;
        let resource = ResourcePath::new([kind, name])?;
        self.caps.authorize(cap, KERNEL_ENDPOINT, &action, &resource, now)?;
        self.caps.charge(cap)
    }

    fn spawn(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        program: ProgramName,
        grants: Vec<Grant>,
        now: u64,
    ) -> Result<ResponseBody, Error> {
        self.check_can_start(&program, grants.len())?;
        let child = ProcessId(self.next_process);
        let planned = self.plan_grants(from, child, grants, now)?;

        let namespace = self.config.namespaces.get(&program).cloned().unwrap_or_default();
        self.processes
            .get(&from)
            .ok_or_else(|| err(ErrorCode::PermissionDenied, "no such process"))?
            .namespace
            .admits(&namespace)?;
        // A configured program is a sandbox recipe of the runtime's own; a
        // project's process starts executables, as its project.
        if self.processes.get(&from).is_some_and(|p| p.identity.is_some()) {
            return Err(err(
                ErrorCode::PermissionDenied,
                "a process running as a project identity starts executables, not configured programs",
            ));
        }
        self.kernel_authority(from, authority, ACTION_SPAWN, "program", program.as_str(), now)?;
        let created = self.insert_process(Some(from), program.as_str().to_owned(), namespace, None);
        debug_assert_eq!(created, child);
        self.commit_grants(from, child, planned);
        self.announce_process(child, Some(from), Launch::Configured(program), now);
        Ok(ResponseBody::Spawned { child })
    }

    /// Starts a child from an executable the caller can itself read, with a
    /// namespace and grants that are subsets of the caller's own, as the
    /// caller's identity or, with `spawn_as`, as a project.
    fn spawn_executable(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        identity: Option<IdentityRequest>,
        now: u64,
    ) -> Result<ResponseBody, Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        if self.processes.len() + self.pending_reaps.len() >= self.config.max_processes {
            return Err(err(ErrorCode::LimitExceeded, "process limit reached"));
        }
        if grants.len() > self.config.max_handles_per_process {
            return Err(err(ErrorCode::LimitExceeded, "too many handles for one process"));
        }
        let (caller_ns, inherited) = self
            .processes
            .get(&from)
            .map(|p| (p.namespace.clone(), p.identity.clone()))
            .ok_or_else(|| err(ErrorCode::PermissionDenied, "no such process"))?;
        let chosen = identity.is_some();
        let identity = match identity {
            None => inherited,
            Some(asked) => {
                // Only the runtime's own processes choose an identity; a
                // project's processes never become anything else.
                if inherited.is_some() {
                    return Err(err(
                        ErrorCode::PermissionDenied,
                        "a process running as a project identity starts children only as that identity",
                    ));
                }
                if !valid_project(&asked.project) {
                    return Err(err(ErrorCode::InvalidInput, "a project is named by a project name"));
                }
                let cap = self.handle_of(from, asked.authority)?.cap;
                let action = ActionName::new(ACTION_SPAWN_AS)?;
                let resource = ResourcePath::new([IDENTITY_RESOURCE, asked.project.as_str()])?;
                self.caps.authorize(cap, KERNEL_ENDPOINT, &action, &resource, now)?;
                if !self.config.projects {
                    return Err(err(ErrorCode::Unavailable, "this runtime runs no project's processes"));
                }
                Some(asked.project)
            }
        };
        // A sandboxed child's namespace must fit within the caller's; this is
        // where a wider bind, a claimed network, or a bad path is refused,
        // before anything is created. A project's process is an ordinary
        // process of the projects' user, in no sandbox and with that user's
        // network. The executable's real readability and any symlink escape
        // are the platform's to check at start.
        let namespace = match &identity {
            None => caller_ns.attenuate(&program)?,
            Some(_) => caller_ns.project(&program, chosen)?,
        };

        let child = ProcessId(self.next_process);
        let planned = self.plan_grants(from, child, grants, now)?;
        // Authority to spawn is over the executable's guest path, so a narrow
        // grant (e.g. `["executable", "plugins"]`) covers only part of the tree.
        let exec_resource: Vec<&str> =
            std::iter::once("executable").chain(crate::namespace::segments(&program.executable)?).collect();
        let resource = ResourcePath::new(exec_resource)?;
        let cap = self.handle_of(from, authority)?.cap;
        let action = ActionName::new(ACTION_SPAWN_EXECUTABLE)?;
        self.caps.authorize(cap, KERNEL_ENDPOINT, &action, &resource, now)?;
        self.caps.charge(cap)?;

        let created = self.insert_process(Some(from), program.executable.clone(), namespace, identity);
        debug_assert_eq!(created, child);
        self.commit_grants(from, child, planned);
        self.announce_process(child, Some(from), Launch::Executable { parent: from, program }, now);
        Ok(ResponseBody::Spawned { child })
    }

    /// Plans every delegation before anything is created, so a rejected grant
    /// leaves no process and no capability behind.
    fn plan_grants(
        &mut self,
        from: ProcessId,
        child: ProcessId,
        grants: Vec<Grant>,
        now: u64,
    ) -> Result<Vec<crate::capability::Capability>, Error> {
        let mut planned = Vec::with_capacity(grants.len());
        for grant in grants {
            let source = self.handle_of(from, grant.from)?.cap;
            let endpoint = self.caps.validate(source, now)?.endpoint.clone();
            let spec = CapSpec {
                endpoint,
                actions: grant.actions,
                resource: grant.resource,
                ttl_ms: grant.ttl_ms,
                delegable: grant.delegable,
                max_calls: grant.max_calls,
            };
            planned.push(self.caps.plan_derive(
                source,
                child,
                spec,
                now,
                self.config.max_delegation_depth,
            )?);
        }
        Ok(planned)
    }

    fn commit_grants(
        &mut self,
        from: ProcessId,
        child: ProcessId,
        planned: Vec<crate::capability::Capability>,
    ) {
        for cap in planned {
            self.outputs.push(Output::Event(SecurityEvent::Delegated {
                from,
                to: child,
                endpoint: cap.endpoint.clone(),
                actions: cap.actions.iter().cloned().collect(),
                resource: cap.resource.clone(),
            }));
            let id = self.caps.commit_derive(cap);
            self.install_handle(child, id);
        }
    }

    /// Allocates a fresh `dynamic.<n>` endpoint and installs two delegable
    /// handles derived from the factory: `provide` on `["endpoint", <name>]`
    /// and invocation of exactly the requested actions. Authority over an
    /// endpoint that already exists comes only from boot grants and their
    /// delegation. The endpoint goes when either handle's capability goes
    /// (closed, expired, or removed at its creator's exit:
    /// [`Self::sweep_endpoints`]); a provider's exit only withdraws the
    /// provider, and the creator may start another.
    fn create_endpoint(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        actions: BTreeSet<ActionName>,
        now: u64,
    ) -> Result<ResponseBody, Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        if actions.is_empty() || actions.len() > MAX_ENDPOINT_ACTIONS {
            return Err(err(ErrorCode::InvalidInput, "endpoint must declare 1..=64 invocation actions"));
        }
        let process =
            self.processes.get(&from).ok_or_else(|| err(ErrorCode::PermissionDenied, "no such process"))?;
        if process.handles.len().saturating_add(2) > self.config.max_handles_per_process
            || process.next_handle.checked_add(2).is_none()
        {
            return Err(err(ErrorCode::LimitExceeded, "endpoint creation needs two free handles"));
        }
        let provide_id = HandleId(process.next_handle);
        let invoke_id = HandleId(process.next_handle + 1);
        let factory = self.handle_of(from, authority)?.cap;
        self.caps.authorize(
            factory,
            KERNEL_ENDPOINT,
            &ActionName::new(ACTION_CREATE_ENDPOINT)?,
            &ResourcePath::new(["endpoint"])?,
            now,
        )?;
        let next_endpoint = self
            .next_endpoint
            .checked_add(1)
            .ok_or_else(|| err(ErrorCode::LimitExceeded, "endpoint identity space exhausted"))?;
        let endpoint = EndpointName::new(format!("{CREATED_ENDPOINT_PREFIX}{}", self.next_endpoint))?;
        let provide = self.caps.plan_created(
            factory,
            from,
            EndpointName::new(KERNEL_ENDPOINT)?,
            BTreeSet::from([ActionName::new(ACTION_PROVIDE)?]),
            ResourcePath::new(["endpoint", endpoint.as_str()])?,
            now,
            self.config.max_delegation_depth,
        )?;
        let invoke = self.caps.plan_created(
            factory,
            from,
            endpoint.clone(),
            actions.clone(),
            ResourcePath::root(),
            now,
            self.config.max_delegation_depth,
        )?;
        let describe = |handle, cap: &crate::capability::Capability| HandleInfo {
            handle,
            endpoint: cap.endpoint.clone(),
            actions: cap.actions.clone(),
            resource: cap.resource.clone(),
            delegable: cap.delegable,
            expires_in_ms: cap.expires_at.map(|at| at.saturating_sub(now)),
            max_calls: cap.max_calls,
        };
        let created = CreatedEndpoint {
            endpoint: endpoint.clone(),
            provide: describe(provide_id, &provide),
            invoke: describe(invoke_id, &invoke),
        };
        // All fallible planning precedes the single factory charge/commit.
        self.caps.charge(factory)?;
        self.next_endpoint = next_endpoint;
        let provide = self.caps.commit_derive(provide);
        let invoke = self.caps.commit_derive(invoke);
        self.install_handle(from, provide);
        self.install_handle(from, invoke);
        self.endpoints.insert(endpoint.clone(), Endpoint::default());
        self.owned_endpoints.insert(
            endpoint.clone(),
            OwnedEndpoint { owner: from, provide, invoke, actions: actions.clone() },
        );
        self.outputs.push(Output::Event(SecurityEvent::EndpointCreated {
            endpoint,
            owner: from,
            actions: actions.into_iter().collect(),
        }));
        Ok(ResponseBody::EndpointCreated(Box::new(created)))
    }

    fn provide(
        &mut self,
        from: ProcessId,
        authority: HandleId,
        interface: InterfaceDecl,
        now: u64,
    ) -> Result<ResponseBody, Error> {
        if self.shutting_down {
            return Err(err(ErrorCode::ShuttingDown, "kernel is shutting down"));
        }
        let name = interface.endpoint.clone();
        if name.as_str() == KERNEL_ENDPOINT {
            return Err(err(ErrorCode::PermissionDenied, "the kernel endpoint cannot be provided"));
        }
        if interface.actions.is_empty() {
            return Err(err(ErrorCode::InvalidInput, "interface declares no actions"));
        }
        let cap = self.handle_of(from, authority)?.cap;
        if name.as_str().starts_with(CREATED_ENDPOINT_PREFIX) {
            let owned = self
                .owned_endpoints
                .get(&name)
                .ok_or_else(|| err(ErrorCode::PermissionDenied, "generated endpoint is not allocated"))?;
            if !self.caps.belongs_to(cap, owned.provide)
                || !interface.actions.keys().all(|action| owned.actions.contains(action))
            {
                return Err(err(
                    ErrorCode::PermissionDenied,
                    "interface exceeds this endpoint's creation authority",
                ));
            }
        }
        self.kernel_authority(from, authority, ACTION_PROVIDE, "endpoint", name.as_str(), now)?;

        // Replacement: the old generation's work is failed, never transferred.
        // The displaced provider is not stopped: it is told `CancelDispatch`
        // for what it was serving and can complete none of it.
        self.drop_provider(&name, true);
        let ep = self.endpoints.entry(name.clone()).or_default();
        ep.generation += 1;
        ep.provider = Some(Provider { process: from, interface, authority: cap });
        let generation = Generation(ep.generation);
        self.outputs.push(Output::Event(SecurityEvent::EndpointBound {
            endpoint: name,
            generation,
            provider: from,
        }));
        Ok(ResponseBody::Bound { generation })
    }

    fn rebind(&mut self, from: ProcessId, handle: HandleId, now: u64) -> Result<ResponseBody, Error> {
        let cap = self.handle_of(from, handle)?.cap;
        let name = self.caps.validate(cap, now)?.endpoint.clone();
        if !self.owns_invocation(&name, cap) {
            return Err(err(
                ErrorCode::PermissionDenied,
                "handle is not derived from this endpoint's invocation authority",
            ));
        }
        let generation = self
            .endpoints
            .get(&name)
            .filter(|ep| ep.provider.is_some())
            .map(|ep| Generation(ep.generation))
            .ok_or_else(|| err(ErrorCode::Unavailable, "endpoint has no provider"))?;
        if let Some(h) = self.processes.get_mut(&from).and_then(|p| p.handles.get_mut(&handle)) {
            h.generation = generation;
        }
        Ok(ResponseBody::Bound { generation })
    }

    /// Only a direct child may be killed. A process that is not the sender's
    /// child and a process that does not exist are indistinguishable, so this
    /// cannot be used to probe for other processes.
    fn kill(&mut self, from: ProcessId, child: ProcessId, now: u64) -> Result<ResponseBody, Error> {
        let is_child = self.processes.get(&child).is_some_and(|p| p.parent == Some(from));
        if !is_child {
            return Err(err(ErrorCode::PermissionDenied, "not a child of this process"));
        }
        self.teardown(child, now);
        Ok(ResponseBody::Done)
    }

    /// Lists the sender's own subtree, breadth-first, to `depth` levels. A
    /// process may always see what it owns, so no capability is required; the
    /// listing never leaves the subtree, so it reveals nothing about siblings,
    /// ancestors, or unrelated processes.
    fn children(&self, from: ProcessId, depth: u32) -> Result<ResponseBody, Error> {
        let depth = depth.clamp(1, nucleus_protocol::MAX_CHILDREN_DEPTH);
        let mut out = Vec::new();
        let mut frontier: Vec<ProcessId> =
            self.processes.get(&from).map(|p| p.children.iter().copied().collect()).unwrap_or_default();
        for _ in 0..depth {
            let mut next = Vec::new();
            for process in frontier {
                let Some(p) = self.processes.get(&process) else { continue };
                let provides = self
                    .endpoints
                    .iter()
                    .filter(|(_, ep)| ep.provider.as_ref().is_some_and(|pr| pr.process == process))
                    .map(|(name, ep)| (name.clone(), Generation(ep.generation)))
                    .collect();
                out.push(ChildInfo {
                    process,
                    program: p.program.clone(),
                    provides,
                    handles: p.handles.len(),
                    children: p.children.len(),
                });
                next.extend(p.children.iter().copied());
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok(ResponseBody::Children(out))
    }

    /// Lists the caller's callable tools: for each of its handles, the actions
    /// whose live provider declared them tool-callable, with their schemas and
    /// the handle's resource scope. Reveals nothing the caller cannot already
    /// call, so it needs no capability of its own.
    fn tools(&self, from: ProcessId, now: u64) -> Result<ResponseBody, Error> {
        let mut out = Vec::new();
        for handle in self.describe_handles(from, now) {
            let held = self.handle_of(from, handle.handle)?;
            if !self.owns_invocation(&handle.endpoint, held.cap) {
                continue;
            }
            // A stale handle cannot call the current provider; offer none of its tools.
            let Some(provider) = self
                .endpoints
                .get(&handle.endpoint)
                .filter(|ep| ep.generation == held.generation.0)
                .and_then(|ep| ep.provider.as_ref())
            else {
                continue;
            };
            for action in &handle.actions {
                if let Some(decl) = provider.interface.actions.get(action)
                    && let Some(schema) = &decl.tool
                {
                    out.push(nucleus_protocol::ToolInfo {
                        endpoint: handle.endpoint.clone(),
                        action: action.clone(),
                        resource: handle.resource.clone(),
                        effect: decl.effect,
                        description: schema.description.clone(),
                        params: schema.params.clone(),
                        resource_from: schema.resource_from.clone(),
                        endpoint_description: provider.interface.description.clone(),
                    });
                }
            }
        }
        Ok(ResponseBody::Tools(out))
    }

    /// Closing a handle gives up its capability, which also withdraws
    /// everything that was delegated from it.
    fn close(&mut self, from: ProcessId, handle: HandleId, now: u64) -> Result<ResponseBody, Error> {
        let removed = self
            .processes
            .get_mut(&from)
            .and_then(|p| p.handles.remove(&handle))
            .ok_or_else(|| err(ErrorCode::UnknownHandle, "no such handle"))?;
        self.caps.remove(removed.cap);
        self.sweep_queues(now);
        Ok(ResponseBody::Done)
    }
}
