//! A model call whose failure another attempt may not meet is made again.
//!
//! `--model-retries` (default 3) and `--model-retry-backoff-ms` (default 2 s),
//! the prover's defaults: a call that failed as a service error, an
//! unavailable provider or a full queue is made again, at most that many
//! times per model step, after a fixed wait that termination interrupts.
//! Every attempt is a model call the kernel charges and a step of the turn,
//! so neither budget grows. Nothing else is retried: a timeout is the configured wait for one
//! request running out, and another attempt would only extend it; an unknown
//! outcome may still be running; a rejected request, a revocation or a spent
//! budget will not change.
//!
//! A failed call ran no tool. Tools run only from a complete response, and
//! nothing of a failed one enters the model's history, so the next attempt
//! asks exactly what the failed one asked. What the failed call already showed
//! (streamed pieces of text and reasoning) is withdrawn under its message id,
//! so the answer of record comes from the attempt that succeeds.

use std::time::{Duration, Instant};

use nucleus_protocol::ErrorCode;
use nucleus_sdk::{ChannelError, Event};

pub(crate) fn retryable(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::ServiceError | ErrorCode::Unavailable | ErrorCode::QueueFull)
}

impl crate::Agent<'_> {
    /// Withdraws what the failed call showed, says that another attempt
    /// follows, and waits the backoff. Termination ends the wait early; the
    /// turn loop sees it before it makes the call.
    pub(crate) fn before_retry(
        &mut self,
        failure: &str,
        attempt: u32,
        shown: Option<&str>,
        reasoning_shown: bool,
    ) -> Result<(), ChannelError> {
        if let Some(id) = shown {
            if reasoning_shown {
                self.post_reasoning("", id)?;
            }
            self.post_assistant("", Some(id.to_owned()))?;
        }
        let backoff = self.options.model_retry_backoff_ms;
        eprintln!(
            "nucleus-chat-agent: {failure}; retrying ({attempt} of {}) in {backoff} ms",
            self.options.model_retries
        );
        let until = Instant::now() + Duration::from_millis(backoff);
        loop {
            let remaining = until.saturating_duration_since(Instant::now());
            if self.kernel.terminate_requested() || remaining.is_zero() {
                return Ok(());
            }
            match self.kernel.next_event_within(remaining)? {
                None | Some(Event::Terminate) => return Ok(()),
                // What any other wait of this agent does with them.
                Some(event) => self.plugins.event(&mut self.kernel, event)?,
            }
        }
    }
}
