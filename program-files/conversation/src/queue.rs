//! The queue (interface module doc, "The queue"): the user messages said and
//! not yet received by the agent, `queue`, `move`, `withdraw` and `send_now`
//! on them, and the bounds within which `say` adds one.
//!
//! A holder of `move`, `withdraw` or `send_now` on a session acts on any
//! person's waiting message: in the VM any member of the team may act on any
//! member's. The record names who did it; nothing prevents it. `send_now`
//! changes only when a message is delivered, never its author, text or
//! files.
//!
//! It is rebuilt from the log in log order: a `user` record joins it at the
//! end; a `delivered` marker (a `receive` took the message) and a
//! `message_withdrawn` record take one out; a `message_moved` record puts it
//! in the order the record holds. A call is checked against the queue, its
//! record appended, and only then the queue changed, so what a restart
//! rebuilds is what was answered. The service handles one dispatch at a
//! time: a queue mutation and the `receive` that takes the queue never
//! interleave, and the one that comes second finds the message taken.

use std::collections::{BTreeSet, VecDeque};

use nucleus_conversation_interface::{
    ACTION_MOVE, ACTION_QUEUE, ACTION_SEND_NOW, ALREADY_TAKEN, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES,
    MoveRequest, PostResponse, QueueRequest, QueueResponse, Queued, RECORD_MESSAGE_MOVED,
    RECORD_MESSAGE_SENT_NOW, RECORD_MESSAGE_WITHDRAWN, UserMessage, WithdrawRequest,
};
use nucleus_protocol::{Error, ErrorCode, Payload, ResourcePath};
use serde_json::{Value, json};

use crate::ConversationService;

/// A waiting message: when it was said, the message, its size as JSON.
#[derive(Debug)]
struct Waiting {
    at_ms: u64,
    message: UserMessage,
    bytes: usize,
    send_now: bool,
}

/// A session's queue, and every message id said in the session.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    waiting: VecDeque<Waiting>,
    /// Every `say` id ever used, so a retried `say` cannot duplicate a message.
    said: BTreeSet<String>,
    /// The ids withdrawn from the queue.
    withdrawn: BTreeSet<String>,
}

fn broken(why: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::ServiceError, format!("user record: {why}"))
}

/// A user record's optional list `field` (`attachments`, `files`, `images`).
fn listed<T: serde::de::DeserializeOwned>(record: &Value, field: &str) -> Result<Vec<T>, Error> {
    record
        .get(field)
        .map(|list| serde_json::from_value(list.clone()))
        .transpose()
        .map_err(broken)
        .map(Option::unwrap_or_default)
}

/// The message a `user` record holds.
fn user_message(record: &Value) -> Result<UserMessage, Error> {
    Ok(UserMessage {
        id: record["id"].as_str().unwrap_or_default().to_owned(),
        text: record["text"].as_str().unwrap_or_default().to_owned(),
        attachments: listed(record, "attachments")?,
        files: listed(record, "files")?,
        images: listed(record, "images")?,
        author: record["author"].as_str().map(str::to_owned),
        settings: record
            .get("settings")
            .map(|s| serde_json::from_value(s.clone()))
            .transpose()
            .map_err(broken)?,
    })
}

fn size(message: &UserMessage) -> usize {
    serde_json::to_vec(message).map_or(usize::MAX, |json| json.len())
}

impl Queue {
    /// The queue and the ids a session's log leaves (module doc).
    pub(crate) fn from_log(log: &[Value]) -> Result<Self, Error> {
        let mut queue = Self::default();
        for record in log {
            let id = record["id"].as_str().unwrap_or_default();
            match record["kind"].as_str() {
                Some("user") => {
                    let message = user_message(record)?;
                    queue.said.insert(message.id.clone());
                    let (at_ms, bytes) = (record["at_ms"].as_u64().unwrap_or(0), size(&message));
                    queue.waiting.push_back(Waiting { at_ms, message, bytes, send_now: false });
                }
                Some("delivered") => queue.remove(id),
                Some(RECORD_MESSAGE_WITHDRAWN) => {
                    queue.remove(id);
                    queue.withdrawn.insert(id.to_owned());
                }
                Some(RECORD_MESSAGE_MOVED) => {
                    let order: Vec<String> =
                        serde_json::from_value(record["order"].clone()).map_err(broken)?;
                    queue.reorder(&order);
                }
                Some(RECORD_MESSAGE_SENT_NOW) => {
                    if let Some(at) = queue.index(id) {
                        queue.waiting[at].send_now = true;
                    }
                }
                _ => {}
            }
        }
        Ok(queue)
    }

    /// Whether a message with this id was ever said in the session.
    pub(crate) fn said(&self, id: &str) -> bool {
        self.said.contains(id)
    }

