//! The stream consumer against a timed model-interface fixture over the real
//! framed SDK channel. The peer is a scripted provider, not a model; the
//! kernel's authorization is covered by the process integration tests. Both
//! sides run on a simulated clock that only the provider advances, so the
//! timings are exact and nothing sleeps.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;
use std::io::{BufReader, PipeReader, PipeWriter};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nucleus_model_interface::{FinishReason, ReasoningEffort, Role};
use nucleus_protocol::{FromKernel, Payload, ProcessId, ResponseBody, ToKernel, read_frame, write_frame};

use super::*;

#[derive(Clone, Copy)]
enum Event {
    /// The provider holds the stream in its queue for this long.
    Queued(u64),
    Dispatched,
    Output,
    Done,
}

struct Observed {
    cancellations: usize,
    queued_call_ms: u64,
}

fn completion() -> CompletionResponse {
    CompletionResponse {
        message: Message::text(Role::Assistant, "answer"),
        finish_reason: FinishReason::Stop,
        usage: model::Usage { input_tokens: 10, output_tokens: 3, ..model::Usage::default() },
        served_by: "fixture".into(),
    }
}

/// Simulated time. The consumer only reads it, while it waits for the
/// provider's answer, so the two never race.
#[derive(Clone)]
struct Clock(Arc<Mutex<Instant>>);

impl Clock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }

    /// What a provider that sleeps until `until` does.
    fn advance_to(&self, until: Instant) {
        let mut now = self.0.lock().unwrap();
        *now = (*now).max(until);
    }
}

/// A provider that plays `events` (milliseconds after `start`, what happens)
/// and answers `next` with a queue lease while a `Queued` holds.
fn fixture(
    events: Vec<(u64, Event)>,
    clock: Clock,
) -> (Connection<PipeReader, PipeWriter>, JoinHandle<Observed>) {
    let (incoming, mut replies) = std::io::pipe().unwrap();
    let (calls, outgoing) = std::io::pipe().unwrap();
    let server = std::thread::spawn(move || {
        write_frame(&mut replies, &FromKernel::Hello { process: ProcessId(7), handles: vec![] }).unwrap();
        let mut calls = BufReader::new(calls);
        let mut opened = clock.now();
        let mut events = VecDeque::from(events);
        let mut queue_until: Option<Instant> = None;
        let mut produced = 0;
        let mut polls = 0;
        let mut observed = Observed { cancellations: 0, queued_call_ms: 0 };
        while let Ok(ToKernel::Call { request, action, payload, deadline_ms, .. }) = read_frame(&mut calls) {
            let response = match action.as_str() {
                model::ACTION_START => {
                    opened = clock.now();
                    Payload::encode(&model::StartResponse { stream: "fixture-stream".into() }).unwrap()
                }
                model::ACTION_NEXT => {
                    let pull: model::NextRequest = payload.decode().unwrap();
                    assert!(pull.wait_ms <= model::MAX_NEXT_WAIT_MS);
                    polls += 1;
                    assert!(polls < 10_000, "the consumer busy-looped instead of polling");
                    if queue_until.is_some() {
                        observed.queued_call_ms = observed.queued_call_ms.max(deadline_ms.unwrap());
                    }
                    let mut wake = clock.now() + Duration::from_millis(pull.wait_ms);
                    if let Some((at, _)) = events.front() {
                        wake = wake.min(opened + Duration::from_millis(*at));
                    }
                    if let Some(until) = queue_until {
                        wake = wake.min(until);
                    }
                    clock.advance_to(wake);
                    let mut delta = String::new();
                    let mut done = None;
                    if events
                        .front()
                        .is_some_and(|(at, _)| clock.now() >= opened + Duration::from_millis(*at))
                    {
                        match events.pop_front().unwrap().1 {
                            Event::Queued(ttl) => {
                                queue_until = Some(clock.now() + Duration::from_millis(ttl));
                            }
                            Event::Dispatched => queue_until = None,
                            Event::Output => {
                                queue_until = None;
                                produced += 6;
                                delta = "answer".into();
                            }
                            Event::Done => done = Some(completion()),
                        }
                    }
                    let queue = queue_until.map(|until| model::QueueStatus {
                        valid_for_ms: u64::try_from(until.saturating_duration_since(clock.now()).as_millis())
                            .unwrap(),
                    });
                    Payload::encode(&model::NextResponse {
                        delta,
                        reasoning_delta: String::new(),
                        done,
                        produced,
                        queue,
                    })
                    .unwrap()
                }
                model::ACTION_CANCEL => {
                    observed.cancellations += 1;
                    Payload::encode(&model::CancelResponse { released: true }).unwrap()
                }
                other => panic!("unexpected action {other}"),
            };
            write_frame(
                &mut replies,
                &FromKernel::Response { request, result: Ok(ResponseBody::Payload(response)) },
            )
            .unwrap();
        }
        observed
    });
    (Connection::open(incoming, outgoing).unwrap(), server)
}

