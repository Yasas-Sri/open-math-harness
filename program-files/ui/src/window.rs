//! One window's connection: its request lines in, its answer lines out.
//!
//! The same for a stream the platform opened ([`crate::link`]) and for a
//! development socket ([`serve_socket`]): each request line is parsed, signed
//! with the connection's label ([`crate::fill`]) and handed to the kernel's
//! thread without waiting for its answer, so a parked `conversation.wait`
//! does not hold up the requests behind it; answers go back as they come,
//! matched by `id`. A window is a task and a channel: there is no limit on
//! windows.
//!
//! Bounds: a request line is at most [`MAX_REQUEST_BYTES`];
//! a longer one is answered with an error and ends the connection. An answer
//! line is at most [`MAX_ANSWER_BYTES`]; a longer answer is replaced by the
//! error `limit_exceeded` for its request, never cut. One kind of answer may
//! be longer: a `conversation.history` page, up to [`MAX_HISTORY_ANSWER_BYTES`].
//! An ordinary page is under 1 MiB with its envelope (the conversation
//! service's page target leaves the room), but the service answers a single
//! record larger than a page on its own, and refusing that page would leave
//! the session unreadable past the record; so a history answer gets the bound
//! the conversation service gives a page. A window may have
//! [`MAX_IN_FLIGHT`] requests unanswered; one more is answered at once with
//! `limit_exceeded`, and a window that keeps sending while it reads none of
//! its answers is not read until it does.

use std::sync::Arc;

use nucleus_protocol::{Error, ErrorCode};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use nucleus_conversation_interface as conversation;

use crate::access::MAX_LABEL_BYTES;
use crate::serve::{Inbox, Job, invalid, request_id, respond};
use crate::{Operation, Peer, Request, fill};

/// Longest request line, in bytes: room for a message with its bounded
/// attachments, even escaped.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Longest answer line, in bytes.
pub const MAX_ANSWER_BYTES: usize = 1024 * 1024;
/// Requests one window may have unanswered.
pub const MAX_IN_FLIGHT: usize = 48;
/// The most an answer's envelope adds to its result:
/// `{"id":18446744073709551615,"ok":` and `}`.
const ENVELOPE_BYTES: usize = 64;
/// Longest `conversation.history` answer line: the most the conversation
/// service puts in a page (module doc).
pub const MAX_HISTORY_ANSWER_BYTES: usize = conversation::MAX_HISTORY_PAYLOAD_BYTES + ENVELOPE_BYTES;
const _: () = assert!(
    conversation::HISTORY_PAGE_BYTES + ENVELOPE_BYTES <= MAX_ANSWER_BYTES,
    "an ordinary history page fits an ordinary answer line"
);

/// A window's side of the service: who it is, where its requests go, and
/// how many of its answers are not written yet.
pub(crate) struct Window {
    peer: Arc<Peer>,
    inbox: Inbox,
    answers: crate::serve::Answers,
    unwritten: usize,
    /// The ids of history requests not yet answered, each with how many: their
    /// answers have the larger bound.
    histories: std::collections::BTreeMap<u64, usize>,
}

impl Window {
    /// A window of `peer`'s, and the receiving end of its answers.
    pub fn new(peer: Peer, inbox: Inbox) -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (answers, receiver) = mpsc::unbounded_channel();
        let histories = std::collections::BTreeMap::new();
        (Self { peer: Arc::new(peer), inbox, answers, unwritten: 0, histories }, receiver)
    }

    /// One request line, without its newline: handed to the kernel's
    /// thread, or answered at once. False once the kernel's side is gone.
    pub fn request(&mut self, line: &[u8]) -> bool {
        self.unwritten += 1;
        if self.unwritten > MAX_IN_FLIGHT {
            let why = format!("a window may have {MAX_IN_FLIGHT} requests unanswered; wait for an answer");
            return self.answer(respond(request_id(line), Err(Error::new(ErrorCode::LimitExceeded, why))));
        }
        match serde_json::from_slice::<Request>(line) {
            Ok(Request { id, mut operation }) => {
                if matches!(operation, Operation::History { .. }) {
                    *self.histories.entry(id).or_default() += 1;
                }
                fill(&mut operation, &self.peer);
                let job = Job { id, operation, peer: self.peer.clone(), answer: self.answers.clone() };
                self.inbox.send(job)
            }
            // Answered by its id when the line has one, so a client newer than
            // this service hears at once that an operation is unknown.
            Err(e) => self.answer(invalid(line, e)),
        }
    }

    /// Answers a request of this window's at once.
    fn answer(&mut self, answer: Value) -> bool {
        drop(self.answers.send(answer));
        true
    }

    /// An answer as its line, within the bound of its request's kind.
    pub fn encode(&mut self, answer: &Value) -> Vec<u8> {
        let history = answer.get("id").and_then(Value::as_u64).and_then(|id| {
            let count = self.histories.get_mut(&id)?;
            *count -= 1;
            if *count == 0 {
                self.histories.remove(&id);
            }
            Some(())
        });
        encode(answer, if history.is_some() { MAX_HISTORY_ANSWER_BYTES } else { MAX_ANSWER_BYTES })
    }

    /// An answer line was written.
    pub fn written(&mut self) {
        self.unwritten = self.unwritten.saturating_sub(1);
    }

    /// Whether to read the window's next request: not while twice its
    /// in-flight bound of answers waits for it to read them.
    pub fn reading(&self) -> bool {
        self.unwritten < 2 * MAX_IN_FLIGHT
    }
}

