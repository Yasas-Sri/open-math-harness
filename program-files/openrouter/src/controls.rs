//! `next` and `cancel`, parked off the dispatch loop.
//!
//! Neither call is served by waiting. A `next` is parked until its stream
//! has something to report (text, reasoning, progress, a queue change, the
//! end) or its wait is over; a `cancel` stops the stream at once and is
//! parked until the stream's reader has released its transport, or for at
//! most [`CANCEL_WAIT`], after which it answers `released: false`. The loop
//! looks at every parked call each time round (at least every 20 ms), so one
//! caller's quiet two-second pull never delays another caller's reply.
//!
//! One call of each kind per caller and stream may be parked at a time; a
//! second is refused rather than queued. What bounds the parked set as a
//! whole is the kernel's `max_inflight_per_endpoint`.

use std::time::{Duration, Instant};

use nucleus_model_interface::{
    ACTION_CANCEL, ACTION_NEXT, CancelRequest, CancelResponse, MAX_NEXT_WAIT_MS, NextRequest,
};
use nucleus_protocol::{DispatchId, Error, ErrorCode, Payload, ProcessId};
use nucleus_sdk::Dispatch;

use crate::Streams;

/// Longest a `cancel` waits for the stream's transport to be released.
pub const CANCEL_WAIT: Duration = Duration::from_secs(2);

struct Parked {
    dispatch: DispatchId,
    caller: ProcessId,
    stream: String,
    cancel: bool,
    until: Instant,
}

/// The parked calls and the streams they are about.
pub struct Controls {
    streams: Streams,
    parked: Vec<Parked>,
}