    /// The size of `message`, if it may join the queue: at most
    /// [`MAX_QUEUED_MESSAGES`] and [`MAX_QUEUED_BYTES`] wait at once.
    pub(crate) fn admit(&self, message: &UserMessage) -> Result<usize, Error> {
        let full = |why: String| Err(Error::new(ErrorCode::ServiceError, why));
        if self.waiting.len() >= MAX_QUEUED_MESSAGES {
            return full(format!(
                "{MAX_QUEUED_MESSAGES} messages wait for the agent already; withdraw one or wait for its turn to end"
            ));
        }
        let bytes = size(message);
        if self.waiting.iter().map(|w| w.bytes).sum::<usize>().saturating_add(bytes) > MAX_QUEUED_BYTES {
            return full(format!(
                "the messages waiting for the agent would hold more than {} MiB; withdraw one or wait for its \
                 turn to end",
                MAX_QUEUED_BYTES >> 20
            ));
        }
        Ok(bytes)
    }

    /// Adds a message [`Self::admit`] admitted at the end.
    pub(crate) fn push(&mut self, at_ms: u64, message: UserMessage, bytes: usize) {
        self.said.insert(message.id.clone());
        self.waiting.push_back(Waiting { at_ms, message, bytes, send_now: false });
    }

    /// The first waiting message and when it was said.
    pub(crate) fn front(&self) -> Option<(u64, &UserMessage)> {
        self.front_now().or_else(|| self.waiting.front().map(|w| (w.at_ms, &w.message)))
    }

    pub(crate) fn front_now(&self) -> Option<(u64, &UserMessage)> {
        self.waiting.iter().find(|w| w.send_now).map(|w| (w.at_ms, &w.message))
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.waiting.retain(|w| w.message.id != id);
    }

    /// Puts the waiting messages in `order`; any it does not name follow,
    /// as they were.
    fn reorder(&mut self, order: &[String]) {
        let mut rest = std::mem::take(&mut self.waiting);
        for id in order {
            if let Some(found) = rest.iter().position(|w| w.message.id == *id).and_then(|at| rest.remove(at))
            {
                self.waiting.push_back(found);
            }
        }
        self.waiting.extend(rest);
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.waiting.iter().position(|w| w.message.id == id)
    }

    /// Why `id` names no waiting message.
    fn not_waiting(&self, id: &str) -> Error {
        let why = if self.withdrawn.contains(id) {
            format!("message {id:?} was withdrawn already")
        } else if self.said.contains(id) {
            format!("{ALREADY_TAKEN} the agent has received message {id:?}")
        } else {
            format!("no message {id:?} was said in this session")
        };
        Error::new(ErrorCode::InvalidInput, why)
    }

    /// The order of the queue once `id` is moved before `before`, or to the end.
    fn moved(&self, id: &str, before: Option<&str>) -> Result<Vec<String>, Error> {
        let from = self.index(id).ok_or_else(|| self.not_waiting(id))?;
        let mut order: Vec<String> = self.waiting.iter().map(|w| w.message.id.clone()).collect();
        let moving = order.remove(from);
        let to = match before {
            None => order.len(),
            Some(before) => {
                order.iter().position(|other| other == before).ok_or_else(|| self.not_waiting(before))?
            }
        };
        order.insert(to, moving);
        Ok(order)
    }

    fn listed(&self) -> Vec<Queued> {
        self.waiting
            .iter()
            .filter(|w| w.send_now)
            .chain(self.waiting.iter().filter(|w| !w.send_now))
            .map(|w| Queued { id: w.message.id.clone(), send_now: w.send_now })
            .collect()
    }
}

