//! The kernel side of the service: one thread owns the kernel channel.
//!
//! Windows hand it their requests ([`Inbox`]); it decides each against the
//! window's label ([`crate::plan`]), answers a refusal and `whoami` at once,
//! joins a wait to its session's watch
//! ([`crate::fanout`]) and submits every other call without waiting for it,
//! answering when the service replies. A slow service (a discovery run
//! checking a proof in Lean answers nothing for minutes) delays only its own
//! requests, never a message being sent or another window.
//!
//! Each kind of call has its own room ([`Room`]): the watches' waits, the
//! mirror's change feed, the suggestions, the long git work, the steps of
//! projects' deletions ([`crate::deletion`]) and the other calls; a call
//! beyond its room waits here, in arrival order, for a place. Together they
//! are [`MAX_PENDING`], within the kernel's per-caller limit. Nothing here
//! grows with the number of windows: a window costs a task and a channel on
//! the connection side, and its requests in flight (at most
//! [`crate::window::MAX_IN_FLIGHT`]).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, mpsc};

use nucleus_conversation_interface::{self as conversation, WaitResponse};
use nucleus_protocol::{Error, ErrorCode, RequestId};
use nucleus_sdk::{Answer, ChannelError};
use serde_json::{Value, json};

use crate::deletion::Deletions;
use crate::fanout::{Key, Watches, wait_of};
use crate::{Kernel, Operation, Peer, Planned, Room, Route, deadline_ms, plan};

/// Where a window's answer lines go: its connection's writer.
pub(crate) type Answers = tokio::sync::mpsc::UnboundedSender<Value>;

/// One request of a window: its `id`, the operation, who sent it and where
/// its answer goes.
pub(crate) struct Job {
    pub id: u64,
    pub operation: Operation,
    pub peer: Arc<Peer>,
    pub answer: Answers,
}

