//! Streams in flight: what `start` creates, `next` drains, and `cancel`,
//! idleness or the deadline ends.
//!
//! One reader thread per stream pulls from the provider and appends to a
//! bounded buffer. When the buffer is full the reader stops reading, so an
//! idle consumer pushes back on the provider through TCP instead of growing
//! memory here. The dispatch loop is the only other party, and it never
//! waits here: it asks [`Streams::poll_next`] and [`Streams::cancel`] whether
//! a parked call can be answered yet.
//!
//! A stream ends for its reader when it is cancelled, when nobody has pulled
//! it for [`IDLE_LIMIT`], or at its deadline (half a second after the `start`
//! caller's own, capped by `--max-timeout-ms`). The reader looks at all three
//! whenever it touches the stream and at least every 50 ms while it waits on
//! the network. Its table entry stays until the reader has finished, so a
//! stream counts against its caller's bound until its transport is gone, and
//! `cancel` can say whether that happened.
//!
//! On failure, pending answer and readable reasoning are drained as provisional
//! text before the next pull returns the original error. A failed stream never
//! manufactures a completed response or authorizes tool calls.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use nucleus_model_interface::{CompletionResponse, MAX_QUEUE_LEASE_MS, NextResponse, QueueStatus};
use nucleus_protocol::{Error, ErrorCode, ProcessId};

/// Streams one caller may have open at once, counted until each one's reader
/// has ended. The bound is per caller, not per adapter, so that background
/// workers (a discovery run's provers) cannot take every stream and refuse
/// another caller's. The total is bounded by the kernel's process limit times
/// this.
pub const MAX_STREAMS_PER_CALLER: usize = 2;
/// Answer and readable reasoning bytes together, before the reader stops
/// reading from the provider. Neither channel gets a separate allowance.
pub const MAX_BUFFERED_BYTES: usize = 256 * 1024;
/// A stream nobody pulls for this long is abandoned.
pub const IDLE_LIMIT: Duration = Duration::from_secs(30);

struct Stream {
    /// The process that started it, as asserted by the kernel.
    owner: ProcessId,
    buffer: String,
    reasoning_buffer: String,
    outcome: Option<Result<CompletionResponse, Error>>,
    last_pull: Instant,
    /// Bytes the reader has seen from the provider: content, reasoning and
    /// tool-call fragments together. Reasoning is buffered separately from the
    /// answer and tool calls not at all, so a consumer may report progress
    /// without displaying either.
    produced: u64,
    /// `produced` as last reported, so progress alone wakes a waiting pull.
    reported: u64,
    /// While the stream waits for a provider slot: until when the puller may
    /// read its silence as queueing. Kept after it passes, so a pull reports
    /// an expired lease as zero rather than as no lease.
    queue_until: Option<Instant>,
    queue_changed: bool,
    queue_expiry_reported: bool,
    cancelled: bool,
    /// The reader has stopped and its transport is released.
    reader_finished: bool,
    deadline: Instant,
}

impl Stream {
    fn over(&self, now: Instant) -> bool {
        self.cancelled || now.duration_since(self.last_pull) >= IDLE_LIMIT || now >= self.deadline
    }
}

#[derive(Default)]
struct Table {
    streams: BTreeMap<String, Stream>,
    next_id: u64,
}

/// Shared between the dispatch loop and the reader threads.
#[derive(Clone, Default)]
pub struct Streams {
    table: Arc<Mutex<Table>>,
    changed: Arc<Condvar>,
}

fn no_such_stream() -> Error {
    // The same answer for a stream that does not exist and one that belongs to
    // someone else, so ids cannot be probed.
    Error::new(ErrorCode::PermissionDenied, "no such stream for this caller")
}