impl ConversationService {
    /// Queue operations on the session `resource` names. An
    /// observation, or a call on a message that is not there, never creates
    /// the session.
    pub(crate) fn serve_queue(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        now_ms: u64,
    ) -> Result<Payload, Error> {
        if action == ACTION_QUEUE {
            payload.decode::<QueueRequest>()?;
            let messages = self.existing(resource)?.map(|session| session.queue.listed()).unwrap_or_default();
            return Payload::encode(&QueueResponse { messages });
        }
        let (id, author, before) = if action == ACTION_MOVE {
            let request: MoveRequest = payload.decode()?;
            request.validate()?;
            (request.id, request.author, Some(request.before))
        } else {
            let request: WithdrawRequest = payload.decode()?;
            request.validate()?;
            (request.id, request.author, None)
        };
        let Some(session) = self.existing(resource)? else {
            return Err(Queue::default().not_waiting(&id));
        };
        let at = session.queue.index(&id).ok_or_else(|| session.queue.not_waiting(&id))?;
        if action == ACTION_SEND_NOW {
            let mut record = json!({"kind": RECORD_MESSAGE_SENT_NOW, "id": id});
            if let Some(author) = author {
                record["author"] = json!(author);
            }
            session.append_at(record, now_ms)?;
            session.queue.waiting[at].send_now = true;
            return Payload::encode(&PostResponse {});
        }
        let (mut record, order) = match before {
            Some(before) => {
                let order = session.queue.moved(&id, before.as_deref())?;
                (
                    json!({"kind": RECORD_MESSAGE_MOVED, "id": id, "before": before, "order": order}),
                    Some(order),
                )
            }
            None => {
                let mut record = json!({"kind": RECORD_MESSAGE_WITHDRAWN, "id": id});
                if let Some(said_by) = session.queue.waiting.get(at).and_then(|w| w.message.author.as_ref()) {
                    record["message_author"] = json!(said_by);
                }
                (record, None)
            }
        };
        if let Some(author) = author {
            record["author"] = json!(author);
        }
        session.append_at(record, now_ms)?;
        match order {
            Some(order) => session.queue.reorder(&order),
            None => {
                session.queue.remove(&id);
                session.queue.withdrawn.insert(id);
            }
        }
        Payload::encode(&PostResponse {})
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use nucleus_conversation_interface::{
        ACTION_RECEIVE, ACTION_SAY, ACTION_WITHDRAW, MAX_TEXT_BYTES, ReceiveResponse,
    };
    use nucleus_protocol::{DispatchId, ProcessId};

    use super::*;
    use crate::tests::Bench;

    fn queued(bench: &mut Bench) -> Vec<String> {
        let (_, replies) = bench.call(ACTION_QUEUE, &json!({}));
        let listed: QueueResponse = replies[0].1.clone().unwrap().decode().unwrap();
        listed.messages.into_iter().map(|m| m.id).collect()
    }

    /// What a `receive` with `all` takes: the ids, in order.
    fn taken(bench: &mut Bench) -> Vec<String> {
        let (_, replies) = bench.call(ACTION_RECEIVE, &json!({"all": true}));
        let received: ReceiveResponse = replies[0].1.clone().unwrap().decode().unwrap();
        received.message.into_iter().chain(received.rest).map(|m| m.id).collect()
    }

    fn said_by(bench: &mut Bench, id: &str, author: &str) {
        let say = json!({"id": id, "text": format!("message {id}"), "author": author});
        assert!(bench.call(ACTION_SAY, &say).1[0].1.is_ok());
    }

    #[test]
    fn send_now_delivers_only_the_selected_original_message_between_steps() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        said_by(&mut bench, "waiting", "Lei");
        let file = json!({"name":"proof.txt", "media_type":"text/plain", "path":"attachments/default/proof.txt", "bytes":12});
        assert!(
            bench
                .call(
                    ACTION_SAY,
                    &json!({"id":"selected", "text":"Use induction", "author":"Ada", "files":[file]})
                )
                .1[0]
                .1
                .is_ok()
        );
        let between = json!({"all":true, "now":true});
        let empty: ReceiveResponse =
            bench.call(ACTION_RECEIVE, &between).1[0].1.clone().unwrap().decode().unwrap();
        assert!(empty.message.is_none(), "ordinary queued input waits for the current turn to end");
        assert!(bench.call("send_now", &json!({"id":"selected", "author":"Lei"})).1[0].1.is_ok());
        let mut bench = Bench::new(dir.path());
        let got: ReceiveResponse =
            bench.call(ACTION_RECEIVE, &between).1[0].1.clone().unwrap().decode().unwrap();
        let message = got.message.unwrap();
        assert_eq!(
            (&message.id, &message.text, message.author.as_deref()),
            (&"selected".into(), &"Use induction".into(), Some("Ada"))
        );
        assert_eq!(serde_json::to_value(&message.files).unwrap(), json!([file]));
        assert!(got.rest.is_empty());
        assert_eq!(queued(&mut bench), ["waiting"]);
        let refused = bench.call("send_now", &json!({"id":"selected"})).1[0].1.clone().unwrap_err();
        assert_eq!(refused.code, nucleus_protocol::ErrorCode::InvalidInput);
        assert_eq!(taken(&mut bench), ["waiting"]);
        assert_eq!(
            bench.history(None).iter().filter(|r| r["kind"] == "user" && r["id"] == "selected").count(),
            1
        );
    }

