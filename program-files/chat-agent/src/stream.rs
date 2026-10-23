//! A streamed completion while the model provider holds it back for a free
//! slot, and when it ends before its answer.
//!
//! While the provider holds the call back, a provisional note under an id of
//! its own says so; it is withdrawn when the wait ends, however it ends. A
//! stream that ends early is cancelled, and unless the provider confirms that
//! its reader and connection are gone, the call's outcome is unknown: the
//! provider may still finish it and charge for it.

use nucleus_conversation_interface::Entry;
use nucleus_model_interface as model;
use nucleus_protocol::{Error, ErrorCode, HandleId};
use nucleus_sdk::ChannelError;

/// Shown while the provider holds a call back for a free slot.
const WAITING_NOTE: &str = "Waiting for a free model slot…";

impl crate::Agent<'_> {
    /// Shows the waiting note while `next` says the provider holds the call
    /// back, and withdraws it once it does not.
    pub(crate) fn note_waiting(
        &mut self,
        next: &model::NextResponse,
        waiting: &mut Option<String>,
    ) -> Result<(), ChannelError> {
        // A lease longer than the interface allows is not trusted.
        let queued = next.queue.is_some_and(|lease| lease.validate().is_ok() && lease.valid_for_ms > 0);
        if !queued {
            self.stop_waiting(waiting)?;
        } else if waiting.is_none() {
            let id = self.next_message_id();
            self.post(&Entry::AssistantDelta { message_id: id.clone(), text: WAITING_NOTE.into() })?;
            *waiting = Some(id);
        }
        Ok(())
    }

    /// Withdraws the waiting note, if one is shown: an empty answer of record
    /// under its id replaces it, so neither the page nor `history` keeps it.
    pub(crate) fn stop_waiting(&mut self, waiting: &mut Option<String>) -> Result<(), ChannelError> {
        match waiting.take() {
            Some(id) => self.post(&Entry::Assistant {
                text: String::new(),
                message_id: Some(id),
                images: Vec::new(),
                images_refused: Vec::new(),
            }),
            None => Ok(()),
        }
    }

    /// Cancels the stream and says whether the provider confirmed that its
    /// reader and connection for it are gone.
    pub(crate) fn cancel_stream(&mut self, pulls: HandleId, stream: &str) -> Result<bool, ChannelError> {
        let cancel = model::CancelRequest { stream: stream.into() };
        let reply = self.kernel.call(
            pulls,
            model::ACTION_CANCEL,
            &self.model_resource,
            &cancel,
            Some(crate::CANCEL_DEADLINE_MS),
        )?;
        // Anything but an explicit `released: true` (a refusal, an error, a
        // reply that does not decode) leaves the stream possibly running.
        Ok(reply.and_then(|payload| payload.decode::<model::CancelResponse>()).is_ok_and(|r| r.released))
    }

    /// The error of a call whose stream was cancelled: as it is when the
    /// provider confirmed the release, and an unknown outcome when it did not,
    /// since the provider may still finish and charge for the call. Neither is
    /// a reason to replay a tool or a turn.
    pub(crate) fn unconfirmed(error: Error, released: bool) -> Error {
        if released {
            return error;
        }
        Error::new(
            ErrorCode::UnknownOutcome,
            format!(
                "{}; the provider did not confirm that the call stopped, so whether it finished or was \
                 charged is unknown",
                error.message
            ),
        )
    }
}