/// An answer as its line, newline included: the answer, or when it is longer
/// than `max` the error `limit_exceeded` for its request.
pub(crate) fn encode(answer: &Value, max: usize) -> Vec<u8> {
    let mut line = serde_json::to_vec(answer).unwrap_or_default();
    if line.len() > max {
        let why = format!(
            "the answer is {} bytes, more than the {max} an answer line may have; ask for less",
            line.len()
        );
        let id = answer.get("id").cloned().unwrap_or(Value::Null);
        line = serde_json::to_vec(&json!({"id": id, "error": Error::new(ErrorCode::LimitExceeded, why)}))
            .unwrap_or_default();
    }
    line.push(b'\n');
    line
}

/// What reading a line found.
pub(crate) enum Line {
    /// A whole line, without its newline.
    Whole(Vec<u8>),
    /// A line longer than the bound: the connection ends.
    TooLong,
    /// The other side closed.
    End,
}

/// Reads one line of at most `max` bytes.
pub(crate) async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), max: usize) -> std::io::Result<Line> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(Line::End);
        }
        let (taken, done) = match available.iter().position(|&b| b == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        line.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if line.len() > max + usize::from(done) {
            return Ok(Line::TooLong);
        }
        if done {
            line.pop();
            return Ok(Line::Whole(line));
        }
    }
}

/// Serves windows on a Unix socket, a development front end's: each
/// connection a task. With `labelled` every connection's first line is its
/// label; without, every connection is the team's owner's.
pub(crate) async fn serve_socket(listener: UnixListener, labelled: bool, inbox: Inbox) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => drop(tokio::spawn(socket_window(stream, labelled, inbox.clone()))),
            Err(e) => {
                eprintln!("nucleus-ui: accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// One window on a Unix socket, until either side ends it.
pub(crate) async fn socket_window(stream: UnixStream, labelled: bool, inbox: Inbox) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let peer = if labelled {
        let label = match read_line(&mut reader, MAX_LABEL_BYTES).await {
            Ok(Line::Whole(line)) => String::from_utf8(line).map_err(|_| "the label is not UTF-8".to_owned()),
            Ok(Line::TooLong) => Err(format!("the label is longer than {MAX_LABEL_BYTES} bytes")),
            Ok(Line::End) | Err(_) => return,
        };
        match label.and_then(|label| Peer::from_label(&label)) {
            Ok(peer) => peer,
            Err(why) => {
                let refusal = respond(None, Err(Error::new(ErrorCode::InvalidInput, why)));
                drop(write.write_all(&encode(&refusal, MAX_ANSWER_BYTES)).await);
                return;
            }
        }
    } else {
        Peer::unlabelled()
    };
    let (mut window, mut answers) = Window::new(peer, inbox);
    let (lines_in, mut lines) = mpsc::channel::<Line>(1);
    let reading = tokio::spawn(async move {
        loop {
            let line = read_line(&mut reader, MAX_REQUEST_BYTES).await.unwrap_or(Line::End);
            let last = !matches!(line, Line::Whole(_));
            if lines_in.send(line).await.is_err() || last {
                return;
            }
        }
    });
    loop {
        tokio::select! {
            line = lines.recv(), if window.reading() => match line {
                Some(Line::Whole(line)) => {
                    if !window.request(&line) {
                        break;
                    }
                }
                Some(Line::TooLong) => {
                    let why = format!("a request line is at most {MAX_REQUEST_BYTES} bytes");
                    let refusal = respond(None, Err(Error::new(ErrorCode::InvalidInput, why)));
                    drop(write.write_all(&encode(&refusal, MAX_ANSWER_BYTES)).await);
                    break;
                }
                Some(Line::End) | None => break,
            },
            Some(answer) = answers.recv() => {
                if write.write_all(&window.encode(&answer)).await.is_err() {
                    break;
                }
                window.written();
            }
        }
    }
    reading.abort();
}

#[cfg(test)]
mod tests;
