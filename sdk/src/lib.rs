//! Client side of the Nucleus protocol for a process started by the supervisor.
//!
//! The channel is the process's stdin and stdout. Everything here goes through
//! the same messages any other client would send; there is no privileged path.
//! The connection is synchronous: while a request waits for its response,
//! other kernel messages are either handled by an installed service handler
//! or kept in a bounded backlog for [`Connection::next_event`]. A handler can
//! make nested calls; responses remain correlated with their request IDs. A
//! dispatch that waits in the backlog is handed over with the time its caller
//! has left then, not the time it had when the kernel sent it.
//! Frames are read by one reader thread per
//! connection and handed over one at a time (a rendezvous, so the thread
//! holds at most one frame ahead of the process), which is what lets
//! [`Connection::next_event_within`] give up after a deadline — a provider
//! with work of its own to time (a round of workers, a housekeeping tick)
//! needs that. Nothing else about the channel's order or semantics depends
//! on the thread, and a process that stops taking events stops reading its
//! channel, so the kernel's backpressure applies to a slow consumer.
//!
//! A process that serves several windows from one channel (the UI service)
//! can also [`Connection::submit`] a call without waiting and collect
//! its outcome with [`Connection::next_answer`], which a [`Waker`] held by
//! another thread interrupts when that thread has work for the channel owner.
//! A submitted call that is no longer wanted can be [`Connection::cancel`]led
//! on its own; its outcome still arrives through `next_answer`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::marker::PhantomData;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use nucleus_protocol::{
    ActionName, ChildInfo, ChildProgram, CreatedEndpoint, DispatchId, EndpointName, Error, ErrorCode,
    FrameError, FromKernel, Generation, Grant, HandleId, HandleInfo, IdentityRequest, InterfaceDecl, Payload,
    ProcessId, ProgramName, RequestId, ResourceFrom, ResourcePath, ResponseBody, ToKernel, ToolInfo,
    read_frame, write_frame,
};
use serde::Serialize;

pub mod tool_environments;
pub use tool_environments::{ToolBinding, ToolCatalog, validate_bindings};

/// The action [`Connection::announce_ready`] calls.
pub const ACTION_READY: &str = "ready";

/// Kernel messages that may arrive while waiting for a response. The kernel
/// bounds in-flight dispatches per endpoint, so a well-configured system never
/// reaches this; exceeding it is treated as a broken channel.
const MAX_BACKLOG: usize = 1024;

/// What the reader thread and wakers hand the channel owner.
enum Inbound {
    Frame(Result<FromKernel, ChannelError>),
    Wake,
}

/// Interrupts [`Connection::next_answer`] from another thread. `wake` blocks
/// until the channel owner takes the wake-up, so none is lost; it returns
/// false once the connection is gone.
#[derive(Clone)]
pub struct Waker(SyncSender<Inbound>);

impl Waker {
    pub fn wake(&self) -> bool {
        self.0.send(Inbound::Wake).is_ok()
    }
}

/// Failure of the channel itself, as opposed to a request being refused.
#[derive(Debug)]
pub enum ChannelError {
    Frame(FrameError),
    /// The kernel sent something the protocol does not allow at this point.
    Unexpected(&'static str),
    /// The reader thread is gone: the channel already failed (that failure
    /// was returned once) or the process is shutting down.
    Closed,
    /// The kernel withdrew this process's authority. Awaited outcomes may be
    /// unknown; only local cleanup and supervisor-owned teardown remain.
    Terminated,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Unexpected(what) => write!(f, "protocol violation by kernel: {what}"),
            Self::Closed => f.write_str("the kernel channel is closed"),
            Self::Terminated => f.write_str("the kernel terminated this process"),
        }
    }
}

impl std::error::Error for ChannelError {}

impl From<FrameError> for ChannelError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

/// A request either fails in transport or is answered, possibly with an error.
pub type Reply<T> = Result<Result<T, Error>, ChannelError>;

/// The outcome of a call started with [`Connection::submit`].
pub type Answer = (RequestId, Result<Payload, Error>);

/// A call delivered to this process as the provider of an endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatch {
    pub id: DispatchId,
    /// Which of this process's endpoints was called.
    pub endpoint: EndpointName,
    /// Asserted by the kernel from the caller's channel.
    pub caller: ProcessId,
    pub action: ActionName,
    pub resource: ResourcePath,
    pub payload: Payload,
    /// The caller's time left when the dispatch is handed to the process:
    /// the kernel's figure, less the whole milliseconds the dispatch waited
    /// in the backlog since this connection read it (while the process was
    /// inside a call of its own, say).
    pub remaining_ms: u64,
}

/// A kernel message kept for the handler or [`Connection::next_event`]. A
/// dispatch keeps when it was read and the kernel's `remaining_ms`, so
/// whoever takes it later sees the time left then.
struct Held {
    event: Event,
    read: Option<(Instant, u64)>,
}

impl Held {
    fn new(event: Event) -> Self {
        let read = match &event {
            Event::Dispatch(d) => Some((Instant::now(), d.remaining_ms)),
            _ => None,
        };
        Self { event, read }
    }

    /// The event, a dispatch's `remaining_ms` brought up to now.
    fn current(&mut self) -> &Event {
        if let (Event::Dispatch(d), Some((read, sent_ms))) = (&mut self.event, self.read) {
            let waited = u64::try_from(read.elapsed().as_millis()).unwrap_or(u64::MAX);
            d.remaining_ms = sent_ms.saturating_sub(waited);
        }
        &self.event
    }

    fn take(mut self) -> Event {
        self.current();
        self.event
    }
}

/// Unsolicited kernel messages.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Dispatch(Dispatch),
    CancelDispatch(DispatchId),
    ChildExited(ProcessId),
    ChildReaped {
        child: ProcessId,
        success: bool,
        cleanup_confirmed: bool,
    },
    /// Release resources and exit; the supervisor kills the process otherwise.
    Terminate,
}

