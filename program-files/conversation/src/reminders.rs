//! Reminders: `remind`, `wake`, `reminders`, `cancel_reminder` and
//! `clear_reminders` (interface module doc, "Reminders").
//!
//! A reminder's source is what the kernel checked, never the request: the
//! action it admitted and the caller's process id it asserted with the
//! dispatch. `reminder_set` records that source with the text and due time;
//! `receive` (`Session::take`) records `reminder_fired` before it hands a due
//! one over; `cancel_reminder`, and `clear_reminders` for each pending one,
//! record `reminder_cancelled` with the canceller's process id and the action
//! the kernel admitted. The pending ones are whatever the log has set and
//! neither fired nor cancelled.

use nucleus_conversation_interface::{
    ACTION_CANCEL_REMINDER, ACTION_CLEAR_REMINDERS, ACTION_REMINDERS, CancelReminderRequest,
    CancelReminderResponse, MAX_PENDING_REMINDERS, RECEIVED_BY, RECORD_REMINDER_CANCELLED,
    RECORD_REMINDER_FIRED, RECORD_REMINDER_SET, RemindRequest, RemindResponse, Reminder, ReminderAction,
    ReminderSource, RemindersRequest, RemindersResponse,
};
use nucleus_protocol::{Error, ErrorCode, Payload, ProcessId, ResourcePath};
use serde_json::{Value, json};

use crate::{ConversationService, Session};

/// A session's pending reminders, in the order they were set.
#[derive(Debug, Default)]
pub(crate) struct Pending(Vec<Reminder>);

impl Pending {
    /// The pending reminders a session's log leaves.
    pub(crate) fn from_log(log: &[Value]) -> Result<Self, Error> {
        let mut pending = Self::default();
        for record in log {
            match record["kind"].as_str() {
                Some(RECORD_REMINDER_SET) => pending.0.push(from_record(record)?),
                Some(RECORD_REMINDER_FIRED | RECORD_REMINDER_CANCELLED) => {
                    pending.remove(record["id"].as_str().unwrap_or_default());
                }
                _ => {}
            }
        }
        Ok(pending)
    }

    /// When the earliest pending reminder is due.
    pub(crate) fn earliest(&self) -> Option<u64> {
        self.0.iter().map(|reminder| reminder.due_at_ms).min()
    }

    /// The reminder to deliver first at `now_ms`: the earliest due, then the
    /// earliest set.
    pub(crate) fn first_due(&self, now_ms: u64) -> Option<&Reminder> {
        self.0
            .iter()
            .filter(|reminder| reminder.due_at_ms <= now_ms)
            .min_by_key(|reminder| reminder.due_at_ms)
    }

    fn remove(&mut self, id: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|reminder| reminder.id != id);
        self.0.len() < before
    }
}

fn from_record(record: &Value) -> Result<Reminder, Error> {
    let broken = |why: String| Error::new(ErrorCode::ServiceError, format!("reminder record: {why}"));
    let text =
        |field: &str| record[field].as_str().map(str::to_owned).ok_or_else(|| broken(format!("no {field}")));
    Ok(Reminder {
        id: text("id")?,
        text: text("text")?,
        source: serde_json::from_value(record["source"].clone()).map_err(|e| broken(e.to_string()))?,
        set_at_ms: record["at_ms"].as_u64().unwrap_or(0),
        due_at_ms: record["due_at_ms"].as_u64().ok_or_else(|| broken("no due_at_ms".into()))?,
    })
}

impl Session {
    /// Records a reminder from `source`, due `after_ms` after `now_ms`. Its id
    /// is the `seq` of its `reminder_set` record, so ids never repeat.
    fn remind(
        &mut self,
        request: RemindRequest,
        source: ReminderSource,
        now_ms: u64,
    ) -> Result<RemindResponse, Error> {
        if self.reminders.0.len() >= MAX_PENDING_REMINDERS {
            return Err(Error::new(
                ErrorCode::LimitExceeded,
                format!("this session already has {MAX_PENDING_REMINDERS} pending reminders"),
            ));
        }
        let id = format!("r{}", self.next_seq);
        let due_at_ms = now_ms.saturating_add(request.after_ms);
        let record = json!({"kind": RECORD_REMINDER_SET, "id": id, "text": request.text, "source": source,
                            "due_at_ms": due_at_ms});
        self.append_at(record, now_ms)?;
        let reminder = Reminder { id: id.clone(), text: request.text, source, set_at_ms: now_ms, due_at_ms };
        self.reminders.0.push(reminder);
        Ok(RemindResponse { id, due_at_ms })
    }

