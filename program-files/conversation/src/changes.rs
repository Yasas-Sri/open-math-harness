//! The change feed (interface module doc, "The change feed"): which sessions
//! have records newer than a cursor, for the callers that follow every
//! session of the VM with one call (the platform's mirror and the windows).
//!
//! The service numbers the appends it makes to any session's log in order
//! (one counter for the run) and keeps, for each session changed in this
//! run, its newest change, the `seq` of its last visible record and whether
//! a turn is open on it: a session is listed once however often it changed.
//! A cursor is `<epoch>.<change>`; the epoch is drawn at the start of the
//! run, so a cursor of an earlier run (whose changes this run does not
//! know) is told to read everything again.

use std::collections::BTreeMap;
use std::time::Instant;

use nucleus_conversation_interface::{Change, ChangesRequest, ChangesResponse, MAX_CHANGE_WAITS};
use nucleus_protocol::{DispatchId, Error, ErrorCode, ResourcePath};

/// A parked `changes` call.
struct Waiting {
    id: DispatchId,
    after: u64,
    limit: usize,
    due: Instant,
}

/// What the feed answers a call: now, or when a change comes.
pub(crate) enum Answer {
    Now(ChangesResponse),
    Later,
}

pub(crate) struct Feed {
    epoch: String,
    /// The number of the last change of this run.
    count: u64,
    /// Each session changed in this run: its newest change, last `seq` and
    /// whether a turn is open on it.
    latest: BTreeMap<ResourcePath, (u64, u64, bool)>,
    waiting: Vec<Waiting>,
}