/// Where windows hand their requests to the kernel's thread, which `wake`
/// interrupts from waiting for the kernel.
#[derive(Clone)]
pub(crate) struct Inbox {
    jobs: mpsc::Sender<Job>,
    wake: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl Inbox {
    pub fn new(jobs: mpsc::Sender<Job>, wake: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self { jobs, wake }
    }

    /// For the kernel's own connection: a thread of its own takes the
    /// wake-ups, which block until the kernel's thread takes them, so that no
    /// window's task ever blocks; wake-ups that come while one is pending
    /// are one.
    pub fn for_kernel(jobs: mpsc::Sender<Job>, waker: nucleus_sdk::Waker) -> Self {
        let (nudge, nudges) = mpsc::sync_channel::<()>(1);
        std::thread::spawn(move || {
            for () in nudges {
                if !waker.wake() {
                    return;
                }
            }
        });
        let wake = move || !matches!(nudge.try_send(()), Err(mpsc::TrySendError::Disconnected(())));
        Self::new(jobs, Arc::new(wake))
    }

    /// Hands a job over; false once the kernel's side is gone.
    pub fn send(&self, job: Job) -> bool {
        self.jobs.send(job).is_ok() && (self.wake)()
    }
}

/// The `id` of a request line that did not parse as a known operation.
pub(crate) fn request_id(line: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(line).ok()?.get("id")?.as_u64()
}

pub(crate) fn respond(id: Option<u64>, result: Result<Value, Error>) -> Value {
    match result {
        Ok(value) => json!({"id": id, "ok": value}),
        Err(error) => json!({"id": id, "error": error}),
    }
}

/// `conversation.wait` calls outstanding at once: one for each session a
/// window watches, which the conversation service holds all of. A watch
/// beyond them waits for a place; windows are not counted.
pub(crate) const MAX_WAITS: usize = conversation::MAX_WAITS;
/// `conversation.changes` calls outstanding at once: the mirror's one, that
/// of a mirror link replacing it while the old one's is still held, and a
/// held wait per window that follows the feed: the conversation service's
/// `MAX_CHANGE_WAITS`; a caller beyond them waits for a place.
pub const MAX_CHANGES_CALLS: usize = conversation::MAX_CHANGE_WAITS;
/// `suggestions.next` calls outstanding at once, across windows: a slow
/// suggestion never holds up a message being sent. Two, so that a newer
/// request for a session reaches the service, which then drops the older one.
pub(crate) const MAX_SUGGESTIONS: usize = 2;
/// Other calls outstanding at once, across windows.
pub(crate) const MAX_CALLS: usize = 7;
/// Project creations outstanding at once, which may take up to the git
/// timeout: they never take the room of the calls a window makes all the time.
pub(crate) const MAX_GIT: usize = 2;
/// Projects' deletions with a step outstanding at once: each step (a
/// service forgetting a project, its purge) may take minutes. One more waits
/// for a place.
pub(crate) const MAX_DELETIONS: usize = 2;
/// The calls this process may have outstanding, every room full: the
/// composition's per-caller limit must allow it.
pub const MAX_PENDING: usize =
    MAX_WAITS + MAX_CHANGES_CALLS + MAX_SUGGESTIONS + MAX_CALLS + MAX_GIT + MAX_DELETIONS;

impl Room {
    fn limit(self) -> usize {
        match self {
            Self::Wait => MAX_WAITS,
            Self::Changes => MAX_CHANGES_CALLS,
            Self::Suggestion => MAX_SUGGESTIONS,
            Self::Call => MAX_CALLS,
            Self::Git => MAX_GIT,
            Self::Deletion => MAX_DELETIONS,
        }
    }
}

/// The kernel as [`serve_jobs`] uses it.
pub(crate) trait Calls {
    /// `whoami`, for `peer`.
    fn whoami(&self, peer: &Peer) -> Value;
    /// Submits the call `route` stands for, without waiting for its answer.
    fn submit(
        &mut self,
        route: &Route,
        deadline_ms: Option<u64>,
    ) -> Result<Result<RequestId, Error>, ChannelError>;
    fn next_answer(&mut self) -> Result<Option<Answer>, ChannelError>;
    fn terminate_requested(&self) -> bool;
}

impl Calls for Kernel {
    fn whoami(&self, peer: &Peer) -> Value {
        peer.whoami(self.process(), self.handles())
    }
    fn submit(
        &mut self,
        route: &Route,
        deadline_ms: Option<u64>,
    ) -> Result<Result<RequestId, Error>, ChannelError> {
        let (endpoint, action, resource, payload) = route;
        let handle = match crate::handle_for(self, endpoint, action) {
            Ok(handle) => handle,
            Err(e) => return Ok(Err(e)),
        };
        nucleus_sdk::Connection::submit(self, handle, action, resource, payload, deadline_ms)
    }
    fn next_answer(&mut self) -> Result<Option<Answer>, ChannelError> {
        nucleus_sdk::Connection::next_answer(self)
    }
    fn terminate_requested(&self) -> bool {
        nucleus_sdk::Connection::terminate_requested(self)
    }
}

/// A call that waits for a place in its room.
struct Queued {
    id: u64,
    route: Route,
    deadline: Option<u64>,
    room: Room,
    answer: Answers,
}

/// What a call with the kernel answers.
enum Outstanding {
    Call {
        id: u64,
        answer: Answers,
        room: Room,
    },
    Watch(Key),
    /// A step of the deletion of this project.
    Deletion(String),
}

/// The calls with the kernel and the rooms they take.
#[derive(Default)]
struct Rooms {
    pending: BTreeMap<RequestId, Outstanding>,
    taken: BTreeMap<RoomIndex, usize>,
}

/// A room as an ordered key.
type RoomIndex = u8;

fn index(room: Room) -> RoomIndex {
    room as RoomIndex
}

impl Rooms {
    fn free(&self, room: Room) -> bool {
        self.taken.get(&index(room)).copied().unwrap_or(0) < room.limit()
    }
    fn take(&mut self, request: RequestId, room: Room, outstanding: Outstanding) {
        *self.taken.entry(index(room)).or_default() += 1;
        self.pending.insert(request, outstanding);
    }
    fn release(&mut self, request: RequestId) -> Option<Outstanding> {
        let outstanding = self.pending.remove(&request)?;
        let room = match &outstanding {
            Outstanding::Call { room, .. } => *room,
            Outstanding::Watch(_) => Room::Wait,
            Outstanding::Deletion(_) => Room::Deletion,
        };
        if let Some(taken) = self.taken.get_mut(&index(room)) {
            *taken = taken.saturating_sub(1);
        }
        Some(outstanding)
    }
}

/// Decides one job: answered here (a refusal or `whoami`), joined to its
/// session's watch or to a project's deletion, or queued for its room.
fn admit(
    kernel: &impl Calls,
    job: Job,
    queued: &mut VecDeque<Queued>,
    watches: &mut Watches,
    deletions: &mut Deletions,
) {
    let Job { id, operation, peer, answer } = job;
    let (deadline, room) = (deadline_ms(&operation), Room::of(&operation));
    let at_once = match plan(operation, &peer.access) {
        Err(e) => Err(e),
        Ok(Planned::WhoAmI) => Ok(kernel.whoami(&peer)),
        Ok(Planned::Archive(project)) => {
            deletions.archive(project, id, answer);
            return;
        }
        Ok(Planned::Delete(project)) => {
            deletions.ask(project, id, answer);
            return;
        }
        Ok(Planned::Call(route)) => {
            match wait_of(&route) {
                Some((resource, request)) => watches.join(&resource, request, id, answer),
                None => queued.push_back(Queued { id, route, deadline, room, answer }),
            }
            return;
        }
    };
    drop(answer.send(respond(Some(id), at_once)));
}

/// Serves every window's requests as they arrive, within the room of their
/// kind, and answers each when its service replies; deletes projects with
/// `forget` on the services of `forgets` ([`crate::deletion`]). Returns when
/// the kernel terminates this process; a lost channel ends it: fail and
/// restart clean.
pub(crate) fn serve_jobs(
    kernel: &mut impl Calls,
    jobs: &mpsc::Receiver<Job>,
    forgets: Vec<&'static str>,
) -> Result<(), ChannelError> {
    let mut rooms = Rooms::default();
    let mut queued: VecDeque<Queued> = VecDeque::new();
    let mut watches = Watches::default();
    let mut deletions = Deletions::new(forgets);
    loop {
        for job in jobs.try_iter() {
            admit(kernel, job, &mut queued, &mut watches, &mut deletions);
        }
        let mut blocked = VecDeque::new();
        while let Some(call) = queued.pop_front() {
            if !rooms.free(call.room) {
                blocked.push_back(call);
                continue;
            }
            match kernel.submit(&call.route, call.deadline)? {
                Ok(request) => {
                    let outstanding = Outstanding::Call { id: call.id, answer: call.answer, room: call.room };
                    rooms.take(request, call.room, outstanding);
                }
                Err(e) => drop(call.answer.send(respond(Some(call.id), Err(e)))),
            }
        }
        queued = blocked;
        while rooms.free(Room::Deletion) {
            let Some((project, call, deadline)) = deletions.next() else { break };
            let submitted = match call {
                Ok(route) => kernel.submit(&route, Some(deadline))?,
                Err(e) => Err(e),
            };
            match submitted {
                Ok(request) => rooms.take(request, Room::Deletion, Outstanding::Deletion(project)),
                Err(e) => deletions.answered(&project, Err(e)),
            }
        }
        while rooms.free(Room::Wait) && watches.queued() {
            let Some((key, request)) = watches.next() else { break };
            let route: Result<Route, Error> = key.resource().map(|resource| {
                (conversation::ENDPOINT, conversation::ACTION_WAIT, resource, json!(request))
            });
            let submitted = match route {
                Ok(route) => kernel.submit(&route, Some(crate::WAIT_DEADLINE_MS))?,
                Err(e) => Err(e),
            };
            match submitted {
                Ok(request) => rooms.take(request, Room::Wait, Outstanding::Watch(key)),
                Err(e) => watches.answered(&key, Err(e)),
            }
        }
        if kernel.terminate_requested() {
            return Ok(());
        }
        match kernel.next_answer() {
            Ok(Some((request, result))) => match rooms.release(request) {
                Some(Outstanding::Call { id, answer, .. }) => {
                    drop(answer.send(respond(Some(id), result.and_then(|p| p.decode::<Value>()))));
                }
                Some(Outstanding::Watch(key)) => {
                    watches.answered(&key, result.and_then(|p| p.decode::<WaitResponse>()));
                }
                Some(Outstanding::Deletion(project)) => {
                    deletions.answered(&project, result.and_then(|p| p.decode::<Value>()));
                }
                None => {}
            },
            Ok(None) => {} // a window has sent a request
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// A refusal of a request line that is not one, by its `id` when it has one.
pub(crate) fn invalid(line: &[u8], why: impl std::fmt::Display) -> Value {
    respond(request_id(line), Err(Error::new(ErrorCode::InvalidInput, why.to_string())))
}

#[cfg(test)]
mod tests;