    /// Takes the reminder to deliver first at `now_ms` for `receiver`,
    /// recorded as fired before it is returned. Stop holds even a later
    /// completion until a user message is delivered.
    pub(crate) fn fire(&mut self, now_ms: u64, receiver: ProcessId) -> Result<Option<Reminder>, Error> {
        if self.reminders_paused {
            return Ok(None);
        }
        let Some(reminder) = self.reminders.first_due(now_ms).cloned() else { return Ok(None) };
        let record = json!({"kind": RECORD_REMINDER_FIRED, "id": reminder.id, RECEIVED_BY: receiver});
        self.append_at(record, now_ms)?;
        self.reminders.remove(&reminder.id);
        Ok(Some(reminder))
    }

    /// Withdraws the pending reminder `id`, recording the caller the kernel
    /// asserted and the action it admitted.
    fn cancel(&mut self, id: &str, process: ProcessId, action: &str, now_ms: u64) -> Result<bool, Error> {
        if !self.reminders.0.iter().any(|reminder| reminder.id == id) {
            return Ok(false);
        }
        let record =
            json!({"kind": RECORD_REMINDER_CANCELLED, "id": id, "process": process, "action": action});
        self.append_at(record, now_ms)?;
        Ok(self.reminders.remove(id))
    }

    /// Withdraws every pending reminder, earliest due first, and returns them.
    fn clear(&mut self, process: ProcessId, now_ms: u64) -> Result<Vec<Reminder>, Error> {
        let mut cleared = self.reminders.0.clone();
        cleared.sort_by_key(|reminder| reminder.due_at_ms);
        for reminder in &cleared {
            self.cancel(&reminder.id, process, ACTION_CLEAR_REMINDERS, now_ms)?;
        }
        Ok(cleared)
    }
}