type EventHandler<R, W> = Box<dyn FnMut(&mut Connection<R, W>, &Event) -> Result<bool, ChannelError>>;

/// An open channel to the kernel.
pub struct Connection<R: Read, W: Write> {
    /// Frames from the reader thread, in channel order; an error is the last
    /// thing it sends.
    inbound: Receiver<Inbound>,
    /// Kept to hand out [`Waker`]s, so the receiver never disconnects;
    /// `failed` records that the reader's last frame was delivered.
    wake: SyncSender<Inbound>,
    failed: bool,
    woken: bool,
    reader: PhantomData<fn() -> R>,
    writer: W,
    process: ProcessId,
    handles: Vec<HandleInfo>,
    next_request: u64,
    backlog: VecDeque<Held>,
    active: BTreeSet<RequestId>,
    /// Calls started with `submit` whose outcome `next_answer` has not
    /// returned; the bool records whether `cancel` sent Cancel for it.
    submitted: BTreeMap<RequestId, bool>,
    replies: BTreeMap<RequestId, Result<ResponseBody, Error>>,
    handler: Option<EventHandler<R, W>>,
    /// Set by `Terminate` or a successful [`Connection::request_exit`], and
    /// never cleared: a waiting request then returns
    /// [`ChannelError::Terminated`] at once instead of waiting for a reply the
    /// kernel may never send, and later requests and provider replies fail here
    /// without writing to the channel. An event handler may still clean up
    /// locally.
    terminating: bool,
}

/// Reads frames until the channel fails or the connection is dropped. The
/// hand-over is a rendezvous: when the process is not taking frames, this
/// blocks holding one instead of draining the channel into memory.
fn read_frames<R: Read>(mut reader: BufReader<R>, queue: &SyncSender<Inbound>) {
    loop {
        let frame = read_frame::<_, FromKernel>(&mut reader).map_err(ChannelError::from);
        let last = frame.is_err();
        if queue.send(Inbound::Frame(frame)).is_err() || last {
            return;
        }
    }
}

impl Connection<io::Stdin, io::Stdout> {
    /// Connects over the channel the supervisor attached to stdin and stdout.
    /// Nothing else may be printed to stdout afterwards.
    pub fn from_stdio() -> Result<Self, ChannelError> {
        Self::open(io::stdin(), io::stdout())
    }
}

impl<R: Read, W: Write> Connection<R, W> {
    /// Starts the reader thread and waits for the kernel's `Hello`.
    pub fn open(reader: R, writer: W) -> Result<Self, ChannelError>
    where
        R: Send + 'static,
    {
        let (queue, inbound) = mpsc::sync_channel(0);
        let reader = BufReader::new(reader);
        let wake = queue.clone();
        std::thread::Builder::new()
            .name("kernel-channel".to_owned())
            .spawn(move || read_frames(reader, &queue))
            .map_err(|e| ChannelError::Frame(FrameError::Io(e)))?;
        let mut connection = Self {
            inbound,
            wake,
            failed: false,
            woken: false,
            reader: PhantomData,
            writer,
            process: ProcessId(0),
            handles: Vec::new(),
            next_request: 1,
            backlog: VecDeque::new(),
            active: BTreeSet::new(),
            submitted: BTreeMap::new(),
            replies: BTreeMap::new(),
            handler: None,
            terminating: false,
        };
        match connection.receive()? {
            FromKernel::Hello { process, handles } => {
                connection.process = process;
                connection.handles = handles;
                Ok(connection)
            }
            _ => Err(ChannelError::Unexpected("first message was not hello")),
        }
    }

    /// The next frame, blocking. A wake-up is remembered for `next_answer`.
    fn receive(&mut self) -> Result<FromKernel, ChannelError> {
        loop {
            if let Some(frame) = self.take(None)? {
                return Ok(frame);
            }
        }
    }

    /// One item from the reader or a waker, within `timeout` if given.
    /// `Ok(None)` is a wake-up or the timeout.
    fn take(&mut self, timeout: Option<Duration>) -> Result<Option<FromKernel>, ChannelError> {
        if self.failed {
            return Err(ChannelError::Closed);
        }
        let item = match timeout {
            None => self.inbound.recv().map_err(|_| ChannelError::Closed)?,
            Some(timeout) => match self.inbound.recv_timeout(timeout) {
                Ok(item) => item,
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => return Err(ChannelError::Closed),
            },
        };
        match item {
            Inbound::Frame(Ok(frame)) => Ok(Some(frame)),
            Inbound::Frame(Err(error)) => {
                self.failed = true;
                Err(error)
            }
            Inbound::Wake => {
                self.woken = true;
                Ok(None)
            }
        }
    }

    /// A handle another thread uses to interrupt [`Connection::next_answer`].
    pub fn waker(&self) -> Waker {
        Waker(self.wake.clone())
    }

    /// This process's identity as the kernel knows it. Informational.
    pub fn process(&self) -> ProcessId {
        self.process
    }

    /// Handles received at startup or through endpoint creation, minus closed
    /// handles. This description is a snapshot; the kernel checks live authority.
    pub fn handles(&self) -> &[HandleInfo] {
        &self.handles
    }

    /// First cached handle for `endpoint` that allows `action`.
    pub fn find_handle(&self, endpoint: &str, action: &str) -> Option<HandleId> {
        self.handles
            .iter()
            .find(|h| h.endpoint.as_str() == endpoint && h.actions.iter().any(|a| a.as_str() == action))
            .map(|h| h.handle)
    }

    /// Selects a held handle covering the actual resource. Multiple narrowed
    /// handles for one action must not accidentally use the first one's scope.
    pub fn find_handle_for(&self, endpoint: &str, action: &str, resource: &ResourcePath) -> Option<HandleId> {
        self.handles
            .iter()
            .find(|h| {
                h.endpoint.as_str() == endpoint
                    && h.actions.iter().any(|a| a.as_str() == action)
                    && resource.is_within(&h.resource)
            })
            .map(|h| h.handle)
    }