impl Feed {
    pub fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()),
        );
        Self::with_epoch(format!("{:016x}", hasher.finish()))
    }

    pub(crate) fn with_epoch(epoch: String) -> Self {
        Self { epoch, count: 0, latest: BTreeMap::new(), waiting: Vec::new() }
    }

    fn cursor(&self, change: u64) -> String {
        format!("{}.{change}", self.epoch)
    }

    /// The change a cursor of this run stands for; `None` for none, another
    /// run's, or one this run never gave.
    fn change_of(&self, cursor: Option<&str>) -> Option<u64> {
        let (epoch, change) = cursor?.split_once('.')?;
        let change: u64 = change.parse().ok()?;
        (epoch == self.epoch && change <= self.count).then_some(change)
    }

    /// `resource` was appended to; its last visible record is now `seq` and
    /// `turn_open` says whether a turn is open on it.
    pub fn record(&mut self, resource: &ResourcePath, seq: u64, turn_open: bool) {
        self.count += 1;
        self.latest.insert(resource.clone(), (self.count, seq, turn_open));
    }

    /// Drops the sessions `gone` names: they no longer exist, and a caller
    /// that asks for them is told so by `list` and `history`.
    pub fn forget(&mut self, gone: &impl Fn(&ResourcePath) -> bool) {
        self.latest.retain(|resource, _| !gone(resource));
    }

    /// The changes after `after`, in their order, at most `limit`.
    fn page(&self, after: u64, limit: usize) -> ChangesResponse {
        let mut newer: Vec<(&ResourcePath, u64, u64, bool)> = self
            .latest
            .iter()
            .filter(|(_, (change, ..))| *change > after)
            .map(|(resource, (change, seq, turn_open))| (resource, *change, *seq, *turn_open))
            .collect();
        newer.sort_by_key(|(_, change, ..)| *change);
        let more = newer.len() > limit;
        newer.truncate(limit);
        let next = newer.last().map_or(after, |(_, change, ..)| *change);
        let changes = newer
            .into_iter()
            .filter_map(|(resource, _, seq, turn_open)| match resource.segments() {
                [project, session] => Some(Change {
                    project: project.clone(),
                    session: session.clone(),
                    seq,
                    turn_open: Some(turn_open),
                }),
                _ => None,
            })
            .collect();
        ChangesResponse { changes, next: self.cursor(next), more, reset: false }
    }

    /// A `changes` call: answered now, or parked until a change or `due`.
    pub fn ask(
        &mut self,
        id: DispatchId,
        resource: &ResourcePath,
        request: &ChangesRequest,
        due: Instant,
    ) -> Result<Answer, Error> {
        if !resource.segments().is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "changes is every session's: its resource is []",
            ));
        }
        let limit = request.validate()?;
        let Some(after) = self.change_of(request.after.as_deref()) else {
            let next = self.cursor(self.count);
            return Ok(Answer::Now(ChangesResponse { changes: Vec::new(), next, more: false, reset: true }));
        };
        if after < self.count || !request.wait {
            return Ok(Answer::Now(self.page(after, limit)));
        }
        if self.waiting.len() >= MAX_CHANGE_WAITS {
            return Err(Error::new(
                ErrorCode::LimitExceeded,
                format!("{MAX_CHANGE_WAITS} calls already wait for changes"),
            ));
        }
        self.waiting.push(Waiting { id, after, limit, due });
        Ok(Answer::Later)
    }

    /// The parked calls a change now answers, and those due by `now`.
    pub fn answer(&mut self, now: Instant) -> Vec<(DispatchId, ChangesResponse)> {
        let count = self.count;
        let (done, waiting): (Vec<Waiting>, Vec<Waiting>) =
            std::mem::take(&mut self.waiting).into_iter().partition(|w| w.after < count || w.due <= now);
        self.waiting = waiting;
        done.into_iter().map(|w| (w.id, self.page(w.after, w.limit))).collect()
    }

    /// When the earliest parked call is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.waiting.iter().map(|w| w.due).min()
    }

    /// The kernel no longer wants an answer to `id`.
    pub fn cancel(&mut self, id: DispatchId) {
        self.waiting.retain(|w| w.id != id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::time::Duration;

    use super::*;

    fn session(project: &str, name: &str) -> ResourcePath {
        ResourcePath::new([project, name]).unwrap()
    }

    fn ask(feed: &mut Feed, after: Option<&str>, limit: Option<usize>, wait: bool) -> Answer {
        let request = ChangesRequest { after: after.map(str::to_owned), limit, wait };
        feed.ask(DispatchId(9), &ResourcePath::root(), &request, Instant::now() + Duration::from_secs(25))
            .unwrap()
    }

    fn now(answer: Answer) -> ChangesResponse {
        match answer {
            Answer::Now(response) => response,
            Answer::Later => panic!("parked"),
        }
    }

    /// A session is listed once, with its newest `seq` and whether a turn is
    /// open on it, in the order of its
    /// newest change; a page ends at `limit` and says `more`; a cursor of
    /// another run, none, or one never given is told to read everything.
    #[test]
    fn the_feed_lists_each_changed_session_once_by_its_newest_change() {
        let mut feed = Feed::with_epoch("e1".into());
        let start = now(ask(&mut feed, None, None, true));
        assert_eq!((start.reset, start.next.as_str(), start.changes.len()), (true, "e1.0", 0));
        feed.record(&session("alpha", "s1"), 0, true);
        feed.record(&session("beta", "s2"), 4, true);
        feed.record(&session("alpha", "s1"), 1, false);
        let all = now(ask(&mut feed, Some("e1.0"), None, false));
        let listed: Vec<(&str, &str, u64, Option<bool>)> = all
            .changes
            .iter()
            .map(|c| (c.project.as_str(), c.session.as_str(), c.seq, c.turn_open))
            .collect();
        assert_eq!(listed, [("beta", "s2", 4, Some(true)), ("alpha", "s1", 1, Some(false))]);
        assert_eq!((all.next.as_str(), all.more, all.reset), ("e1.3", false, false));
        let first = now(ask(&mut feed, Some("e1.0"), Some(1), false));
        assert_eq!((first.changes.len(), first.next.as_str(), first.more), (1, "e1.2", true));
        let rest = now(ask(&mut feed, Some(&first.next), Some(1), false));
        assert_eq!((rest.changes[0].session.as_str(), rest.next.as_str(), rest.more), ("s1", "e1.3", false));
        for stale in ["e0.2", "e1.4", "e1", "x"] {
            let reset = now(ask(&mut feed, Some(stale), None, false));
            assert_eq!((reset.reset, reset.next.as_str(), reset.changes.len()), (true, "e1.3", 0), "{stale}");
        }
        for limit in [0, 1001] {
            let request = ChangesRequest { after: None, limit: Some(limit), wait: false };
            assert!(feed.ask(DispatchId(1), &ResourcePath::root(), &request, Instant::now()).is_err());
        }
        let one = ChangesRequest::default();
        assert!(
            feed.ask(DispatchId(1), &session("alpha", "s1"), &one, Instant::now()).is_err(),
            "VM-wide only"
        );
    }

    /// A waiting call is answered by the next change, or with nothing when it
    /// is due; a limited number wait at once.
    #[test]
    fn a_waiting_call_is_answered_by_the_next_change_or_its_end() {
        let mut feed = Feed::with_epoch("e".into());
        assert!(matches!(ask(&mut feed, Some("e.0"), None, true), Answer::Later));
        assert!(feed.answer(Instant::now()).is_empty(), "nothing yet");
        feed.record(&session("alpha", "s1"), 7, true);
        let answered = feed.answer(Instant::now());
        assert_eq!(answered.len(), 1);
        assert_eq!(
            (answered[0].1.changes[0].seq, answered[0].1.changes[0].turn_open, answered[0].1.next.as_str()),
            (7, Some(true), "e.1")
        );
        let request = ChangesRequest { after: Some("e.1".into()), limit: None, wait: true };
        let due = Instant::now();
        assert!(matches!(
            feed.ask(DispatchId(2), &ResourcePath::root(), &request, due).unwrap(),
            Answer::Later
        ));
        let ended = feed.answer(due);
        assert_eq!(
            (ended[0].0, ended[0].1.changes.len(), ended[0].1.next.as_str()),
            (DispatchId(2), 0, "e.1")
        );
        for n in 0..MAX_CHANGE_WAITS {
            let id = DispatchId(u64::try_from(n).unwrap() + 10);
            assert!(matches!(
                feed.ask(id, &ResourcePath::root(), &request, Instant::now()).unwrap(),
                Answer::Later
            ));
        }
        let refused = feed.ask(DispatchId(99), &ResourcePath::root(), &request, Instant::now());
        assert_eq!(refused.err().map(|e| e.code), Some(ErrorCode::LimitExceeded));
        feed.cancel(DispatchId(10));
        assert_eq!(feed.waiting.len(), MAX_CHANGE_WAITS - 1);
    }

    /// Through the service: an append to any session answers the parked call,
    /// and the entry says whether a turn is open on the session.
    #[test]
    fn an_append_to_a_session_answers_the_service_s_parked_call() {
        use nucleus_conversation_interface::{ACTION_CHANGES, ACTION_POST};
        use nucleus_protocol::{Payload, ProcessId};
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let mut bench = crate::tests::Bench::new(dir.path());
        let changes = |bench: &mut crate::tests::Bench, id: u64, request: serde_json::Value| {
            let payload = Payload::encode(&request).unwrap();
            bench.service.dispatch_at(
                DispatchId(id),
                ProcessId(1),
                ACTION_CHANGES,
                &ResourcePath::root(),
                &payload,
                bench.now,
            )
        };
        let decode = |reply: &crate::Reply| reply.1.clone().unwrap().decode::<ChangesResponse>().unwrap();
        let start = decode(&changes(&mut bench, 100, json!({}))[0]);
        assert!(start.reset && start.changes.is_empty());
        assert!(changes(&mut bench, 101, json!({"after": start.next, "wait": true})).is_empty(), "parked");
        let replies = bench.say("u1", "hello");
        let answer = replies.iter().find(|reply| reply.0 == DispatchId(101)).map(decode).unwrap();
        assert_eq!(
            answer.changes,
            [Change { project: "default".into(), session: "default".into(), seq: 0, turn_open: Some(true) }]
        );
        assert!(bench.call(ACTION_POST, &json!({"kind": "assistant", "text": "hi"})).1[0].1.is_ok());
        let later = decode(&changes(&mut bench, 102, json!({"after": answer.next}))[0]);
        assert_eq!(later.changes.iter().map(|c| c.seq).collect::<Vec<_>>(), [1]);
        assert_eq!(later.changes[0].turn_open, Some(true), "an assistant's record keeps the turn open");
        assert!(bench.call(ACTION_POST, &json!({"kind": "turn_complete"})).1[0].1.is_ok());
        let done = decode(&changes(&mut bench, 103, json!({"after": later.next}))[0]);
        assert_eq!(done.changes[0].turn_open, Some(false), "a turn's end closes it");
    }
}