impl Controls {
    pub fn new(streams: Streams) -> Self {
        Self { streams, parked: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }

    /// Parks a `next` or `cancel`. A refusal is answered at once.
    pub fn park(&mut self, dispatch: &Dispatch, reply_margin_ms: u64) -> Result<(), Error> {
        let (stream, cancel, wait) = match dispatch.action.as_str() {
            ACTION_NEXT => {
                let request: NextRequest = dispatch.payload.decode()?;
                (request.stream, false, Duration::from_millis(request.wait_ms.min(MAX_NEXT_WAIT_MS)))
            }
            ACTION_CANCEL => {
                let request: CancelRequest = dispatch.payload.decode()?;
                (request.stream, true, CANCEL_WAIT)
            }
            _ => return Err(Error::new(ErrorCode::InvalidInput, "not a stream control action")),
        };
        let duplicate = self.parked.iter().any(|parked| {
            parked.dispatch == dispatch.id
                || (parked.caller == dispatch.caller && parked.stream == stream && parked.cancel == cancel)
        });
        if duplicate {
            return Err(Error::new(
                ErrorCode::ServiceError,
                "a call of this kind for this stream is already waiting; wait for its answer",
            ));
        }
        if cancel {
            // Takes effect now, whatever the wait for the release comes to.
            self.streams.cancel(dispatch.caller, &stream);
        }
        // Never hold the caller past its own deadline.
        let budget = Duration::from_millis(dispatch.remaining_ms.saturating_sub(reply_margin_ms));
        let until = Instant::now() + wait.min(budget);
        self.parked.push(Parked { dispatch: dispatch.id, caller: dispatch.caller, stream, cancel, until });
        Ok(())
    }

    /// The kernel withdrew a parked call (its caller gave up or exited).
    /// Returns whether it was parked here; the loop then answers it.
    pub fn withdraw(&mut self, dispatch: DispatchId) -> bool {
        let before = self.parked.len();
        self.parked.retain(|parked| parked.dispatch != dispatch);
        self.parked.len() != before
    }

    /// Every parked call that can be answered now, with its answer.
    pub fn ready(&mut self) -> Vec<(DispatchId, Result<Payload, Error>)> {
        let now = Instant::now();
        let streams = &self.streams;
        let mut answers = Vec::new();
        self.parked.retain(|parked| {
            let expired = now >= parked.until;
            let answer = if parked.cancel {
                let released = streams.cancel(parked.caller, &parked.stream);
                (released || expired).then(|| Payload::encode(&CancelResponse { released }))
            } else {
                streams
                    .poll_next(parked.caller, &parked.stream, expired)
                    .map(|next| next.and_then(|next| Payload::encode(&next)))
            };
            match answer {
                Some(answer) => {
                    answers.push((parked.dispatch, answer));
                    false
                }
                None => true,
            }
        });
        answers
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use nucleus_model_interface::NextResponse;
    use nucleus_protocol::{ActionName, ResourcePath};
    use serde_json::json;

    use super::*;

    fn dispatch(id: u64, caller: u64, action: &str, stream: &str, remaining_ms: u64) -> Dispatch {
        let payload = if action == ACTION_NEXT {
            json!({"stream": stream, "wait_ms": MAX_NEXT_WAIT_MS})
        } else {
            json!({"stream": stream})
        };
        Dispatch {
            id: DispatchId(id),
            endpoint: nucleus_protocol::EndpointName::new("model").unwrap(),
            caller: ProcessId(caller),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(["deepseek"]).unwrap(),
            payload: Payload::encode(&payload).unwrap(),
            remaining_ms,
        }
    }

    /// Polls like the dispatch loop until `id` is answered.
    fn answer(controls: &mut Controls, id: u64, within: Duration) -> Result<Payload, Error> {
        let until = Instant::now() + within;
        loop {
            if let Some((_, answer)) = controls.ready().into_iter().find(|(d, _)| *d == DispatchId(id)) {
                return answer;
            }
            assert!(Instant::now() < until, "dispatch {id} was not answered within {within:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A parked quiet pull never delays another caller's pull or cancel.
    #[test]
    fn a_pull_or_cancel_is_answered_at_once_while_sixteen_quiet_pulls_wait() {
        let streams = Streams::default();
        let later = Instant::now() + Duration::from_secs(60);
        let mut controls = Controls::new(streams.clone());
        let mut quiet = Vec::new();
        for caller in 1..=8 {
            for _ in 0..2 {
                let (id, feed) = streams.open(ProcessId(caller), later).unwrap();
                controls
                    .park(&dispatch(quiet.len() as u64 + 1, caller, ACTION_NEXT, &id, 30_000), 500)
                    .unwrap();
                quiet.push((id, feed));
            }
        }
        assert!(controls.ready().is_empty(), "nothing to report yet");

        let (chat, feed) = streams.open(ProcessId(100), later).unwrap();
        assert!(feed.push("hello"));
        controls.park(&dispatch(100, 100, ACTION_NEXT, &chat, 30_000), 500).unwrap();
        let next: NextResponse = answer(&mut controls, 100, CANCEL_WAIT).unwrap().decode().unwrap();
        assert_eq!(next.delta, "hello");

        // A reader that lets go as soon as it is told to: released, promptly.
        let reader = std::thread::spawn(move || {
            while feed.wait_for_retry(Duration::from_millis(10)) {}
            drop(feed);
        });
        controls.park(&dispatch(101, 100, ACTION_CANCEL, &chat, 30_000), 500).unwrap();
        let cancel: CancelResponse = answer(&mut controls, 101, CANCEL_WAIT).unwrap().decode().unwrap();
        assert!(cancel.released);
        reader.join().unwrap();
        // Answered while every quiet pull is still parked: none was waited out first.
        assert_eq!(controls.parked.len(), 16, "the quiet pulls are still waiting");
    }

    #[test]
    fn a_cancel_whose_reader_does_not_stop_answers_not_released_and_duplicates_are_refused() {
        let streams = Streams::default();
        let (id, reader) = streams.open(ProcessId(1), Instant::now() + Duration::from_secs(60)).unwrap();
        let mut controls = Controls::new(streams.clone());
        // 150 ms of the caller's time is left after the reply margin.
        controls.park(&dispatch(1, 1, ACTION_CANCEL, &id, 650), 500).unwrap();
        assert_eq!(
            controls.park(&dispatch(2, 1, ACTION_CANCEL, &id, 650), 500).unwrap_err().code,
            ErrorCode::ServiceError,
            "one cancel per caller and stream at a time"
        );
        assert!(!reader.alive(), "the cancel took effect at once");
        let cancel: CancelResponse =
            answer(&mut controls, 1, Duration::from_secs(1)).unwrap().decode().unwrap();
        assert!(!cancel.released, "the reader never let go");
        assert!(controls.is_empty());
        controls.park(&dispatch(3, 1, ACTION_NEXT, &id, 650), 500).unwrap();
        assert!(controls.withdraw(DispatchId(3)));
        assert!(controls.is_empty());
        drop(reader);
    }
}