    /// Handles service events while ordinary calls wait. The handler may make
    /// nested calls; it is temporarily disabled during those calls, so it is
    /// never reentered. Unhandled events remain in the bounded backlog.
    pub fn set_event_handler(
        &mut self,
        handler: impl FnMut(&mut Self, &Event) -> Result<bool, ChannelError> + 'static,
    ) {
        self.handler = Some(Box::new(handler));
    }

    fn handle_event(&mut self, event: &Event) -> Result<bool, ChannelError> {
        let Some(mut handler) = self.handler.take() else { return Ok(false) };
        let result = handler(self, event);
        self.handler = Some(handler);
        result
    }

    fn handle_backlog(&mut self) -> Result<(), ChannelError> {
        if self.handler.is_none() {
            return Ok(());
        }
        for _ in 0..self.backlog.len() {
            let Some(mut held) = self.backlog.pop_front() else { break };
            if !self.handle_event(held.current())? {
                self.backlog.push_back(held);
            }
        }
        Ok(())
    }

    fn request(&mut self, build: impl FnOnce(RequestId) -> ToKernel) -> Reply<ResponseBody> {
        if self.terminating {
            return Err(ChannelError::Terminated);
        }
        if self.active.len() >= MAX_BACKLOG {
            return Err(ChannelError::Unexpected("too many nested requests"));
        }
        let request = RequestId(self.next_request);
        self.next_request += 1;
        let message = build(request);
        self.active.insert(request);
        let result = (|| {
            write_frame(&mut self.writer, &message)?;
            self.wait_response(request)
        })();
        self.active.remove(&request);
        result
    }

    fn wait_response(&mut self, request: RequestId) -> Reply<ResponseBody> {
        loop {
            self.handle_backlog()?;
            if self.terminating {
                return Err(ChannelError::Terminated);
            }
            if let Some(result) = self.replies.remove(&request) {
                return Ok(result);
            }
            match self.receive()? {
                FromKernel::Response { request: answered, result } if answered == request => {
                    return Ok(result);
                }
                other => self.stash(other)?,
            }
        }
    }

    /// Keeps the outcome of a request other than the one being awaited.
    fn record_response(
        &mut self,
        answered: RequestId,
        result: Result<ResponseBody, Error>,
    ) -> Result<(), ChannelError> {
        if !self.active.contains(&answered) || self.replies.insert(answered, result).is_some() {
            return Err(ChannelError::Unexpected("unsolicited or duplicate response"));
        }
        Ok(())
    }

    fn stash(&mut self, msg: FromKernel) -> Result<(), ChannelError> {
        if self.backlog.len() >= MAX_BACKLOG {
            return Err(ChannelError::Unexpected("backlog overflow"));
        }
        let event = match msg {
            FromKernel::Dispatch { dispatch, endpoint, caller, action, resource, payload, remaining_ms } => {
                Event::Dispatch(Dispatch {
                    id: dispatch,
                    endpoint,
                    caller,
                    action,
                    resource,
                    payload,
                    remaining_ms,
                })
            }
            FromKernel::CancelDispatch { dispatch } => Event::CancelDispatch(dispatch),
            FromKernel::ChildExited { child } => Event::ChildExited(child),
            FromKernel::ChildReaped { child, success, cleanup_confirmed } => {
                Event::ChildReaped { child, success, cleanup_confirmed }
            }
            FromKernel::Terminate => {
                self.terminating = true;
                Event::Terminate
            }
            FromKernel::Hello { .. } => return Err(ChannelError::Unexpected("second hello")),
            FromKernel::Response { request, result } => return self.record_response(request, result),
        };
        let mut held = Held::new(event);
        if !self.handle_event(held.current())? {
            self.backlog.push_back(held);
        }
        Ok(())
    }

    /// True once the supervisor has asked this process to exit. A loop that
    /// spends its time in calls should check this between them.
    pub fn terminate_requested(&self) -> bool {
        self.terminating
    }

    /// Blocks until the kernel sends something that is not a response.
    pub fn next_event(&mut self) -> Result<Event, ChannelError> {
        loop {
            if let Some(held) = self.backlog.pop_front() {
                return Ok(held.take());
            }
            let msg = self.receive()?;
            self.stash(msg)?;
        }
    }