impl ConversationService {
    /// One of the five reminder actions, from `caller`. None creates a
    /// session: one that does not exist has no reminders to list or cancel,
    /// and setting one there is refused, since `wake` is held by processes
    /// outside the session and creating sessions is `ensure`'s.
    pub(crate) fn serve_reminder(
        &mut self,
        caller: ProcessId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        now_ms: u64,
    ) -> Result<Payload, Error> {
        match (action, ReminderAction::of(action)) {
            (_, Some(admitted)) => {
                let request: RemindRequest = payload.decode()?;
                request.validate(admitted)?;
                let Some(session) = self.existing(resource)? else {
                    return Err(Error::new(ErrorCode::InvalidInput, "no such session"));
                };
                let source = ReminderSource { action: admitted, process: caller };
                Payload::encode(&session.remind(request, source, now_ms)?)
            }
            (ACTION_REMINDERS, None) => {
                payload.decode::<RemindersRequest>()?;
                let mut reminders = self.existing(resource)?.map_or_else(Vec::new, |s| s.reminders.0.clone());
                reminders.sort_by_key(|reminder| reminder.due_at_ms);
                Payload::encode(&RemindersResponse { reminders })
            }
            (ACTION_CANCEL_REMINDER, None) => {
                let request: CancelReminderRequest = payload.decode()?;
                request.validate()?;
                let cancelled = match self.existing(resource)? {
                    Some(session) => session.cancel(&request.id, caller, ACTION_CANCEL_REMINDER, now_ms)?,
                    None => false,
                };
                Payload::encode(&CancelReminderResponse { cancelled })
            }
            (ACTION_CLEAR_REMINDERS, None) => {
                payload.decode::<RemindersRequest>()?;
                let reminders = match self.existing(resource)? {
                    Some(session) => session.clear(caller, now_ms)?,
                    None => Vec::new(),
                };
                Payload::encode(&RemindersResponse { reminders })
            }
            _ => Err(Error::new(ErrorCode::InvalidInput, "unknown action")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::{Duration, Instant};

    use nucleus_conversation_interface::{
        ACTION_CANCEL_REMINDER, ACTION_ENSURE, ACTION_POST, ACTION_RECEIVE, ACTION_REMIND, ACTION_REMINDERS,
        ACTION_WAKE, MAX_PENDING_REMINDERS, MIN_REMIND_MS, ReceiveResponse, RemindResponse,
        RemindersResponse,
    };
    use nucleus_protocol::{ErrorCode, ProcessId};
    use serde_json::{Value, json};

    use crate::tests::{Bench, log_kinds};

    const AGENT: ProcessId = ProcessId(7);
    const RUNNER: ProcessId = ProcessId(42);

    fn set(bench: &mut Bench, caller: ProcessId, action: &str, after_ms: u64, text: &str) -> RemindResponse {
        let replies = bench.call_as(caller, action, &json!({"after_ms": after_ms, "text": text})).1;
        replies[0].1.clone().unwrap().decode().unwrap()
    }

    fn listed(bench: &mut Bench) -> Vec<(String, Value)> {
        let replies = bench.call(ACTION_REMINDERS, &json!({})).1;
        let listed: RemindersResponse = replies[0].1.clone().unwrap().decode().unwrap();
        listed.reminders.into_iter().map(|r| (r.text, serde_json::to_value(r.source).unwrap())).collect()
    }

    fn received(bench: &mut Bench, all: bool) -> Option<ReceiveResponse> {
        let replies = bench.call(ACTION_RECEIVE, &json!({"all": all})).1;
        replies.first().map(|reply| reply.1.clone().unwrap().decode().unwrap())
    }

    #[test]
    fn a_reminders_source_is_the_admitted_action_and_caller_and_the_session_bounds_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        // Read-only or no-op calls on a session that does not exist create nothing.
        assert!(listed(&mut bench).is_empty());
        let none = bench.call(ACTION_CANCEL_REMINDER, &json!({"id": "r1"})).1;
        assert_eq!(none[0].1.clone().unwrap().decode::<Value>().unwrap(), json!({"cancelled": false}));
        assert!(!dir.path().join("default").exists());
        assert!(bench.call(ACTION_ENSURE, &json!({})).1[0].1.is_ok());

        let own = set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, "check run gen2");
        assert_eq!(own.due_at_ms, bench.now + MIN_REMIND_MS);
        set(&mut bench, RUNNER, ACTION_WAKE, 0, "run gen2 ended");
        assert_eq!(
            listed(&mut bench),
            [
                ("run gen2 ended".into(), json!({"action": "wake", "process": 42})),
                ("check run gen2".into(), json!({"action": "remind", "process": 7})),
            ],
            "earliest due first; the source is who the kernel said called, and how"
        );
        // `remind` has its floor whoever calls it; a request cannot name a source.
        for (action, payload) in [
            (ACTION_REMIND, json!({"after_ms": 0, "text": "now"})),
            (ACTION_WAKE, json!({"after_ms": 0, "text": "x", "source": {"action": "remind", "process": 7}})),
        ] {
            let replies = bench.call_as(RUNNER, action, &payload).1;
            assert_eq!(replies[0].1.clone().unwrap_err().code, ErrorCode::InvalidInput, "{action} {payload}");
        }
        // Any holder may cancel any of the session's reminders, once.
        let cancel = json!({"id": own.id});
        for expected in [true, false] {
            let replies = bench.call_as(RUNNER, ACTION_CANCEL_REMINDER, &cancel).1;
            assert_eq!(replies[0].1.clone().unwrap().decode::<Value>().unwrap()["cancelled"], expected);
        }
        let cancelled = crate::read_log(&dir.path().join("default/default/log.jsonl")).unwrap();
        assert_eq!(cancelled.last().unwrap()["process"], 42);
        for n in 1..MAX_PENDING_REMINDERS {
            set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, &format!("r{n}"));
        }
        let full =
            bench.call_as(AGENT, ACTION_REMIND, &json!({"after_ms": MIN_REMIND_MS, "text": "one more"}));
        assert_eq!(full.1[0].1.clone().unwrap_err().code, ErrorCode::LimitExceeded);
        assert_eq!(listed(&mut bench).len(), MAX_PENDING_REMINDERS);
    }