impl Streams {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a stream for `owner`, ending at `deadline`, and returns its
    /// id and the reader's end.
    pub fn open(&self, owner: ProcessId, deadline: Instant) -> Result<(String, Feed), Error> {
        let mut table = self.lock();
        let now = Instant::now();
        table.streams.retain(|_, stream| {
            if now.duration_since(stream.last_pull) >= IDLE_LIMIT {
                stream.cancelled = true;
            }
            !(stream.cancelled && stream.reader_finished)
        });
        self.changed.notify_all();
        if table.streams.values().filter(|s| s.owner == owner).count() >= MAX_STREAMS_PER_CALLER {
            return Err(Error::new(
                ErrorCode::ServiceError,
                "this caller already has its maximum of open streams; finish or cancel one first",
            ));
        }
        table.next_id += 1;
        let id = format!("s{}", table.next_id);
        table.streams.insert(
            id.clone(),
            Stream {
                owner,
                buffer: String::new(),
                reasoning_buffer: String::new(),
                outcome: None,
                last_pull: now,
                produced: 0,
                reported: 0,
                queue_until: None,
                queue_changed: false,
                queue_expiry_reported: false,
                cancelled: false,
                reader_finished: false,
                deadline,
            },
        );
        Ok((id.clone(), Feed { streams: self.clone(), id }))
    }

    /// What a `next` would return now, or `None` while it should keep
    /// waiting: there is text, progress, a queue change or an outcome to
    /// report, or `give_up` says the pull has waited long enough.
    pub fn poll_next(
        &self,
        caller: ProcessId,
        id: &str,
        give_up: bool,
    ) -> Option<Result<NextResponse, Error>> {
        let mut table = self.lock();
        let Some(stream) = table.streams.get_mut(id).filter(|s| s.owner == caller) else {
            return Some(Err(no_such_stream()));
        };
        if stream.cancelled {
            return Some(Err(Error::new(ErrorCode::Cancelled, "this stream was cancelled")));
        }
        let now = Instant::now();
        stream.last_pull = now;
        let queue_expired = stream.queue_until.is_some_and(|until| now >= until);
        let ready = !stream.buffer.is_empty()
            || !stream.reasoning_buffer.is_empty()
            || stream.outcome.is_some()
            || stream.produced > stream.reported
            || stream.queue_changed
            || (queue_expired && !stream.queue_expiry_reported);
        if !ready && !give_up {
            return None;
        }
        let delta = std::mem::take(&mut stream.buffer);
        let reasoning_delta = std::mem::take(&mut stream.reasoning_buffer);
        let outcome = stream.outcome.take();
        let produced = stream.produced;
        stream.reported = produced;
        let queue = stream.queue_until.map(|until| QueueStatus {
            valid_for_ms: u64::try_from(until.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX),
        });
        stream.queue_changed = false;
        stream.queue_expiry_reported = queue.is_some_and(|status| status.valid_for_ms == 0);
        // Room was made: a reader waiting on a full buffer may continue.
        self.changed.notify_all();
        let pending = NextResponse { delta, reasoning_delta, done: None, produced, queue };
        Some(match outcome {
            None => Ok(pending),
            Some(Err(error)) if !pending.delta.is_empty() || !pending.reasoning_delta.is_empty() => {
                // Already-received display text survives a failed completion.
                // Deliver it once, then return the original error on the next
                // pull; never invent a final response.
                stream.outcome = Some(Err(error));
                Ok(pending)
            }
            Some(result) => {
                table.streams.remove(id);
                result.map(|done| NextResponse { done: Some(done), ..pending })
            }
        })
    }

    /// Asks the stream's reader to stop, and says whether it has: `true`
    /// once the reader and its network attempt are gone, and for an unknown
    /// or another caller's stream (which is left alone), so ids cannot be
    /// probed. Repeating it is how a caller waits for the release.
    pub fn cancel(&self, caller: ProcessId, id: &str) -> bool {
        let mut table = self.lock();
        let Some(stream) = table.streams.get_mut(id).filter(|s| s.owner == caller) else {
            return true;
        };
        stream.cancelled = true;
        self.changed.notify_all();
        if stream.reader_finished {
            table.streams.remove(id);
            true
        } else {
            false
        }
    }