    /// Like [`Connection::next_event`], but gives up after `timeout` with
    /// `Ok(None)`. Nothing is lost by the wait: a frame that arrives later is
    /// returned by the next call. For a provider that has its own deadlines
    /// to keep while it waits for the kernel.
    pub fn next_event_within(&mut self, timeout: Duration) -> Result<Option<Event>, ChannelError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(held) = self.backlog.pop_front() {
                return Ok(Some(held.take()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.take(Some(remaining))? {
                Some(msg) => self.stash(msg)?,
                None if Instant::now() >= deadline => return Ok(None),
                None => {}
            }
        }
    }

    /// Invokes `action` on `resource` through `handle` and waits for the outcome.
    /// Termination interrupts the wait with `ChannelError::Terminated`. That
    /// does not establish whether dispatched effects happened; never retry an
    /// uncertain side effect from this error. Further requests/replies fail locally.
    pub fn call<T: Serialize>(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &T,
        deadline_ms: Option<u64>,
    ) -> Reply<Payload> {
        let (action, payload) = match (ActionName::new(action), Payload::encode(payload)) {
            (Ok(action), Ok(payload)) => (action, payload),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
        };
        let resource = resource.clone();
        let result = self.request(|request| ToKernel::Call {
            request,
            handle,
            action,
            resource,
            payload,
            deadline_ms,
        })?;
        Ok(result.and_then(Self::payload))
    }

    fn payload(body: ResponseBody) -> Result<Payload, Error> {
        match body {
            ResponseBody::Payload(payload) => Ok(payload),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered a call without a payload")),
        }
    }

    /// Sends a call like [`Connection::call`] but returns its request ID
    /// without waiting. [`Connection::next_answer`] returns the outcome once;
    /// meanwhile other requests may be made. [`Connection::cancel`] cancels
    /// one.
    pub fn submit<T: Serialize>(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &T,
        deadline_ms: Option<u64>,
    ) -> Reply<RequestId> {
        let (action, payload) = match (ActionName::new(action), Payload::encode(payload)) {
            (Ok(action), Ok(payload)) => (action, payload),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
        };
        if self.terminating {
            return Err(ChannelError::Terminated);
        }
        if self.active.len() >= MAX_BACKLOG {
            return Err(ChannelError::Unexpected("too many outstanding requests"));
        }
        let request = RequestId(self.next_request);
        self.next_request += 1;
        let resource = resource.clone();
        write_frame(
            &mut self.writer,
            &ToKernel::Call { request, handle, action, resource, payload, deadline_ms },
        )?;
        self.active.insert(request);
        self.submitted.insert(request, false);
        Ok(Ok(request))
    }

    /// Asks the kernel to end every process of `project`, through a
    /// `stop_project` handle, and returns its request ID without waiting.
    /// [`Connection::next_answer`] returns the outcome once, a payload holding
    /// [`nucleus_protocol::ProjectStopped`], when nothing of the project runs
    /// any more. It cannot be cancelled.
    pub fn submit_project_stop(&mut self, authority: HandleId, project: &str) -> Reply<RequestId> {
        if self.terminating {
            return Err(ChannelError::Terminated);
        }
        if self.active.len() >= MAX_BACKLOG {
            return Err(ChannelError::Unexpected("too many outstanding requests"));
        }
        let request = RequestId(self.next_request);
        self.next_request += 1;
        write_frame(
            &mut self.writer,
            &ToKernel::StopProject { request, authority, project: project.to_owned() },
        )?;
        self.active.insert(request);
        // Marked as if a cancel had been sent: `cancel` sends none for it.
        self.submitted.insert(request, true);
        Ok(Ok(request))
    }

    /// Asks the kernel to cancel one call started with [`Connection::submit`]
    /// whose outcome has not arrived yet, sending at most one `Cancel` for it;
    /// for any other request it does nothing. It neither waits nor invents an
    /// outcome: [`Connection::next_answer`] still returns the call's own
    /// response once, which may be `Cancelled` or whatever won the race.
    pub fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
        if self.terminating {
            return Err(ChannelError::Terminated);
        }
        if self.replies.contains_key(&request) {
            return Ok(());
        }
        if let Some(sent) = self.submitted.get_mut(&request)
            && !*sent
        {
            write_frame(&mut self.writer, &ToKernel::Cancel { request })?;
            *sent = true;
        }
        Ok(())
    }

    /// Blocks until a submitted call is answered and returns its outcome, or
    /// until a [`Waker`] wakes this connection (`Ok(None)`). Other kernel
    /// messages go to the event handler or the backlog, as while a call waits.
    pub fn next_answer(&mut self) -> Result<Option<Answer>, ChannelError> {
        loop {
            self.handle_backlog()?;
            if self.terminating {
                return Err(ChannelError::Terminated);
            }
            if let Some(request) = self.submitted.keys().copied().find(|r| self.replies.contains_key(r))
                && let Some(result) = self.replies.remove(&request)
            {
                self.submitted.remove(&request);
                self.active.remove(&request);
                return Ok(Some((request, result.and_then(Self::payload))));
            }
            if std::mem::take(&mut self.woken) {
                return Ok(None);
            }
            if let Some(msg) = self.take(None)? {
                self.stash(msg)?;
            }
        }
    }

