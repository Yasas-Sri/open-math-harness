//! One kernel subscription per session, fanned out to the windows watching
//! it.
//!
//! A window learns that a session has something new with
//! `conversation.wait`. Were every window's wait a call of its own, which
//! the conversation service holds while it waits, the windows one process
//! could serve would be the waits that service holds
//! ([`conversation::MAX_WAITS`]). Instead the windows waiting on one
//! `[project, session]` share one call: the first makes it, the others join
//! it, and its answer goes to each of them as its own answer, by its own
//! request's `id`. What is shared is only whether the session has a record
//! after the one the window last saw; each window reads the
//! records themselves with its own `history` call, which its label admits or
//! refuses as any other.
//!
//! A window joins a watch only with a wait its label admitted
//! ([`crate::access::Access::admit`]), on the resource that admission left,
//! so the key `[project, session]` is a project the window may read.
//!
//! The call a watch makes asks for a record after the last one the watch
//! knows (any, at first, so a new watch learns at once where the session
//! is); a window that has seen less than that is answered at once from what
//! the watch knows, one that has seen more waits for the next answer. When
//! the service's wait ends with nothing new (`changed: false`) every window
//! waiting is told so and asks again. A watch nobody waits on any more is
//! dropped once its call comes back; a call is never cancelled or repeated
//! for a window that left.

use std::collections::{BTreeMap, VecDeque};

use nucleus_conversation_interface::{self as conversation, WaitRequest, WaitResponse};
use nucleus_protocol::{Error, ResourcePath};
use serde_json::json;

use crate::serve::{Answers, respond};

/// What a watch watches: a session (`[project, session]`, as admission left
/// it).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub resource: Vec<String>,
}

impl Key {
    /// The resource of the watch's call.
    pub fn resource(&self) -> Result<ResourcePath, Error> {
        ResourcePath::new(self.resource.iter().map(String::as_str))
    }
}

/// A window's wait on a watch: its request's `id`, what it has seen, and
/// where its answer goes.
struct Waiter {
    id: u64,
    request: WaitRequest,
    answer: Answers,
}

struct Watch {
    /// The session as the last answer of the watch's call left it.
    known: Option<WaitResponse>,
    waiters: Vec<Waiter>,
}

/// Every session some window waits on.
#[derive(Default)]
pub(crate) struct Watches {
    watches: BTreeMap<Key, Watch>,
    /// Watches whose call waits for a free place, in arrival order.
    queue: VecDeque<Key>,
}

/// Whether `response` has a record after what `request` has seen; `None` is
/// before every record.
fn satisfied(request: &WaitRequest, response: &WaitResponse) -> bool {
    response.last_seq > request.after_seq
}

/// The answer a waiter gets: the watch's, with `changed` as it is for that
/// waiter.
fn answer_for(waiter: &Waiter, response: &WaitResponse) -> serde_json::Value {
    let mine = WaitResponse { changed: satisfied(&waiter.request, response), last_seq: response.last_seq };
    respond(Some(waiter.id), Ok(json!(mine)))
}

impl Watches {
    /// A window's wait, admitted on `resource`: answered at once when the
    /// watch already knows of something after what the window has seen,
    /// otherwise joined to the watch, which is queued to make its call.
    pub fn join(&mut self, resource: &ResourcePath, request: WaitRequest, id: u64, answer: Answers) {
        let key = Key { resource: resource.segments().to_vec() };
        let waiter = Waiter { id, request, answer };
        match self.watches.get_mut(&key) {
            Some(watch) => {
                if let Some(known) = &watch.known
                    && satisfied(&waiter.request, known)
                {
                    drop(waiter.answer.send(answer_for(&waiter, known)));
                    return;
                }
                watch.waiters.push(waiter);
            }
            None => {
                self.watches.insert(key.clone(), Watch { known: None, waiters: vec![waiter] });
                self.queue.push_back(key);
            }
        }
    }

    /// The next watch whose call may start, and the call: a record after the
    /// last one the watch knows. A watch whose windows all left is dropped
    /// instead.
    pub fn next(&mut self) -> Option<(Key, WaitRequest)> {
        while let Some(key) = self.queue.pop_front() {
            let Some(watch) = self.watches.get_mut(&key) else { continue };
            watch.waiters.retain(|waiter| !waiter.answer.is_closed());
            if watch.waiters.is_empty() {
                self.watches.remove(&key);
                continue;
            }
            let request = WaitRequest { after_seq: watch.known.and_then(|known| known.last_seq) };
            return Some((key, request));
        }
        None
    }

    /// Whether a watch waits for a place for its call.
    pub fn queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The answer of a watch's call: each window it satisfies, or every one
    /// when the service's wait ended with nothing new, is answered; an
    /// error goes to every window. A watch with windows left makes its next
    /// call; one without is dropped.
    pub fn answered(&mut self, key: &Key, result: Result<WaitResponse, Error>) {
        let Some(watch) = self.watches.get_mut(key) else { return };
        match result {
            Ok(response) => {
                watch.known = Some(response);
                watch.waiters.retain(|waiter| {
                    if waiter.answer.is_closed() {
                        return false;
                    }
                    if response.changed && !satisfied(&waiter.request, &response) {
                        return true;
                    }
                    drop(waiter.answer.send(answer_for(waiter, &response)));
                    false
                });
                if watch.waiters.is_empty() {
                    self.watches.remove(key);
                } else {
                    self.queue.push_back(key.clone());
                }
            }
            Err(error) => {
                if let Some(watch) = self.watches.remove(key) {
                    for waiter in watch.waiters {
                        drop(waiter.answer.send(respond(Some(waiter.id), Err(error.clone()))));
                    }
                }
            }
        }
    }
}

/// The resource and request of a window's admitted `conversation.wait`, or
/// `None` for any other call.
pub(crate) fn wait_of(route: &crate::Route) -> Option<(ResourcePath, WaitRequest)> {
    let (endpoint, action, resource, payload) = route;
    if *endpoint != conversation::ENDPOINT || *action != conversation::ACTION_WAIT {
        return None;
    }
    let request = serde_json::from_value(payload.clone()).ok()?;
    Some((resource.clone(), request))
}
