//! One model call of a worker, blocking or streamed: a
//! queued stream is not a stalled one, and a stream stopped without a
//! confirmed release has an unknown outcome.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use nucleus_model_interface::{
    self as model, CompletionRequest, CompletionResponse, Message, ToolChoice, ToolSpec,
};
use nucleus_protocol::{Error, ErrorCode, HandleId, ResourcePath};
use nucleus_sdk::{ChannelError, Connection};

use crate::Options;

/// A provider that has produced nothing at all for this long has stopped,
/// whatever it is doing. Thinking counts as producing: a reasoning model's
/// tokens move `NextResponse::produced` without ever reaching `delta`, so
/// this is silence, not quiet. A blocking `complete` cannot tell a stalled
/// provider from a long answer before the model deadline; a pulled stream
/// can.
pub(crate) const STALL_MS: u64 = 90_000;

/// What a streamed call has done so far, reported on every poll. The loop
/// itself needs only [`Progress::produced`]; the caller writes it down, so
/// that whether a long call is still producing is answered by a record.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    /// Since `start` returned.
    pub waited_ms: u64,
    /// Bytes the provider has produced, thinking included.
    pub produced: u64,
    /// When the first byte the provider produced arrived; a queue lease is
    /// not one.
    pub first_byte_ms: Option<u64>,
    /// The provider's queue lease on the last pull, while the stream waits
    /// for a slot. `valid_for_ms: 0` is an expired lease, not the start of
    /// generation.
    pub queue: Option<model::QueueStatus>,
}

/// Where a worker's model calls go. `pulls` carries `next` and `cancel` on a
/// separate, unbudgeted capability: a poll is not a completion and must not
/// be charged as one (the model interface says so).
#[derive(Clone, Copy)]
pub struct ModelAccess {
    pub handle: HandleId,
    pub pulls: Option<HandleId>,
}

/// One model call with the options' limits; shared by every loop. Streams it
/// when the worker holds the capability to pull, because a stream is the only
/// thing that distinguishes a provider that is thinking from one that has
/// stopped answering.
// Eight, because the request's shape and where it goes are both arguments;
// bundling them would hide which loop passes what.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete<R: Read, W: Write>(
    kernel: &mut Connection<R, W>,
    options: &Options,
    model: ModelAccess,
    model_resource: &ResourcePath,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    tool_choice: Option<ToolChoice>,
    progress: &mut dyn FnMut(Progress),
) -> Result<Result<CompletionResponse, Error>, ChannelError> {
    let request = CompletionRequest {
        messages,
        tools,
        tool_choice,
        max_output_tokens: options.max_output_tokens,
        temperature: options.temperature,
        reasoning_effort: options.reasoning_effort,
        project: options.project.clone(),
    };
    match model.pulls {
        Some(pulls) => {
            stream(kernel, options, model.handle, pulls, model_resource, &request, progress, &Instant::now)
        }
        None => {
            let reply = kernel.call(
                model.handle,
                model::ACTION_COMPLETE,
                model_resource,
                &request,
                Some(options.model_deadline_ms),
            )?;
            Ok(reply.and_then(|p| p.decode::<CompletionResponse>()))
        }
    }
}