    /// A move and a withdrawal before the take decide what the agent
    /// receives and in which order; the records name who did it to whose
    /// message, and a restart rebuilds the same queue.
    #[test]
    fn moves_and_withdrawals_before_the_take_decide_what_the_agent_receives() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        for (id, author) in [("u1", "Ada"), ("u2", "Lei"), ("u3", "Ada")] {
            said_by(&mut bench, id, author);
        }
        assert_eq!(queued(&mut bench), ["u1", "u2", "u3"]);
        let moved = bench.call(ACTION_MOVE, &json!({"id": "u3", "before": "u1", "author": "Lei"}));
        assert!(moved.1[0].1.is_ok());
        let withdrawn = bench.call(ACTION_WITHDRAW, &json!({"id": "u2", "author": "Ada"}));
        assert!(withdrawn.1[0].1.is_ok());
        assert_eq!(queued(&mut bench), ["u3", "u1"]);
        let history = bench.history(None);
        let moved = history.iter().find(|r| r["kind"] == RECORD_MESSAGE_MOVED).unwrap();
        assert_eq!(
            (&moved["id"], &moved["before"], &moved["order"], &moved["author"]),
            (&json!("u3"), &json!("u1"), &json!(["u3", "u1", "u2"]), &json!("Lei"))
        );
        let withdrawn = history.iter().find(|r| r["kind"] == RECORD_MESSAGE_WITHDRAWN).unwrap();
        assert_eq!(
            (&withdrawn["id"], &withdrawn["message_author"], &withdrawn["author"]),
            (&json!("u2"), &json!("Lei"), &json!("Ada")),
            "who removed whose message"
        );
        // To the end, without `before`; then a restart rebuilds the queue from the log.
        assert!(bench.call(ACTION_MOVE, &json!({"id": "u3"})).1[0].1.is_ok());
        let mut bench = Bench::new(dir.path());
        assert_eq!(queued(&mut bench), ["u1", "u3"]);
        assert_eq!(taken(&mut bench), ["u1", "u3"], "the agent never sees the withdrawn message");
        assert!(queued(&mut bench).is_empty());
    }

    /// Once the agent has a message, moving or withdrawing it is refused as
    /// taken; a message another session holds is not in this queue.
    #[test]
    fn a_taken_message_or_another_sessions_is_out_of_reach() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        said_by(&mut bench, "u1", "Ada");
        said_by(&mut bench, "u2", "Ada");
        assert_eq!(taken(&mut bench), ["u1", "u2"]);
        for (action, request) in
            [(ACTION_MOVE, json!({"id": "u1"})), (ACTION_WITHDRAW, json!({"id": "u2", "author": "Lei"}))]
        {
            let refused = bench.call(action, &request).1[0].1.clone().unwrap_err();
            assert_eq!(refused.code, ErrorCode::InvalidInput);
            assert!(refused.message.starts_with(ALREADY_TAKEN), "{}", refused.message);
        }
        said_by(&mut bench, "u3", "Ada");
        let other = ResourcePath::new(["default", "other"]).unwrap();
        let payload = Payload::encode(&json!({"id": "u3"})).unwrap();
        for action in [ACTION_WITHDRAW, ACTION_MOVE] {
            let replies =
                bench.service.dispatch_at(DispatchId(90), ProcessId(1), action, &other, &payload, bench.now);
            let refused = replies[0].1.clone().unwrap_err();
            assert!(refused.code == ErrorCode::InvalidInput && !refused.message.starts_with(ALREADY_TAKEN));
        }
        assert_eq!(queued(&mut bench), ["u3"], "nothing the other session's calls did reached it");
        let kinds: Vec<Value> = bench.history(None).iter().map(|r| r["kind"].clone()).collect();
        assert!(
            !kinds.contains(&json!(RECORD_MESSAGE_WITHDRAWN))
                && !kinds.contains(&json!(RECORD_MESSAGE_MOVED))
        );
    }

    /// At most so many messages, and so many bytes of them, wait: past
    /// either a `say` is refused, so what a `receive` with `all` takes is
    /// bounded.
    #[test]
    fn a_say_past_the_queues_bounds_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        for i in 0..MAX_QUEUED_MESSAGES {
            assert!(bench.say(&format!("u{i}"), "short")[0].1.is_ok());
        }
        assert_eq!(bench.say("one-more", "short")[0].1.clone().unwrap_err().code, ErrorCode::ServiceError);
        assert_eq!(taken(&mut bench).len(), MAX_QUEUED_MESSAGES, "one receive takes them all");
        let large = "x".repeat(MAX_TEXT_BYTES);
        let fit = MAX_QUEUED_BYTES / (MAX_TEXT_BYTES + 64);
        for i in 0..fit {
            assert!(bench.say(&format!("big{i}"), &large)[0].1.is_ok());
        }
        assert_eq!(bench.say("too-big", &large)[0].1.clone().unwrap_err().code, ErrorCode::ServiceError);
        assert!(bench.say("small", "fits")[0].1.is_ok());
    }
}