    /// Starts `program` as a child holding exactly `grants`.
    pub fn spawn(
        &mut self,
        authority: HandleId,
        program: ProgramName,
        grants: Vec<Grant>,
    ) -> Reply<ProcessId> {
        let result = self.request(|request| ToKernel::Spawn { request, authority, program, grants })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::Spawned { child } => Ok(child),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered spawn unexpectedly")),
        }))
    }

    /// Starts a child from an executable in this process's own namespace,
    /// holding exactly `grants` (a subset of this process's authority) and a
    /// namespace that is a subset of this process's own. The kernel refuses a
    /// wider executable, bind, network, or grant before any process is created.
    pub fn spawn_executable(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
    ) -> Reply<ProcessId> {
        self.spawn_executable_as(authority, program, grants, None)
    }

    /// [`Self::spawn_executable`], the child running as `identity` when given:
    /// a project this process, running as the runtime's own, holds
    /// `spawn_as` for.
    pub fn spawn_executable_as(
        &mut self,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        identity: Option<IdentityRequest>,
    ) -> Reply<ProcessId> {
        let result = self.request(|request| ToKernel::SpawnExecutable {
            request,
            authority,
            program,
            grants,
            identity,
        })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::Spawned { child } => Ok(child),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered spawn unexpectedly")),
        }))
    }

    /// Becomes the provider of `interface.endpoint`.
    pub fn provide(&mut self, authority: HandleId, interface: InterfaceDecl) -> Reply<Generation> {
        let result = self.request(|request| ToKernel::Provide { request, authority, interface })?;
        Ok(result.and_then(Self::bound))
    }

    /// Allocates a fresh owned endpoint under a held, delegable factory
    /// capability. Both returned handles remain in its expiry and shared
    /// budget chain. Close both handles when the object is no longer
    /// needed; closing either original handle releases the endpoint.
    pub fn create_endpoint(
        &mut self,
        authority: HandleId,
        actions: BTreeSet<ActionName>,
    ) -> Reply<CreatedEndpoint> {
        let result = self.request(|request| ToKernel::CreateEndpoint { request, authority, actions })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::EndpointCreated(created) => {
                self.handles.extend([created.provide.clone(), created.invoke.clone()]);
                Ok(*created)
            }
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered endpoint creation unexpectedly")),
        }))
    }

    /// Binds a stale handle to the endpoint's current provider.
    pub fn rebind(&mut self, handle: HandleId) -> Reply<Generation> {
        let result = self.request(|request| ToKernel::Rebind { request, handle })?;
        Ok(result.and_then(Self::bound))
    }

    fn bound(body: ResponseBody) -> Result<Generation, Error> {
        match body {
            ResponseBody::Bound { generation } => Ok(generation),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered without a generation")),
        }
    }

    /// Terminates a direct child of this process and everything it owns.
    pub fn kill(&mut self, child: ProcessId) -> Reply<()> {
        let result = self.request(|request| ToKernel::Kill { request, child })?;
        Ok(result.map(|_| ()))
    }

    /// Lists this process's own subtree, `depth` levels deep (1 = direct
    /// children). Never returns anything the process does not own.
    pub fn children(&mut self, depth: u32) -> Reply<Vec<ChildInfo>> {
        let result = self.request(|request| ToKernel::Children { request, depth })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::Children(list) => Ok(list),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered children unexpectedly")),
        }))
    }

    /// Lists this process's callable tools: the actions it holds a handle for
    /// whose live provider declared them tool-callable. See [`resolve_tool_call`]
    /// to route a model's call back.
    pub fn tools(&mut self) -> Reply<Vec<ToolInfo>> {
        let result = self.request(|request| ToKernel::Tools { request })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::Tools(tools) => Ok(tools),
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered tools unexpectedly")),
        }))
    }

    /// Gives up `handle` and everything delegated from it.
    pub fn close(&mut self, handle: HandleId) -> Reply<()> {
        let result = self.request(|request| ToKernel::Close { request, handle })?;
        Ok(result.map(|_| self.handles.retain(|held| held.handle != handle)))
    }

    /// Cooperatively finish this process through the kernel's ownership tree.
    /// Withdraws this subtree's authority immediately and waits until every
    /// descendant's physical cleanup is confirmed. Success permits only local
    /// cleanup and returning from the process; subsequent requests/replies are
    /// rejected locally. It does not prove this process's eventual exit status.
    /// A missing descendant receipt keeps this request pending; channel loss is
    /// an error, never evidence that cleanup completed.
    pub fn request_exit(&mut self) -> Reply<()> {
        let result = self.request(|request| ToKernel::Exit { request })?;
        Ok(result.and_then(|body| match body {
            ResponseBody::Done => {
                self.terminating = true;
                Ok(())
            }
            _ => Err(Error::new(ErrorCode::ServiceError, "kernel answered exit unexpectedly")),
        }))
    }

    /// Tells the process that started this one, when it asked to be told,
    /// that this one now serves: calls [`ACTION_READY`] on the handle that
    /// holds it, if this process was given one, and does nothing otherwise.
    /// A service the projects router starts on demand calls it once it
    /// provides its endpoint, so the router forwards nothing before there is a
    /// provider.
    pub fn announce_ready(&mut self) -> Reply<()> {
        let Some(info) =
            self.handles.iter().find(|h| h.actions.iter().any(|action| action.as_str() == ACTION_READY))
        else {
            return Ok(Ok(()));
        };
        let (handle, resource) = (info.handle, info.resource.clone());
        Ok(self.call(handle, ACTION_READY, &resource, &serde_json::json!({}), None)?.map(|_| ()))
    }

    /// Reports the outcome of a dispatch.
    pub fn reply(
        &mut self,
        dispatch: DispatchId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        if self.terminating {
            return Err(ChannelError::Terminated);
        }
        Ok(write_frame(&mut self.writer, &ToKernel::Reply { dispatch, result })?)
    }
}

/// The tool name a model sees for one callable action: `"endpoint_action"`.
pub fn tool_name(endpoint: &EndpointName, action: &ActionName) -> String {
    format!("{}_{}", endpoint.as_str(), action.as_str())
}

/// Where a resolved tool call should go.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub endpoint: EndpointName,
    pub action: ActionName,
    pub resource: ResourcePath,
    pub payload: Payload,
}

/// Resolves a model's tool call (`name` and its JSON `args`) into the kernel
/// call to make, applying the action's `resource_from`. Returns `None` when
/// `name` is not one of `tools`. The caller finds the handle for the returned
/// endpoint/action and dispatches it — no per-plugin code needed.
pub fn resolve_tool_call(
    tools: &[ToolInfo],
    name: &str,
    args: &serde_json::Value,
) -> Option<Result<ToolCall, Error>> {
    let tool = tools.iter().find(|t| tool_name(&t.endpoint, &t.action) == name)?;
    Some(build_call(tool, args))
}

fn build_call(tool: &ToolInfo, args: &serde_json::Value) -> Result<ToolCall, Error> {
    let invalid = |why: &str| Error::new(ErrorCode::InvalidInput, why.to_owned());
    let mut object = match args {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::Null => serde_json::Map::new(),
        _ => return Err(invalid("tool arguments must be a JSON object")),
    };
    let string = |object: &serde_json::Map<String, serde_json::Value>, param: &str| {
        object.get(param).and_then(|v| v.as_str()).map(str::to_owned)
    };
    let resource = match &tool.resource_from {
        // The handle's own scope; the whole args object is the payload.
        ResourceFrom::Handle => tool.resource.clone(),
        // The named param IS the address: it becomes the resource and leaves the payload.
        ResourceFrom::Segments { param } => {
            let value = string(&object, param).ok_or_else(|| invalid("missing path argument"))?;
            object.remove(param);
            ResourcePath::new(value.split('/').filter(|s| !s.is_empty() && *s != "."))?
        }
        // The named param stays in the payload; its host also scopes the call.
        ResourceFrom::Host { param } => {
            let value = string(&object, param).ok_or_else(|| invalid("missing url argument"))?;
            ResourcePath::new([host_of(&value)?])?
        }
    };
    let payload = Payload::encode(&serde_json::Value::Object(object))?;
    Ok(ToolCall { endpoint: tool.endpoint.clone(), action: tool.action.clone(), resource, payload })
}