    #[test]
    fn a_due_reminder_is_received_once_even_across_a_restart_and_opens_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        // A parked receive is woken by a wake that is due now.
        let (parked, replies) = bench.call(ACTION_RECEIVE, &json!({"all": true}));
        assert!(replies.is_empty() && bench.service.next_due(Instant::now(), bench.now).is_none());
        let replies = bench.call_as(RUNNER, ACTION_WAKE, &json!({"after_ms": 0, "text": "run ended"})).1;
        assert_eq!(replies[1].0, parked);
        let woken: ReceiveResponse = replies[1].1.clone().unwrap().decode().unwrap();
        assert_eq!((woken.message, woken.reminders[0].text.as_str()), (None, "run ended"));
        // A reminder that fired opened a turn, so completion records its end.
        assert_eq!(bench.call(ACTION_POST, &json!({"kind": "turn_complete"})).1.len(), 1);
        assert_eq!(log_kinds(dir.path()), ["reminder_set", "reminder_fired", "turn_complete"]);

        // A parked receive is woken when its own reminder comes due, and not before.
        set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, "look again");
        let (parked, _) = bench.call(ACTION_RECEIVE, &json!({}));
        let at = Instant::now();
        let due = at + Duration::from_millis(MIN_REMIND_MS);
        assert_eq!(bench.service.next_due(at, bench.now), Some(due), "the run loop sleeps until then");
        assert!(bench.service.fire(bench.now + MIN_REMIND_MS - 1).is_empty());
        let fired = bench.service.fire(bench.now + MIN_REMIND_MS);
        assert_eq!(fired[0].0, parked);
        assert!(bench.service.fire(bench.now + MIN_REMIND_MS).is_empty(), "delivered once");

        // Without `all` the earliest item comes first; with it, everything waiting.
        set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, "due first");
        bench.now += MIN_REMIND_MS + 1;
        bench.say("u1", "said after it came due");
        let first = received(&mut bench, false).unwrap();
        assert_eq!((first.message, first.reminders.len()), (None, 1));
        set(&mut bench, RUNNER, ACTION_WAKE, 0, "due now");
        let rest = received(&mut bench, true).unwrap();
        assert_eq!((rest.message.unwrap().id.as_str(), rest.reminders.len()), ("u1", 1));

        // One that came due while the service was down is delivered at the next receive, once.
        set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, "while down");
        let then = bench.now;
        drop(bench);
        let mut restarted = Bench::new(dir.path());
        restarted.now = then;
        assert!(received(&mut restarted, true).is_none(), "not due yet: parked");
        restarted.now = then + MIN_REMIND_MS;
        let late = received(&mut restarted, true).unwrap();
        assert_eq!(late.reminders[0].text, "while down");
        assert_eq!(late.reminders[0].source.process, AGENT);
        drop(restarted);
        let mut again = Bench::new(dir.path());
        again.now = then + 2 * MIN_REMIND_MS;
        assert!(received(&mut again, true).is_none());
        let history = again.history(None);
        assert!(history.iter().all(|r| r["v"] == crate::LOG_FORMAT_VERSION), "no format change");
        assert_eq!(history.iter().filter(|r| r["kind"] == "reminder_fired").count(), 5);
    }

    /// Stop applies to completions already queued and to those arriving later,
    /// even when completion won the race with Stop or the service restarts.
    #[test]
    fn stopped_sessions_keep_completions_pending_until_a_persons_message_is_delivered() {
        for queued_before_stop in [false, true] {
            for completed_before_stop in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let mut bench = Bench::new(dir.path());
                bench.say("u1", "work");
                assert_eq!(received(&mut bench, true).unwrap().message.unwrap().id, "u1");
                if queued_before_stop {
                    set(&mut bench, RUNNER, ACTION_WAKE, 0, "background ended");
                }
                if completed_before_stop {
                    bench.call(ACTION_POST, &json!({"kind": "turn_complete"}));
                }
                bench.call(ACTION_POST, &json!({"kind": "interrupted"}));
                if !queued_before_stop {
                    set(&mut bench, RUNNER, ACTION_WAKE, 0, "background ended");
                }
                set(&mut bench, AGENT, ACTION_REMIND, MIN_REMIND_MS, "check again");
                let (parked, replies) = bench.call(ACTION_RECEIVE, &json!({"all": true}));
                assert!(replies.is_empty(), "Stop must not start a reminder-only turn");
                assert!(bench.service.next_due(Instant::now(), bench.now).is_none(), "no busy timer");
                assert!(bench.service.fire(bench.now + MIN_REMIND_MS).is_empty());
                assert_eq!(listed(&mut bench).len(), 2, "results are retained");
                bench.service.cancel(parked);
                let now = bench.now + MIN_REMIND_MS;
                drop(bench);

                let mut bench = Bench::new(dir.path());
                bench.now = now;
                let (parked, replies) = bench.call(ACTION_RECEIVE, &json!({"all": true}));
                assert!(replies.is_empty(), "the pause survives a restart");
                assert!(bench.service.next_due(Instant::now(), bench.now).is_none());
                assert!(bench.service.fire(bench.now).is_empty());
                assert!(!bench.history(None).iter().any(|r| r["kind"] == "reminder_fired"));
                let replies = bench.say("u2", "Continue");
                let reply = replies.iter().find(|r| r.0 == parked).unwrap();
                let resumed: ReceiveResponse = reply.1.clone().unwrap().decode().unwrap();
                assert_eq!(resumed.message.unwrap().id, "u2");
                assert_eq!(resumed.reminders.len(), 2);
                drop(bench);

                let mut bench = Bench::new(dir.path());
                bench.now = now;
                set(&mut bench, RUNNER, ACTION_WAKE, 0, "another completion");
                let next = received(&mut bench, true).unwrap();
                assert_eq!(next.reminders[0].text, "another completion", "delivery clears the pause durably");
            }
        }
    }

    #[test]
    fn a_queued_user_message_precedes_old_reminders_after_stop_even_without_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        bench.call(ACTION_ENSURE, &json!({}));
        set(&mut bench, RUNNER, ACTION_WAKE, 0, "old completion");
        bench.now += 1;
        bench.say("u1", "a queued request");
        bench.call(ACTION_POST, &json!({"kind": "interrupted"}));
        let taken = received(&mut bench, false).unwrap();
        assert_eq!(taken.message.unwrap().id, "u1");
        assert!(taken.reminders.is_empty());
        assert_eq!(received(&mut bench, false).unwrap().reminders[0].text, "old completion");
    }

    #[test]
    fn only_delivery_of_a_user_message_clears_the_pause_including_send_now() {
        let dir = tempfile::tempdir().unwrap();
        let mut bench = Bench::new(dir.path());
        bench.say("u1", "work");
        received(&mut bench, true).unwrap();
        bench.call(ACTION_POST, &json!({"kind": "interrupted"}));
        bench.say("u2", "withdraw me");
        bench.call(nucleus_conversation_interface::ACTION_WITHDRAW, &json!({"id": "u2"}));
        // A late turn_complete and a withdrawn message cannot undo Stop.
        bench.call(ACTION_POST, &json!({"kind": "turn_complete"}));
        set(&mut bench, RUNNER, ACTION_WAKE, 0, "background ended");
        let (parked, replies) = bench.call(ACTION_RECEIVE, &json!({}));
        assert!(replies.is_empty());
        bench.service.cancel(parked);
        bench.say("u3", "Continue now");
        bench.call(nucleus_conversation_interface::ACTION_SEND_NOW, &json!({"id": "u3"}));
        let replies = bench.call(ACTION_RECEIVE, &json!({"now": true})).1;
        let taken: ReceiveResponse = replies[0].1.clone().unwrap().decode().unwrap();
        assert_eq!(taken.message.unwrap().id, "u3");
        assert!(taken.reminders.is_empty());
        assert_eq!(received(&mut bench, true).unwrap().reminders[0].text, "background ended");
    }
}