    /// Tells every reader to stop, for shutdown.
    pub fn stop_all(&self) {
        let mut table = self.lock();
        for stream in table.streams.values_mut() {
            stream.cancelled = true;
        }
        self.changed.notify_all();
    }
}

/// A reader thread's end of one stream.
pub struct Feed {
    streams: Streams,
    id: String,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Feed({})", self.id)
    }
}

impl Feed {
    /// Whether anyone still wants this stream: not cancelled, pulled within
    /// [`IDLE_LIMIT`], and before its deadline.
    pub fn alive(&self) -> bool {
        let table = self.streams.lock();
        table.streams.get(&self.id).is_some_and(|stream| !stream.over(Instant::now()))
    }

    /// Waits `delay` before a retry, and says whether the stream is still
    /// wanted at the end of it. Cancellation ends the wait early.
    pub fn wait_for_retry(&self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        let mut table = self.streams.lock();
        loop {
            let now = Instant::now();
            let Some(stream) = table.streams.get(&self.id) else { return false };
            if stream.over(now) {
                return false;
            }
            let left = until.saturating_duration_since(now);
            if left.is_zero() {
                return true;
            }
            let idle_left = IDLE_LIMIT.saturating_sub(now.duration_since(stream.last_pull));
            let wait = left.min(idle_left).min(stream.deadline.saturating_duration_since(now));
            table = self.streams.changed.wait_timeout(table, wait).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Sets or clears the queue lease: `Some` while the stream waits for a
    /// provider slot (at most [`MAX_QUEUE_LEASE_MS`] and never past the
    /// deadline), `None` once it is dispatched. Not model output: `produced`
    /// does not move. Returns false once the stream is over.
    pub fn set_queue(&self, valid_for_ms: Option<u64>) -> bool {
        let now = Instant::now();
        let mut table = self.streams.lock();
        let Some(stream) = table.streams.get_mut(&self.id) else { return false };
        if stream.over(now) {
            return false;
        }
        let until = valid_for_ms
            .map(|ms| (now + Duration::from_millis(ms.min(MAX_QUEUE_LEASE_MS))).min(stream.deadline));
        if stream.queue_until != until {
            stream.queue_until = until;
            stream.queue_changed = true;
            stream.queue_expiry_reported = false;
            self.streams.changed.notify_all();
        }
        true
    }

    /// Buffers readable reasoning under the same total byte budget as answers,
    /// and counts `tool_bytes` of tool-call fragments, which reach the puller
    /// only whole with the completion: for them only `produced` moves. Both
    /// in one step, so a pull sees one event's output whole.
    pub fn push_reasoning(&self, text: &str, tool_bytes: usize) -> bool {
        self.push_text(text, true, tool_bytes)
    }

    /// Appends text, first waiting for room. Returns false once the stream is
    /// over (cancelled, abandoned or past its deadline), which tells the
    /// reader to stop.
    pub fn push(&self, text: &str) -> bool {
        self.push_text(text, false, 0)
    }

    fn push_text(&self, mut text: &str, reasoning: bool, mut uncounted: usize) -> bool {
        let mut table = self.streams.lock();
        loop {
            let Some(stream) = table.streams.get_mut(&self.id) else { return false };
            if stream.over(Instant::now()) {
                return false;
            }
            if uncounted > 0 {
                stream.produced = stream.produced.saturating_add(uncounted as u64);
                uncounted = 0;
                if stream.queue_until.take().is_some() {
                    stream.queue_changed = true;
                }
                self.streams.changed.notify_all();
            }
            if text.is_empty() {
                return true;
            }
            // An individual SSE event may exceed our whole buffer. Split at
            // UTF-8 boundaries and apply backpressure even to that first event.
            let available = MAX_BUFFERED_BYTES - stream.buffer.len() - stream.reasoning_buffer.len();
            let mut end = available.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                let chunk = &text[..end];
                if reasoning {
                    stream.reasoning_buffer.push_str(chunk);
                } else {
                    stream.buffer.push_str(chunk);
                }
                stream.produced = stream.produced.saturating_add(end as u64);
                if stream.queue_until.take().is_some() {
                    stream.queue_changed = true;
                }
                self.streams.changed.notify_all();
                text = &text[end..];
                continue;
            }
            table = self
                .streams
                .changed
                .wait_timeout(table, Duration::from_millis(250))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Records how the stream ended. The reader's transport must already be
    /// released: from here on `cancel` reports the stream released.
    pub fn finish(self, outcome: Result<CompletionResponse, Error>) {
        let mut table = self.streams.lock();
        if let Some(stream) = table.streams.get_mut(&self.id) {
            stream.queue_until = None;
            stream.outcome = Some(outcome);
        }
        drop(table);
        // Dropping `self` marks the reader finished.
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let mut table = self.streams.lock();
        if let Some(stream) = table.streams.get_mut(&self.id) {
            stream.reader_finished = true;
            if stream.outcome.is_none() {
                // A reader that never reached `finish`: a panic, or a thread
                // the host would not start.
                stream.outcome = Some(Err(Error::new(
                    ErrorCode::ServiceError,
                    "stream reader stopped without an outcome",
                )));
            }
        }
        self.streams.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use nucleus_model_interface::{FinishReason, Message, Role, Usage};

    use super::*;

    fn completion(text: &str) -> CompletionResponse {
        CompletionResponse {
            message: Message::text(Role::Assistant, text),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            served_by: "m".into(),
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// What the dispatch loop does for a parked `next`, as one blocking call.
    fn next(streams: &Streams, caller: ProcessId, id: &str, wait_ms: u64) -> Result<NextResponse, Error> {
        let until = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            if let Some(reply) = streams.poll_next(caller, id, Instant::now() >= until) {
                return reply;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn text_is_drained_in_order_and_the_last_call_carries_the_completion() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(7), later()).unwrap();
        assert!(feed.push("Hel"));
        assert!(feed.push("lo"));
        assert_eq!(
            next(&streams, ProcessId(7), &id, 0).unwrap(),
            NextResponse {
                delta: "Hello".into(),
                reasoning_delta: String::new(),
                done: None,
                produced: 5,
                queue: None
            }
        );
        assert!(streams.poll_next(ProcessId(7), &id, false).is_none(), "nothing new: the pull keeps waiting");
        assert_eq!(next(&streams, ProcessId(7), &id, 0).unwrap().delta, "", "an empty poll is not an error");
        // Reasoning moves the counter and stays separate from answer text.
        assert!(feed.push_reasoning(&"r".repeat(1200), 0));
        let thinking = next(&streams, ProcessId(7), &id, 0).unwrap();
        assert_eq!((thinking.delta.as_str(), thinking.produced), ("", 1205));
        assert_eq!(thinking.reasoning_delta, "r".repeat(1200));
        // Tool-call fragments move it too, and wake a waiting pull.
        assert!(feed.push_reasoning("", 7));
        assert_eq!(streams.poll_next(ProcessId(7), &id, false).unwrap().unwrap().produced, 1212);
        feed.finish(Ok(completion("Hello")));
        let last = next(&streams, ProcessId(7), &id, 0).unwrap();
        assert_eq!(last.done.unwrap().message.content, "Hello");
        assert_eq!(
            next(&streams, ProcessId(7), &id, 0).unwrap_err().code,
            ErrorCode::PermissionDenied,
            "gone after done"
        );
    }

    #[test]
    fn a_stream_belongs_to_the_process_that_started_it() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(7), later()).unwrap();
        feed.push("private");
        let foreign = next(&streams, ProcessId(8), &id, 0).unwrap_err();
        let missing = next(&streams, ProcessId(8), "s999", 0).unwrap_err();
        assert_eq!((foreign.code, &foreign.message), (missing.code, &missing.message), "indistinguishable");
        assert!(streams.cancel(ProcessId(8), &id), "answered like an unknown stream");
        assert_eq!(
            next(&streams, ProcessId(7), &id, 0).unwrap().delta,
            "private",
            "a stranger cannot cancel it either"
        );
    }

    #[test]
    fn reasoning_and_answer_share_one_bound_even_for_an_oversized_utf8_event() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(7), later()).unwrap();
        let prefix = "x".repeat(MAX_BUFFERED_BYTES - 2);
        assert!(feed.push(&prefix));
        let reasoning = "€🙂".repeat(MAX_BUFFERED_BYTES / 3);
        let expected = reasoning.clone();
        let (finished, done) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            finished.send(feed.push_reasoning(&reasoning, 0)).unwrap();
            feed.finish(Ok(completion("done")));
        });
        // Two bytes remain, insufficient for the next UTF-8 character. The
        // reader must wait, even though its reasoning buffer is still empty.
        assert!(done.recv_timeout(Duration::from_millis(100)).is_err());
        let first = next(&streams, ProcessId(7), &id, 0).unwrap();
        assert_eq!(first.delta, prefix);
        assert!(first.reasoning_delta.is_empty());
        let mut received = String::new();
        let mut latest_produced = first.produced;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "writer did not finish after the buffer was drained");
            let next = next(&streams, ProcessId(7), &id, 2_000).unwrap();
            assert!(next.delta.len() + next.reasoning_delta.len() <= MAX_BUFFERED_BYTES);
            assert!(next.produced >= latest_produced);
            latest_produced = next.produced;
            received.push_str(&next.reasoning_delta);
            if next.done.is_some() {
                break;
            }
        }
        assert_eq!(done.recv_timeout(Duration::from_secs(1)), Ok(true));
        writer.join().unwrap();
        assert_eq!(received, expected, "all UTF-8 bytes survive backpressure");
        assert_eq!(latest_produced as usize, prefix.len() + expected.len());
    }

    #[test]
    fn a_failure_drains_both_text_channels_before_returning_the_original_error_once() {
        let streams = Streams::default();
        let owner = ProcessId(7);
        let (id, feed) = streams.open(owner, later()).unwrap();
        assert!(feed.push("partial answer"));
        assert!(feed.push_reasoning("unfinished🙂", 0));
        let error = Error::new(ErrorCode::ServiceError, "upstream closed mid-response");
        feed.finish(Err(error.clone()));
        // The error and both pieces are already queued before this first pull.
        let partial = next(&streams, owner, &id, 0).unwrap();
        assert_eq!(partial.delta, "partial answer");
        assert_eq!(partial.reasoning_delta, "unfinished🙂");
        assert_eq!(partial.produced as usize, "partial answerunfinished🙂".len());
        assert!(partial.done.is_none(), "an error cannot authorize final tool calls");
        assert_eq!(next(&streams, owner, &id, 0).unwrap_err(), error);
        assert_eq!(next(&streams, owner, &id, 0).unwrap_err(), no_such_stream());
    }

    #[test]
    fn a_full_buffer_stops_the_reader_until_the_consumer_pulls_or_leaves() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(1), later()).unwrap();
        assert!(feed.push(&"x".repeat(MAX_BUFFERED_BYTES)));
        let (done, blocked) = std::sync::mpsc::channel();
        let (cancelled, proceed) = std::sync::mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            done.send(feed.push("more")).unwrap();
            proceed.recv().unwrap();
            // Blocked again on a full buffer: cancellation must wake it.
            feed.push(&"y".repeat(MAX_BUFFERED_BYTES))
        });
        assert!(blocked.recv_timeout(Duration::from_millis(150)).is_err(), "the reader is held back");
        assert_eq!(next(&streams, ProcessId(1), &id, 0).unwrap().delta.len(), MAX_BUFFERED_BYTES);
        assert_eq!(blocked.recv_timeout(Duration::from_secs(2)), Ok(true), "pulling made room");
        cancelled.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!streams.cancel(ProcessId(1), &id), "the reader has not stopped yet");
        assert!(!reader.join().unwrap(), "after cancel the reader is told to stop");
    }

    #[test]
    fn cancellation_wakes_retry_backoff_and_prevents_the_next_attempt() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(7), later()).unwrap();
        streams.cancel(ProcessId(8), &id);
        assert!(feed.wait_for_retry(Duration::ZERO), "a stranger cannot cancel retries");
        let (done, result) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            done.send(feed.wait_for_retry(Duration::from_secs(20))).unwrap();
            assert!(!feed.wait_for_retry(Duration::ZERO));
        });
        assert!(result.recv_timeout(Duration::from_millis(50)).is_err());
        streams.cancel(ProcessId(7), &id);
        assert_eq!(result.recv_timeout(Duration::from_secs(2)), Ok(false));
        waiting.join().unwrap();
    }

    /// A cancelled stream keeps counting against its caller until its reader
    /// has let go of the provider, and `cancel` says whether that happened.
    #[test]
    fn a_stream_counts_and_cancel_is_unconfirmed_until_its_reader_has_stopped() {
        let streams = Streams::default();
        let caller = ProcessId(3);
        let (first, reader) = streams.open(caller, later()).unwrap();
        let _second = streams.open(caller, later()).unwrap();
        assert!(streams.open(ProcessId(99), later()).is_ok(), "another caller still gets a stream");
        assert!(!streams.cancel(caller, &first), "the reader is still running");
        assert_eq!(
            streams.open(caller, later()).unwrap_err().code,
            ErrorCode::ServiceError,
            "a cancelled stream whose reader runs still counts"
        );
        assert!(!reader.alive());
        assert_eq!(next(&streams, caller, &first, 0).unwrap_err().code, ErrorCode::Cancelled);
        drop(reader);
        assert!(streams.cancel(caller, &first), "released once the reader has stopped");
        assert!(streams.open(caller, later()).is_ok());
    }

    #[test]
    fn the_deadline_ends_the_stream_for_its_reader() {
        let streams = Streams::default();
        let (_, feed) = streams.open(ProcessId(1), Instant::now() + Duration::from_millis(80)).unwrap();
        assert!(feed.alive());
        let began = Instant::now();
        assert!(!feed.wait_for_retry(Duration::from_secs(20)), "a retry cannot outlast the deadline");
        assert!(began.elapsed() < Duration::from_secs(2));
        assert!(!feed.push("late"));
    }

    #[test]
    fn a_queue_lease_wakes_a_pull_moves_no_progress_and_ends_with_output() {
        let streams = Streams::default();
        let (id, feed) = streams.open(ProcessId(1), later()).unwrap();
        assert!(feed.set_queue(Some(u64::MAX)));
        let queued = streams.poll_next(ProcessId(1), &id, false).unwrap().unwrap();
        let lease = queued.queue.unwrap().valid_for_ms;
        assert!((29_000..=MAX_QUEUE_LEASE_MS).contains(&lease), "clamped: {lease}");
        assert_eq!((queued.produced, queued.delta.as_str()), (0, ""));
        assert!(streams.poll_next(ProcessId(1), &id, false).is_none(), "an unchanged lease is no news");
        assert!(feed.set_queue(Some(1)));
        std::thread::sleep(Duration::from_millis(10));
        let expired = streams.poll_next(ProcessId(1), &id, false).unwrap().unwrap();
        assert_eq!(expired.queue, Some(QueueStatus { valid_for_ms: 0 }), "expiry is reported once");
        assert!(streams.poll_next(ProcessId(1), &id, false).is_none());
        assert!(feed.set_queue(Some(30_000)));
        assert!(feed.push("out"));
        let output = streams.poll_next(ProcessId(1), &id, false).unwrap().unwrap();
        assert_eq!((output.delta.as_str(), output.queue), ("out", None), "output ends the lease");
    }
}