/// `start`, then pull until the completion arrives. The deltas are thrown
/// away — this loop has nobody to show them to — and what is kept is the
/// evidence that the provider is alive: `produced` rising. When it stops
/// rising for `--stall-ms` the stream is cancelled and the call fails as a
/// timeout rather than at the model deadline (`--model-deadline-ms`).
///
/// A stream the provider reports queued (a [`model::QueueStatus`] lease)
/// waits for a slot and is not stalled: while the lease holds the silence
/// clock does not run, and leaving the queue starts a fresh silence
/// interval. A lease never moves the deadline, and one that expires without
/// renewal cancels the stream. Every cancel waits for the provider's answer,
/// and without `released: true` the call's outcome is unknown
/// ([`after_stream_cancel`]). Every bound is measured on `clock`.
// `clock` is an argument so that tests run the loop on simulated time.
#[allow(clippy::too_many_arguments)]
fn stream<R: Read, W: Write>(
    kernel: &mut Connection<R, W>,
    options: &Options,
    model: HandleId,
    pulls: HandleId,
    resource: &ResourcePath,
    request: &CompletionRequest,
    progress: &mut dyn FnMut(Progress),
    clock: &dyn Fn() -> Instant,
) -> Result<Result<CompletionResponse, Error>, ChannelError> {
    let started =
        kernel.call(model, model::ACTION_START, resource, request, Some(options.model_deadline_ms))?;
    let stream = match started.and_then(|p| p.decode::<model::StartResponse>()) {
        Ok(started) => started.stream,
        Err(e) => return Ok(Err(e)),
    };
    let opened = clock();
    let give_up = opened + Duration::from_millis(options.model_deadline_ms);
    let stall = Duration::from_millis(options.stall_ms);
    let mut last_change = opened;
    let mut produced = 0;
    let mut first_byte_ms = None;
    let mut queue: Option<model::QueueStatus> = None;
    let mut queue_until: Option<Instant> = None;
    let quit = |kernel: &mut Connection<R, W>, error: Error| -> Result<Error, ChannelError> {
        let cancel = model::CancelRequest { stream: stream.clone() };
        let reply = kernel.call(pulls, model::ACTION_CANCEL, resource, &cancel, Some(10_000))?;
        Ok(after_stream_cancel(reply, error))
    };
    let since =
        |at: Instant| u64::try_from(clock().saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX);
    let late = || Error::new(ErrorCode::Timeout, "the model did not finish within the deadline");
    loop {
        if kernel.terminate_requested() {
            return Ok(Err(quit(kernel, Error::new(ErrorCode::Cancelled, "the worker was asked to stop"))?));
        }
        if clock() >= give_up {
            return Ok(Err(quit(kernel, late())?));
        }
        if queue.is_none() && clock().saturating_duration_since(last_change) >= stall {
            let why = format!(
                "the provider produced nothing for {} s (it had produced {produced} bytes); \
                 the stream was cancelled",
                options.stall_ms / 1000
            );
            return Ok(Err(quit(kernel, Error::new(ErrorCode::Timeout, why))?));
        }
        // Pull no later than the nearest bound: the deadline, the lease, or
        // the end of the silence allowance. The pull's own deadline leaves a
        // second for the reply beyond that, never past the call's deadline.
        let remaining = give_up.saturating_duration_since(clock());
        let patience = queue_until.map_or_else(
            || stall.saturating_sub(clock().saturating_duration_since(last_change)),
            |until| until.saturating_duration_since(clock()),
        );
        let wait_ms = u64::try_from(remaining.min(patience).as_millis())
            .unwrap_or(u64::MAX)
            .min(model::MAX_NEXT_WAIT_MS);
        let pull = model::NextRequest { stream: stream.clone(), wait_ms };
        let reply_by = remaining.min(patience.saturating_add(Duration::from_secs(1)));
        let call_ms = u64::try_from(reply_by.as_millis()).unwrap_or(u64::MAX).clamp(1, 30_000);
        let reply = kernel.call(pulls, model::ACTION_NEXT, resource, &pull, Some(call_ms))?;
        if clock() >= give_up {
            // A completion that lands after the deadline does not count.
            return Ok(Err(quit(kernel, late())?));
        }
        let next = match reply.and_then(|p| p.decode::<model::NextResponse>()) {
            Ok(next) => next,
            Err(e) => return Ok(Err(quit(kernel, e)?)),
        };
        if let Some(Err(e)) = next.queue.map(|q| q.validate()) {
            return Ok(Err(quit(kernel, e)?));
        }
        if queue.is_some() && next.queue.is_none() {
            // Leaving the queue starts a new silence interval; an ordinary
            // empty pull never does.
            last_change = clock();
        }
        queue = next.queue;
        queue_until = queue.map(|q| (clock() + Duration::from_millis(q.valid_for_ms)).min(give_up));
        if next.produced > produced {
            produced = next.produced;
            last_change = clock();
            first_byte_ms.get_or_insert_with(|| since(opened));
        }
        progress(Progress { waited_ms: since(opened), produced, first_byte_ms, queue });
        if queue.is_some_and(|q| q.valid_for_ms == 0) {
            let why = "the provider's queue lease expired; the stream was cancelled";
            return Ok(Err(quit(kernel, Error::new(ErrorCode::Timeout, why))?));
        }
        if let Some(done) = next.done {
            return Ok(Ok(done));
        }
    }
}

/// The error a stopped stream ends with. Only an acknowledged release
/// (`released: true`) keeps `original`. Anything else — `false`, a reply
/// without `released`, no answer — means the provider may still be running and
/// billing the call: its outcome is unknown, and nothing retries it.
fn after_stream_cancel(reply: Result<nucleus_protocol::Payload, Error>, original: Error) -> Error {
    match reply.and_then(|payload| payload.decode::<model::CancelResponse>()) {
        Ok(response) if response.released => original,
        _ => Error::new(
            ErrorCode::UnknownOutcome,
            format!(
                "{}; the provider did not confirm that the stream was released, so the call's outcome \
                 and cost are unknown and it is not retried",
                original.message
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