/// One streamed call against the fixture with a 150 ms stall limit.
fn run(
    events: Vec<(u64, Event)>,
    deadline_ms: u64,
) -> (Result<CompletionResponse, Error>, Vec<Progress>, Observed) {
    let clock = Clock(Arc::new(Mutex::new(Instant::now())));
    let (mut kernel, server) = fixture(events, clock.clone());
    let deadline = deadline_ms.to_string();
    let args =
        ["--model", "fixture", "--out", "/unused", "--stall-ms", "150", "--model-deadline-ms", &deadline];
    let options = Options::parse(args.map(str::to_owned)).unwrap();
    let request = CompletionRequest {
        messages: vec![Message::text(Role::User, "fixture")],
        tools: vec![],
        tool_choice: None,
        project: None,
        max_output_tokens: 32,
        temperature: None,
        reasoning_effort: Some(ReasoningEffort::Off),
    };
    let mut progress = Vec::new();
    let result = stream(
        &mut kernel,
        &options,
        HandleId(1),
        HandleId(2),
        &ResourcePath::new(["fixture"]).unwrap(),
        &request,
        &mut |seen| progress.push(seen),
        &|| clock.now(),
    )
    .unwrap();
    drop(kernel);
    (result, progress, server.join().unwrap())
}

#[test]
fn queue_heartbeats_outlast_model_stall_without_inventing_output_or_usage() {
    let (result, progress, observed) = run(
        vec![
            (0, Event::Queued(250)),
            (100, Event::Queued(250)),
            (200, Event::Queued(250)),
            (300, Event::Queued(250)),
            (400, Event::Dispatched),
            (460, Event::Output),
            (480, Event::Done),
        ],
        2_000,
    );
    assert_eq!(result.unwrap(), completion());
    assert_eq!(observed.cancellations, 0);
    assert!(observed.queued_call_ms <= 1_250, "a short lease bounds the pull's own deadline too");
    let queued: Vec<_> = progress.iter().filter(|p| p.queue.is_some()).collect();
    assert!(!queued.is_empty());
    assert!(queued.iter().all(|p| p.produced == 0 && p.first_byte_ms.is_none()));
    assert!(progress.iter().any(|p| p.first_byte_ms.is_some_and(|ms| ms >= 400) && p.produced == 6));
}

#[test]
fn expired_queue_lease_cancels_instead_of_becoming_generation() {
    let (result, progress, observed) = run(vec![(0, Event::Queued(250))], 2_000);
    let error = result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(error.message.contains("queue lease expired"), "{error:?}");
    assert_eq!(observed.cancellations, 1);
    assert!(progress.last().unwrap().queue.is_some_and(|q| q.valid_for_ms == 0));
    assert!(progress.iter().all(|p| p.produced == 0 && p.first_byte_ms.is_none()));
}

#[test]
fn overall_deadline_still_cancels_a_healthy_queue() {
    let events = (0..20).map(|i| (i * 50, Event::Queued(250))).collect();
    let (result, progress, observed) = run(events, 450);
    let error = result.unwrap_err();
    assert!(error.message.contains("within the deadline"), "{error:?}");
    assert_eq!(observed.cancellations, 1);
    assert!(progress.last().unwrap().waited_ms >= 400);
    assert!(progress.iter().all(|p| p.produced == 0 && p.first_byte_ms.is_none()));
}

/// A stream stopped without an acknowledged release may still be running
/// and billing: the call's outcome is unknown, and nothing retries it.
#[test]
fn timeout_recovery_requires_explicit_transport_release() {
    for reply in [
        Ok(Payload::encode(&serde_json::json!({})).unwrap()),
        Ok(Payload::encode(&serde_json::json!({"released": false})).unwrap()),
        Err(Error::new(ErrorCode::Timeout, "cancel timed out")),
        Err(Error::new(ErrorCode::Unavailable, "provider disappeared")),
    ] {
        assert_eq!(
            after_stream_cancel(reply, Error::new(ErrorCode::Timeout, "stalled")).code,
            ErrorCode::UnknownOutcome
        );
    }
    let reply = Payload::encode(&model::CancelResponse { released: true }).unwrap();
    assert_eq!(
        after_stream_cancel(Ok(reply), Error::new(ErrorCode::Timeout, "stalled")).code,
        ErrorCode::Timeout
    );
}