/// The host of an `http`/`https` URL, lower-cased and without a port. Kept here
/// so the tool layer can form a `Host` resource without depending on any
/// particular service's crate.
fn host_of(url: &str) -> Result<String, Error> {
    let invalid = |why: &str| Error::new(ErrorCode::InvalidInput, format!("url: {why}"));
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| invalid("must be an http or https URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid("userinfo in the authority is not allowed"));
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return Err(invalid("no host"));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use nucleus_protocol::EffectClass;
    use serde_json::json;

    fn tool(endpoint: &str, action: &str, resource: &[&str], resource_from: ResourceFrom) -> ToolInfo {
        ToolInfo {
            endpoint: EndpointName::new(endpoint).unwrap(),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            effect: EffectClass::Observation,
            description: "d".to_owned(),
            params: json!({"type": "object"}),
            resource_from,
            endpoint_description: String::new(),
        }
    }

    /// A kernel side that writes frames whenever the test says so; the
    /// reader thread blocks on the pipe in between, as it does on stdin.
    fn kernel_pipe() -> (Connection<io::PipeReader, Vec<u8>>, io::PipeWriter) {
        let (reader, mut writer) = io::pipe().unwrap();
        write_frame(&mut writer, &FromKernel::Hello { process: ProcessId(7), handles: vec![] }).unwrap();
        (Connection::open(reader, Vec::new()).unwrap(), writer)
    }

    #[test]
    fn created_endpoint_handles_are_cached_and_successful_close_removes_them() {
        let (mut kernel, mut writer) = kernel_pipe();
        let endpoint = EndpointName::new("dynamic.1").unwrap();
        let created = CreatedEndpoint {
            endpoint: endpoint.clone(),
            provide: HandleInfo {
                handle: HandleId(7),
                endpoint: EndpointName::new("kernel").unwrap(),
                actions: BTreeSet::from([ActionName::new("provide").unwrap()]),
                resource: ResourcePath::new(["endpoint", endpoint.as_str()]).unwrap(),
                delegable: true,
                expires_in_ms: Some(50),
                max_calls: None,
            },
            invoke: HandleInfo {
                handle: HandleId(8),
                endpoint: endpoint.clone(),
                actions: BTreeSet::from([ActionName::new("run").unwrap()]),
                resource: ResourcePath::root(),
                delegable: true,
                expires_in_ms: Some(50),
                max_calls: None,
            },
        };
        write_frame(
            &mut writer,
            &FromKernel::Response {
                request: RequestId(1),
                result: Ok(ResponseBody::EndpointCreated(Box::new(created.clone()))),
            },
        )
        .unwrap();
        let wanted = created.invoke.actions.clone();
        assert_eq!(kernel.create_endpoint(HandleId(3), wanted.clone()).unwrap().unwrap(), created);
        assert_eq!(kernel.handles(), &[created.provide.clone(), created.invoke.clone()]);
        assert_eq!(
            kernel.find_handle_for(endpoint.as_str(), "run", &ResourcePath::root()),
            Some(created.invoke.handle)
        );
        let sent: ToKernel = read_frame(&mut io::Cursor::new(&kernel.writer)).unwrap();
        assert_eq!(
            sent,
            ToKernel::CreateEndpoint { request: RequestId(1), authority: HandleId(3), actions: wanted }
        );
        write_frame(
            &mut writer,
            &FromKernel::Response {
                request: RequestId(2),
                result: Err(Error::new(ErrorCode::ServiceError, "failed")),
            },
        )
        .unwrap();
        assert!(kernel.close(created.invoke.handle).unwrap().is_err());
        assert_eq!(kernel.handles().len(), 2, "a failed close cannot change the local view");
        for (request, handle) in [(3, created.invoke.handle), (4, created.provide.handle)] {
            write_frame(
                &mut writer,
                &FromKernel::Response { request: RequestId(request), result: Ok(ResponseBody::Done) },
            )
            .unwrap();
            kernel.close(handle).unwrap().unwrap();
            assert!(!kernel.handles().iter().any(|held| held.handle == handle));
        }
        assert!(kernel.handles().is_empty());
    }

    #[test]
    fn failed_endpoint_creation_never_adds_cached_handles() {
        let (mut kernel, mut writer) = kernel_pipe();
        for (request, result) in
            [(1, Err(Error::new(ErrorCode::PermissionDenied, "no factory"))), (2, Ok(ResponseBody::Done))]
        {
            write_frame(&mut writer, &FromKernel::Response { request: RequestId(request), result }).unwrap();
            assert!(
                kernel
                    .create_endpoint(HandleId(3), BTreeSet::from([ActionName::new("run").unwrap()]))
                    .unwrap()
                    .is_err()
            );
            assert!(kernel.handles().is_empty());
        }
    }

    #[test]
    fn child_cleanup_evidence_is_preserved_independently_of_success() {
        let (mut kernel, mut writer) = kernel_pipe();
        for (success, cleanup_confirmed) in [(false, true), (true, true), (false, false)] {
            write_frame(
                &mut writer,
                &FromKernel::ChildReaped { child: ProcessId(9), success, cleanup_confirmed },
            )
            .unwrap();
            assert_eq!(
                kernel.next_event().unwrap(),
                Event::ChildReaped { child: ProcessId(9), success, cleanup_confirmed }
            );
        }
    }

    #[test]
    fn waiting_for_an_event_gives_up_at_the_deadline_and_loses_nothing() {
        let (mut kernel, mut writer) = kernel_pipe();
        assert_eq!(kernel.process(), ProcessId(7));
        let started = std::time::Instant::now();
        assert!(matches!(kernel.next_event_within(Duration::from_millis(50)), Ok(None)), "nothing arrived");
        assert!(started.elapsed() >= Duration::from_millis(50));
        write_frame(&mut writer, &FromKernel::ChildExited { child: ProcessId(9) }).unwrap();
        write_frame(&mut writer, &FromKernel::Terminate).unwrap();
        assert_eq!(
            kernel.next_event_within(Duration::from_secs(5)).unwrap(),
            Some(Event::ChildExited(ProcessId(9)))
        );
        assert!(kernel.terminate_requested() || kernel.next_event().unwrap() == Event::Terminate);
        // The kernel going away is the channel closing, once, then `Closed`.
        drop(writer);
        assert!(matches!(kernel.next_event(), Err(ChannelError::Frame(FrameError::Closed))));
        assert!(matches!(kernel.next_event_within(Duration::from_millis(10)), Err(ChannelError::Closed)));
    }

    #[test]
    fn serving_during_a_call_preserves_out_of_order_nested_responses_and_events() {
        let (mut kernel, mut writer) = kernel_pipe();
        kernel.set_event_handler(|kernel, event| {
            if matches!(event, Event::ChildExited(_)) {
                assert!(kernel.tools()?.is_ok());
                return Ok(true);
            }
            Ok(false)
        });
        write_frame(&mut writer, &FromKernel::ChildExited { child: ProcessId(8) }).unwrap();
        // The outer call returns while the handler is waiting on its nested call.
        write_frame(
            &mut writer,
            &FromKernel::Response { request: RequestId(1), result: Ok(ResponseBody::Tools(vec![])) },
        )
        .unwrap();
        write_frame(
            &mut writer,
            &FromKernel::Response { request: RequestId(2), result: Ok(ResponseBody::Tools(vec![])) },
        )
        .unwrap();
        assert!(kernel.tools().unwrap().is_ok());
        write_frame(&mut writer, &FromKernel::Terminate).unwrap();
        assert_eq!(kernel.next_event().unwrap(), Event::Terminate);
        assert!(kernel.terminate_requested());
    }

    /// A dispatch that arrives while the process waits on a call of its own
    /// is kept in the backlog; whoever takes it later is told the time its
    /// caller has left then, not the figure the kernel sent.
    #[test]
    fn a_dispatch_that_waited_behind_a_call_is_handed_over_with_the_time_left() {
        let (mut kernel, mut writer) = kernel_pipe();
        let dispatch = FromKernel::Dispatch {
            dispatch: DispatchId(1),
            endpoint: EndpointName::new("discovery").unwrap(),
            caller: ProcessId(9),
            action: ActionName::new("start").unwrap(),
            resource: ResourcePath::root(),
            payload: Payload::encode(&json!({})).unwrap(),
            remaining_ms: 10_000,
        };
        write_frame(&mut writer, &dispatch).unwrap();
        let answer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            let tools =
                FromKernel::Response { request: RequestId(1), result: Ok(ResponseBody::Tools(vec![])) };
            write_frame(&mut writer, &tools).unwrap();
            writer
        });
        assert!(kernel.tools().unwrap().is_ok());
        let _writer = answer.join().unwrap();
        let Event::Dispatch(waited) = kernel.next_event().unwrap() else { panic!("the dispatch") };
        assert!((5_000..=9_700).contains(&waited.remaining_ms), "{}", waited.remaining_ms);
    }

    #[test]
    fn submitted_calls_are_answered_once_across_ordinary_calls_and_wakes() {
        let (mut kernel, mut writer) = kernel_pipe();
        let answer = |request, text: &str| FromKernel::Response {
            request: RequestId(request),
            result: Ok(ResponseBody::Payload(Payload::encode(&json!(text)).unwrap())),
        };
        let submitted = kernel.submit(HandleId(1), "wait", &ResourcePath::root(), &json!({}), None);
        assert_eq!(submitted.unwrap().unwrap(), RequestId(1));
        // An ordinary call made meanwhile keeps the submitted outcome for later.
        write_frame(&mut writer, &answer(1, "first")).unwrap();
        write_frame(&mut writer, &answer(2, "ordinary")).unwrap();
        assert!(kernel.call(HandleId(1), "run", &ResourcePath::root(), &json!({}), None).unwrap().is_ok());
        let (request, result) = kernel.next_answer().unwrap().unwrap();
        assert_eq!(
            (request, result.unwrap().decode::<String>().unwrap()),
            (RequestId(1), "first".to_owned())
        );
        // Another thread interrupts the wait for an answer; nothing is lost.
        kernel.submit(HandleId(1), "wait", &ResourcePath::root(), &json!({}), None).unwrap().unwrap();
        let waker = kernel.waker();
        let woke = std::thread::spawn(move || waker.wake());
        assert!(kernel.next_answer().unwrap().is_none());
        assert!(woke.join().unwrap());
        write_frame(&mut writer, &answer(3, "second")).unwrap();
        assert_eq!(kernel.next_answer().unwrap().unwrap().0, RequestId(3));
        assert!(kernel.active.is_empty() && kernel.submitted.is_empty() && kernel.replies.is_empty());
    }

    #[test]
    fn cancelling_one_submitted_call_sends_one_cancel_and_its_answer_still_arrives_once() {
        let (mut kernel, mut writer) = kernel_pipe();
        let submit = |kernel: &mut Connection<_, _>| {
            kernel.submit(HandleId(1), "wait", &ResourcePath::root(), &json!({}), None).unwrap().unwrap()
        };
        let (first, second) = (submit(&mut kernel), submit(&mut kernel));
        kernel.cancel(first).unwrap();
        kernel.cancel(first).unwrap();
        kernel.cancel(RequestId(99)).unwrap();
        write_frame(
            &mut writer,
            &FromKernel::Response {
                request: second,
                result: Ok(ResponseBody::Payload(Payload::encode(&json!("second")).unwrap())),
            },
        )
        .unwrap();
        write_frame(
            &mut writer,
            &FromKernel::Response {
                request: first,
                result: Err(Error::new(ErrorCode::Cancelled, "cancelled after dispatch")),
            },
        )
        .unwrap();
        let (answered, result) = kernel.next_answer().unwrap().unwrap();
        assert_eq!((answered, result.unwrap().decode::<String>().unwrap()), (second, "second".to_owned()));
        // An outcome that has already arrived is not cancelled.
        kernel.cancel(second).unwrap();
        let (answered, result) = kernel.next_answer().unwrap().unwrap();
        assert_eq!((answered, result.unwrap_err().code), (first, ErrorCode::Cancelled));
        let cancels: Vec<_> = sent_frames(&kernel.writer)
            .into_iter()
            .filter(|f| matches!(f, ToKernel::Cancel { .. }))
            .collect();
        assert_eq!(cancels, [ToKernel::Cancel { request: first }]);
        assert!(kernel.active.is_empty() && kernel.submitted.is_empty() && kernel.replies.is_empty());
    }

    fn sent_frames(bytes: &[u8]) -> Vec<ToKernel> {
        let mut reader = io::Cursor::new(bytes);
        let mut frames = Vec::new();
        while reader.position() < bytes.len() as u64 {
            frames.push(read_frame(&mut reader).unwrap());
        }
        frames
    }

    #[test]
    fn termination_interrupts_an_unanswered_call_and_permanently_stops_writes() {
        let (mut kernel, mut writer) = kernel_pipe();
        write_frame(&mut writer, &FromKernel::Terminate).unwrap();
        // Leave the kernel pipe open and provide no response. This must return
        // on Terminate, not on EOF or an eventual supervisor kill.
        assert!(matches!(kernel.tools(), Err(ChannelError::Terminated)));
        assert!(kernel.terminate_requested());
        assert_eq!(kernel.next_event().unwrap(), Event::Terminate);
        let bytes = kernel.writer.len();
        assert!(matches!(kernel.tools(), Err(ChannelError::Terminated)));
        assert!(matches!(
            kernel.reply(DispatchId(1), Payload::encode(&json!({}))),
            Err(ChannelError::Terminated)
        ));
        assert_eq!(kernel.writer.len(), bytes, "no IPC with withdrawn authority");
        assert!(kernel.terminate_requested(), "consuming the event cannot restore authority");
    }

    #[test]
    fn cooperative_exit_waits_for_the_kernel_and_permanently_stops_writes() {
        let (mut kernel, mut writer) = kernel_pipe();
        write_frame(&mut writer, &FromKernel::ChildExited { child: ProcessId(9) }).unwrap();
        write_frame(
            &mut writer,
            &FromKernel::Response { request: RequestId(1), result: Ok(ResponseBody::Done) },
        )
        .unwrap();
        kernel.request_exit().unwrap().unwrap();
        let sent: ToKernel = read_frame(&mut io::Cursor::new(&kernel.writer)).unwrap();
        assert_eq!(sent, ToKernel::Exit { request: RequestId(1) });
        assert!(kernel.terminate_requested());
        assert_eq!(kernel.next_event().unwrap(), Event::ChildExited(ProcessId(9)));
        let bytes = kernel.writer.len();
        assert!(matches!(kernel.request_exit(), Err(ChannelError::Terminated)));
        assert!(matches!(kernel.tools(), Err(ChannelError::Terminated)));
        assert!(matches!(
            kernel.reply(DispatchId(1), Payload::encode(&json!({}))),
            Err(ChannelError::Terminated)
        ));
        assert_eq!(kernel.writer.len(), bytes);
    }

    #[test]
    fn cooperative_exit_does_not_treat_channel_loss_as_cleanup_confirmation() {
        let (mut kernel, writer) = kernel_pipe();
        drop(writer);
        assert!(matches!(kernel.request_exit(), Err(ChannelError::Frame(FrameError::Closed))));
    }

    #[test]
    fn segments_resource_takes_the_path_out_of_the_payload() {
        let tools = [tool("fs", "read", &["work"], ResourceFrom::Segments { param: "path".to_owned() })];
        let call = resolve_tool_call(&tools, "fs_read", &json!({"path": "work/notes.txt"})).unwrap().unwrap();
        assert_eq!(call.resource.segments(), ["work", "notes.txt"]);
        assert_eq!(call.payload, Payload::encode(&json!({})).unwrap(), "the path is not left in the payload");
        // `..` is refused by ResourcePath, never resolved.
        assert!(resolve_tool_call(&tools, "fs_read", &json!({"path": "../secret"})).unwrap().is_err());
    }

    #[test]
    fn host_resource_keeps_the_url_in_the_payload() {
        let tools = [tool("web", "fetch", &[], ResourceFrom::Host { param: "url".to_owned() })];
        let call = resolve_tool_call(&tools, "web_fetch", &json!({"url": "https://Example.com/x"}))
            .unwrap()
            .unwrap();
        assert_eq!(call.resource.segments(), ["example.com"]);
        assert_eq!(call.payload, Payload::encode(&json!({"url": "https://Example.com/x"})).unwrap());
    }

    #[test]
    fn handle_resource_uses_the_handles_scope_and_an_unknown_name_is_none() {
        let tools = [tool("sessions", "create", &["default"], ResourceFrom::Handle)];
        let call = resolve_tool_call(&tools, "sessions_create", &json!({})).unwrap().unwrap();
        assert_eq!(call.resource.segments(), ["default"]);
        assert!(resolve_tool_call(&tools, "nope_nope", &json!({})).is_none());
    }
}
