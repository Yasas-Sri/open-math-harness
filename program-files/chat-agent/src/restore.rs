//! A session's agent continues from its conversation (conversation
//! interface, "Continuing a conversation"): every start, whatever ended the
//! agent before it (an interrupt, a stop, a change of model, a runtime
//! update, a crash), rebuilds what its model saw from the session's log
//! before it takes the next message.
//!
//! The log is read with `history` on the agent's own session, a page at a
//! time as the window reads it, with the `delivered` markers. What the model
//! saw is rebuilt as closely as the log allows:
//!
//! - The messages of one `receive` at the point they were delivered, framed
//!   as a received message is (`reminders::queued_prompt`): author lines,
//!   the one-time instruction when that author turns Ultramath off, inline
//!   attachments, the images a message carries, shown again from the log when
//!   the model takes images, each attached file named by its path,
//!   its content not read again (`attachments::replayed`). Those the agent took between
//!   two steps of a turn are in that turn, after the step's results. A
//!   message never delivered, waiting still or withdrawn, is not replayed;
//!   one waiting arrives through `receive` as usual. A delivered reminder in
//!   its untrusted frame.
//! - The agent's own text and tool calls, each call with the result the
//!   conversation kept (64 KiB; a longer one says it was cut), and where the
//!   turn went on after a response the agent's own continuation of it, which
//!   the log does not keep but the response implies: a response cut at the
//!   output limit is followed by the length continuation, one with neither
//!   text nor a tool call by the nudge (`crate::NUDGE`). A response whose
//!   record names images the person was not shown is followed, after its
//!   calls' results, by the note on them (module `reply_images`); the images
//!   it showed were the person's and are not the model's.
//! - A turn that ended without `turn_complete`: each call without a result
//!   gets one saying it did not return and that its outcome is unknown, and a
//!   note says when the turn was stopped. Nothing is run again.
//! - From the last compaction on: its summary, as the agent that summarised
//!   handed it on.
//!
//! Left out: reasoning, streamed pieces, errors and bookkeeping, which the
//! model never read; and what any other process posted, the swarm service
//! (the `posted_by` stamps), and every record outside a turn.
//!
//! What is replayed carries no authority: the agent holds only what it was
//! granted at this start, and the replayed text, calls, results and
//! summaries are model and tool output, untrusted as when they were produced.
//!
//! The history is bounded by the compaction threshold, estimated at
//! [`BYTES_PER_TOKEN`] bytes a token since no provider has counted it yet.
//! The oldest messages beyond it are left out, with a note saying so, and
//! then the history is summarised before the first model call. What is held
//! while reading is that bound, the page being read, and the messages and
//! reminders waiting for delivery, which the conversation service bounds.
//!
//! A swarm member started again on its transcript (`--resume`) is not rebuilt
//! from a log: its transcript holds the messages as its model saw them. One
//! stopped while a call ran (a restart interrupted its swarm) left that call
//! without a result, and [`transcript`] gives it one the same way. Either way no
//! model request carries a call without its result, which strict providers
//! refuse (DeepSeek answers 400). A transcript is held to the same bound as a
//! log: the oldest exchanges beyond it are left out, with the note, and the
//! rest is summarised before the first model call.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;

use nucleus_conversation_interface::{
    self as conversation, MAX_PENDING_REMINDERS, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES, POSTED_BY,
    RECEIVED_BY, RECORD_MESSAGE_WITHDRAWN, RECORD_REMINDER_CANCELLED, RECORD_REMINDER_FIRED,
    RECORD_REMINDER_SET, ReceiveResponse, RefusedImage, Reminder, UserMessage,
};
use nucleus_model_interface::{Message, Role, ToolCall};
use nucleus_protocol::{HandleId, MAX_FRAME_BYTES, ResourcePath};
use serde_json::{Value, json};

use crate::images::Held;

/// Bytes a token is assumed to take in a history no provider has counted:
/// few, so a rebuilt history is counted high rather than low. CJK text runs
/// close to it; English and code take more bytes a token.
const BYTES_PER_TOKEN: usize = 3;

/// Where the oldest messages since the last summary were left out.
const LEFT_OUT: &str = "[The start of this conversation is left out here: with it, the conversation is \
    longer than this agent keeps. Everything outside the conversation, the files included, is as it was \
    left.]";

/// What a start of the agent begins with.
#[derive(Debug, Default)]
pub(crate) struct Restored {
    /// The model's history, oldest first.
    pub(crate) messages: Vec<Message>,
    /// The images the messages carry, by the index of the message in
    /// `messages`, in order.
    pub(crate) images: Vec<(usize, Vec<Held>)>,
    /// Who has written and their last delivered mode (`reminders::queued_prompt`).
    pub(crate) authors: BTreeMap<Option<String>, nucleus_model_interface::ReasoningMode>,
    /// How full the context is taken to be until the first model call
    /// counts it: the estimate, or the threshold when messages were left
    /// out, so the history is summarised before that call.
    pub(crate) context_tokens: u64,
}

/// Reads the session's log through `history` with `handle` and rebuilds
/// what the model saw from it; `image_input`: whether the model takes images.
pub(crate) fn read(
    kernel: &mut crate::Kernel,
    handle: HandleId,
    session: &ResourcePath,
    compact_at_tokens: u64,
    image_input: bool,
) -> Result<Restored, String> {
    let mut rebuild = Rebuild::new(compact_at_tokens, image_input);
    let mut after = None;
    loop {
        let request = conversation::HistoryRequest { after_seq: after };
        let reply = kernel
            .call(handle, conversation::ACTION_HISTORY, session, &request, None)
            .map_err(|e| e.to_string())?;
        let page = reply
            .and_then(|payload| payload.decode::<conversation::HistoryResponse>())
            .and_then(|page| Ok((page.continuation(after)?, page.records)))
            .map_err(|e| format!("the session's conversation could not be read: {e}"))?;
        for record in &page.1 {
            rebuild.push(record);
        }
        match page.0 {
            Some(seq) => after = Some(seq),
            None => return Ok(rebuild.finish()),
        }
    }
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// `turn_complete`: the agent finished it.
    Complete,
    /// `interrupted`, at this time: the session's agent was stopped.
    Stopped(u64),
    /// Nothing ended it: the agent working on it stopped (it crashed, or the
    /// machine did) after its last record, at this time.
    Abandoned(u64),
    /// A transcript ends inside it: the agent stopped while its calls ran, at
    /// a time the transcript does not keep ([`transcript`]).
    Transcript,
}

/// The deliveries of one `receive`: one dispatch, one time, one receiver.
#[derive(Debug)]
struct Batch {
    at_ms: u64,
    receiver: u64,
    messages: Vec<UserMessage>,
    reminders: Vec<Reminder>,
}

/// One model response: its text and its calls, each with its result.
#[derive(Debug, Default)]
struct Step {
    text: String,
    message_id: Option<String>,
    calls: Vec<(ToolCall, Option<String>)>,
    /// The images its text referenced that the person was not shown.
    refused: Vec<RefusedImage>,
    /// How the model ended it, as its `model_call` record says (`length`,
    /// `stop`, `toolcalls`).
    finish: Option<String>,
}

/// The turn a batch opened.
#[derive(Debug, Default)]
struct Turn {
    /// The process that received the turn's messages, whose posts are the
    /// turn's.
    agent: u64,
    step: Option<Step>,
    /// When its last record was written.
    last_at_ms: u64,
}

/// The rebuild, one record at a time (module doc).
#[derive(Debug)]
pub(crate) struct Rebuild {
    compact_at_tokens: u64,
    /// Bytes the history may hold.
    budget: usize,
    /// Where the history starts: the last compaction's resume message.
    head: Option<String>,
    /// What came after `head`, in units kept or left out whole: a prompt, a
    /// response with its calls' results, a note; each with the images its
    /// first message carries.
    groups: VecDeque<(Vec<Message>, Vec<Held>)>,
    /// Whether the model takes images: the images messages carry are shown.
    image_input: bool,
    /// Bytes of `head` and `groups`, as the request carries them.
    bytes: usize,
    left_out: bool,
    /// Messages said and not delivered, with their sizes; a withdrawn one
    /// leaves.
    said: VecDeque<(UserMessage, usize)>,
    /// Reminders set and neither fired nor cancelled.
    reminders: Vec<Reminder>,
    batch: Option<Batch>,
    turn: Option<Turn>,
    authors: BTreeMap<Option<String>, nucleus_model_interface::ReasoningMode>,
}

fn size(message: &Message) -> usize {
    serde_json::to_vec(message).map_or(0, |json| json.len())
}

impl Rebuild {
    pub(crate) fn new(compact_at_tokens: u64, image_input: bool) -> Self {
        let threshold = usize::try_from(compact_at_tokens).unwrap_or(usize::MAX);
        Self {
            compact_at_tokens,
            // The frame bound leaves room for the instructions, the tools,
            // the next message and its images.
            budget: threshold.saturating_mul(BYTES_PER_TOKEN).min(MAX_FRAME_BYTES / 2),
            head: None,
            groups: VecDeque::new(),
            image_input,
            bytes: 0,
            left_out: false,
            said: VecDeque::new(),
            reminders: Vec::new(),
            batch: None,
            turn: None,
            authors: BTreeMap::new(),
        }
    }

    /// Takes the next record of the log, in order.
    pub(crate) fn push(&mut self, record: &Value) {
        let kind = record["kind"].as_str().unwrap_or_default();
        let at_ms = record["at_ms"].as_u64().unwrap_or(0);
        let id = record["id"].as_str().unwrap_or_default();
        let receiver = record[RECEIVED_BY].as_u64();
        let delivery = matches!(kind, "delivered" | RECORD_REMINDER_FIRED);
        // One `receive` writes its markers together, at one time, for one receiver.
        if self.batch.as_ref().is_some_and(|b| !delivery || b.at_ms != at_ms || Some(b.receiver) != receiver)
        {
            self.open_turn();
        }
        match kind {
            "user" => self.say(record),
            RECORD_MESSAGE_WITHDRAWN => self.said.retain(|(message, _)| message.id != id),
            "delivered" => {
                let Some(receiver) = receiver else { return };
                let message =
                    self.said.iter().position(|(m, _)| m.id == id).and_then(|at| self.said.remove(at));
                self.batch_at(at_ms, receiver).messages.extend(message.map(|(message, _)| message));
            }
            RECORD_REMINDER_SET => {
                if let Ok(reminder) = reminder(record) {
                    if self.reminders.len() >= MAX_PENDING_REMINDERS {
                        self.reminders.remove(0);
                    }
                    self.reminders.push(reminder);
                }
            }
            RECORD_REMINDER_FIRED => {
                let Some(receiver) = receiver else { return };
                let reminder =
                    self.reminders.iter().position(|r| r.id == id).map(|at| self.reminders.remove(at));
                self.batch_at(at_ms, receiver).reminders.extend(reminder);
            }
            RECORD_REMINDER_CANCELLED => self.reminders.retain(|reminder| reminder.id != id),
            // The sessions service's: it stopped the agent.
            "interrupted" => self.end_turn(End::Stopped(at_ms)),
            _ => self.posted(kind, record, at_ms),
        }
    }

    /// What the rebuild comes to once the log is read.
    pub(crate) fn finish(mut self) -> Restored {
        self.open_turn();
        if let Some(at) = self.turn.as_ref().map(|turn| turn.last_at_ms) {
            self.end_turn(End::Abandoned(at));
        }
        let opening: Vec<&str> =
            self.head.as_deref().into_iter().chain(self.left_out.then_some(LEFT_OUT)).collect();
        let (mut messages, mut images) = (Vec::new(), Vec::new());
        if !opening.is_empty() {
            messages.push(Message::text(Role::User, opening.join("\n\n")));
        }
        for (group, held) in self.groups {
            if !held.is_empty() {
                images.push((messages.len(), held));
            }
            messages.extend(group);
        }
        let estimate = messages.iter().map(size).sum::<usize>() / BYTES_PER_TOKEN;
        let context_tokens =
            if self.left_out { self.compact_at_tokens } else { u64::try_from(estimate).unwrap_or(u64::MAX) };
        Restored { messages, images, authors: self.authors, context_tokens }
    }

    /// A message was said: it waits for a `receive`.
    fn say(&mut self, record: &Value) {
        let Ok(message) = serde_json::from_value::<UserMessage>(record.clone()) else { return };
        let bytes = serde_json::to_vec(&message).map_or(0, |json| json.len());
        self.said.push_back((message, bytes));
        // A restored queue is held to the bounds the service holds.
        while self.said.len() > MAX_QUEUED_MESSAGES
            || self.said.iter().map(|(_, bytes)| bytes).sum::<usize>() > MAX_QUEUED_BYTES
        {
            self.said.pop_front();
        }
    }

    fn batch_at(&mut self, at_ms: u64, receiver: u64) -> &mut Batch {
        self.batch.get_or_insert_with(|| Batch {
            at_ms,
            receiver,
            messages: Vec::new(),
            reminders: Vec::new(),
        })
    }

    /// The turn the batch just read opens: its prompt, as the agent made it.
    /// A batch the open turn's own agent took came between two of its steps
    /// (conversation interface, "Between steps") and goes on in that turn; any
    /// other means the open turn did not finish: its agent stopped before
    /// this one took the batch.
    fn open_turn(&mut self) {
        let Some(batch) = self.batch.take() else { return };
        let within = self.turn.as_ref().is_some_and(|turn| turn.agent == batch.receiver);
        if within {
            self.went_on();
            self.flush(End::Complete);
        } else if let Some(at) = self.turn.as_ref().map(|turn| turn.last_at_ms) {
            self.end_turn(End::Abandoned(at));
        }
        let mut messages = batch.messages.into_iter();
        let received = ReceiveResponse {
            message: messages.next(),
            rest: messages.collect(),
            reminders: batch.reminders,
        };
        let (image_input, mut shown) = (self.image_input, Vec::new());
        let prompt = crate::reminders::queued_prompt(received, &mut self.authors, |message| {
            let images = crate::attachments::sent_images(&message, image_input);
            let text = crate::attachments::replayed(&message, &images);
            shown.extend(images.into_iter().flatten());
            Ok::<_, Infallible>(text)
        });
        if let Ok(Some(prompt)) = prompt {
            self.keep_with(vec![Message::text(Role::User, prompt)], shown);
        }
        match self.turn.as_mut().filter(|_| within) {
            Some(turn) => turn.last_at_ms = batch.at_ms,
            None => {
                self.turn = Some(Turn { agent: batch.receiver, last_at_ms: batch.at_ms, ..Turn::default() });
            }
        }
    }

    /// A record some process posted: the turn's when its agent posted it.
    fn posted(&mut self, kind: &str, record: &Value, at_ms: u64) {
        let Some(turn) = self.turn.as_mut() else { return };
        if record[POSTED_BY].as_u64() != Some(turn.agent) {
            return;
        }
        turn.last_at_ms = at_ms;
        let text = |field: &str| record[field].as_str().unwrap_or_default();
        match kind {
            "turn_complete" => self.end_turn(End::Complete),
            // A response begins: the one before it is whole, and the turn went on.
            "model_call" => {
                self.went_on();
                let finish = record["finish_reason"].as_str().map(str::to_owned);
                self.step().finish = finish;
            }
            "assistant" => {
                let refused = serde_json::from_value(record["images_refused"].clone()).unwrap_or_default();
                self.assistant(text("text"), record["message_id"].as_str(), refused);
            }
            "tool_call" => {
                let call = ToolCall {
                    id: text("call_id").to_owned(),
                    name: text("name").to_owned(),
                    arguments: record["arguments"].clone(),
                };
                self.step().calls.push((call, None));
            }
            "tool_result" => {
                let call_id = text("call_id");
                let open = self
                    .step()
                    .calls
                    .iter_mut()
                    .find(|(call, result)| call.id == call_id && result.is_none());
                if let Some((_, result)) = open {
                    *result = Some(kept_result(record));
                }
            }
            "compacted" => {
                if let Some(summary) = record["summary"].as_str() {
                    self.compacted(summary);
                }
            }
            // Reasoning, streamed pieces, errors, swarm records: shown to the
            // person, never read by the model.
            _ => {}
        }
    }

    /// The response being read; only called within a turn.
    fn step(&mut self) -> &mut Step {
        self.turn.get_or_insert_with(Turn::default).step.get_or_insert_with(Step::default)
    }

    /// Text of a response. One longer than an entry holds was posted in
    /// pieces under one message id; text under another id joins it, a blank
    /// line between. Text after a response's calls is a message of its own. `refused`: the
    /// images it referenced that the person was not shown.
    fn assistant(&mut self, text: &str, id: Option<&str>, refused: Vec<RefusedImage>) {
        if text.is_empty() {
            return;
        }
        if self.turn.as_ref().and_then(|turn| turn.step.as_ref()).is_some_and(|step| !step.calls.is_empty()) {
            self.flush(End::Complete);
        }
        let step = self.step();
        if !step.message_id.as_deref().zip(id).is_some_and(|(base, id)| chunk_of(base, id)) {
            if !step.text.is_empty() {
                step.text.push_str("\n\n");
            }
            step.message_id = id.map(str::to_owned);
        }
        step.text.push_str(text);
        step.refused.extend(refused);
    }

    /// The conversation was summarised: the history starts again from the
    /// summary, as the agent's did.
    fn compacted(&mut self, summary: &str) {
        self.flush(End::Complete);
        let head = nucleus_prompts::compact::resume_message(summary);
        self.bytes = size(&Message::text(Role::User, head.as_str()));
        self.head = Some(head);
        self.groups.clear();
        self.left_out = false;
    }

    /// The current response is whole and the turn went on after it: the
    /// response, then what the agent told the model next, which the log does
    /// not keep but the response implies (the turn loop of `Agent::answer`):
    /// the length continuation after a response cut at the output limit, the
    /// nudge after one with neither text nor a tool call.
    fn went_on(&mut self) {
        let Some(step) = self.turn.as_ref().and_then(|turn| turn.step.as_ref()) else { return };
        let next = if step.finish.as_deref() == Some("length") {
            Some(crate::LENGTH_CONTINUATION)
        } else {
            (step.text.trim().is_empty() && step.calls.is_empty()).then_some(crate::NUDGE)
        };
        self.flush(End::Complete);
        if let Some(next) = next {
            self.keep(vec![Message::text(Role::User, next)]);
        }
    }

    /// The current response as the model's history holds it, the turn not
    /// going on after it (or not yet known to).
    fn flush(&mut self, end: End) {
        if let Some(step) = self.turn.as_mut().and_then(|turn| turn.step.take()) {
            self.keep_step(step, end);
        }
    }

    /// A response as the model's history holds it: the text and calls, then
    /// each call's result, or one saying it has none because of how the turn
    /// ended, then the note on the images the person was not shown.
    fn keep_step(&mut self, step: Step, end: End) {
        if step.text.trim().is_empty() && step.calls.is_empty() {
            return;
        }
        let calls: Vec<ToolCall> = step.calls.iter().map(|(call, _)| call.clone()).collect();
        let mut group = vec![Message { tool_calls: calls, ..Message::text(Role::Assistant, step.text) }];
        group.extend(step.calls.into_iter().map(|(call, result)| answer(call, result, end)));
        group.extend(crate::reply_images::note(&step.refused).map(|note| Message::text(Role::User, note)));
        self.keep(group);
    }

    fn end_turn(&mut self, end: End) {
        if self.turn.is_none() {
            return;
        }
        self.flush(end);
        self.turn = None;
        let note = match end {
            End::Complete | End::Transcript => return,
            End::Stopped(at) => format!(
                "[The turn above was stopped at {}, before it finished: what it did is above. A call that \
                 had not returned has a result saying so; whether it took effect is unknown.]",
                utc(at)
            ),
            End::Abandoned(at) => format!(
                "[The turn above did not finish: the agent working on it stopped after {}, without a \
                 record of why. What it did is above. A call that had not returned has a result saying \
                 so; whether it took effect is unknown.]",
                utc(at)
            ),
        };
        self.keep(vec![Message::text(Role::User, note)]);
    }

    /// Adds a group, leaving the oldest out while the history is over its
    /// bound. The newest group is always kept.
    fn keep(&mut self, group: Vec<Message>) {
        self.keep_with(group, Vec::new());
    }

    /// [`Self::keep`], with the images the group's first message carries.
    fn keep_with(&mut self, group: Vec<Message>, images: Vec<Held>) {
        self.bytes += group.iter().map(size).sum::<usize>();
        self.groups.push_back((group, images));
        while self.bytes > self.budget && self.groups.len() > 1 {
            if let Some((old, _)) = self.groups.pop_front() {
                self.bytes -= old.iter().map(size).sum::<usize>();
                self.left_out = true;
            }
        }
    }
}

/// Whether `id` is the message id of a later piece of the text posted as
/// `base` (`final_chunk_id`).
fn chunk_of(base: &str, id: &str) -> bool {
    id == base
        || id
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix(":chunk:"))
            .is_some_and(|n| n.parse::<usize>().is_ok())
}

/// A reminder as its `reminder_set` record holds it.
fn reminder(record: &Value) -> Result<Reminder, serde_json::Error> {
    Ok(Reminder {
        id: serde_json::from_value(record["id"].clone())?,
        text: serde_json::from_value(record["text"].clone())?,
        source: serde_json::from_value(record["source"].clone())?,
        set_at_ms: record["at_ms"].as_u64().unwrap_or(0),
        due_at_ms: serde_json::from_value(record["due_at_ms"].clone())?,
    })
}

/// A call's result as the conversation kept it: the tool's reply, or the
/// error as the model was shown it; one the conversation kept less of than
/// the model got says so.
fn kept_result(record: &Value) -> String {
    let summary = record["summary"].as_str().unwrap_or_default();
    let summary = match summary.rfind(crate::RESULT_SHORTENED) {
        None => summary.to_owned(),
        Some(at) => {
            let total = summary[at + crate::RESULT_SHORTENED.len()..].split(' ').next().unwrap_or("?");
            format!(
                "{}\n[cut: the conversation kept the first {at} of this result's {total} bytes; the rest is \
                 not available now]",
                &summary[..at]
            )
        }
    };
    match record.get("error_code").filter(|code| !code.is_null()) {
        None => summary,
        Some(code) => json!({"error": {"code": code, "message": summary}}).to_string(),
    }
}

/// The message answering `call`: its `result`, or, when it has none, one
/// saying so because of how its turn ended (`end`).
fn answer(call: ToolCall, result: Option<String>, end: End) -> Message {
    let content = result.unwrap_or_else(|| missing_result(end));
    Message { tool_call_id: Some(call.id), ..Message::text(Role::Tool, content) }
}

/// What a start continuing a transcript (`--resume`) begins with: its
/// messages, each with its images, every call that has no result answered by
/// one saying it did not return ([`End::Transcript`]) after the results its
/// response has, and the history bounded as a rebuilt log's is ([`Rebuild`]):
/// a message and the results after it are kept or left out together. Nothing
/// is run again.
pub(crate) fn transcript(
    lines: Vec<(Message, Vec<Held>)>,
    compact_at_tokens: u64,
    image_input: bool,
) -> Restored {
    let mut rebuild = Rebuild::new(compact_at_tokens, image_input);
    let (mut group, mut images) = (Vec::new(), Vec::new());
    // The calls of the last response that no result has answered yet.
    let mut open: Vec<ToolCall> = Vec::new();
    for (message, held) in lines {
        if message.role == Role::Tool {
            open.retain(|call| message.tool_call_id.as_ref() != Some(&call.id));
            group.push(message);
            continue;
        }
        group.extend(open.drain(..).map(|call| answer(call, None, End::Transcript)));
        if !group.is_empty() {
            rebuild.keep_with(std::mem::take(&mut group), std::mem::take(&mut images));
        }
        if message.role == Role::Assistant {
            open.clone_from(&message.tool_calls);
        }
        group.push(message);
        images = held;
    }
    group.extend(open.into_iter().map(|call| answer(call, None, End::Transcript)));
    if !group.is_empty() {
        rebuild.keep_with(group, images);
    }
    rebuild.finish()
}

/// The result of a call that has none in the log.
fn missing_result(end: End) -> String {
    let why = match end {
        End::Complete => "no result of this call was recorded".to_owned(),
        End::Transcript => "this call did not return: the agent stopped while it ran".to_owned(),
        End::Stopped(at) => {
            format!("this call did not return: the turn was stopped at {} while it ran", utc(at))
        }
        End::Abandoned(at) => {
            format!("this call did not return: the agent stopped while it ran, after {}", utc(at))
        }
    };
    let message = format!("{why}. Whether it took effect is unknown; check before you run it again.");
    json!({"error": {"code": nucleus_protocol::ErrorCode::UnknownOutcome, "message": message}}).to_string()
}

/// UNIX milliseconds as `YYYY-MM-DD HH:MM:SS UTC`.
fn utc(at_ms: u64) -> String {
    let seconds = at_ms / 1000;
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    // Days since 1970-01-01 as a civil date (H. Hinnant, `civil_from_days`).
    let z = days as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", time / 3600, time % 3600 / 60, time % 60)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A session's log, as `history` reads it: each record
    /// gets `seq` and, unless it has one, `at_ms` one second after the last.
    fn rebuilt(threshold: u64, records: &[Value]) -> Restored {
        rebuilt_for(threshold, false, records)
    }

    /// [`rebuilt`] for a model that takes images, or none.
    fn rebuilt_for(threshold: u64, image_input: bool, records: &[Value]) -> Restored {
        let mut rebuild = Rebuild::new(threshold, image_input);
        let mut at_ms = 1_790_000_000_000;
        for (seq, record) in records.iter().enumerate() {
            let mut record = record.clone();
            at_ms = record["at_ms"].as_u64().unwrap_or(at_ms + 1000);
            record["seq"] = json!(seq);
            record["at_ms"] = json!(at_ms);
            rebuild.push(&record);
        }
        rebuild.finish()
    }

    fn said(id: &str, text: &str) -> Value {
        json!({"kind": "user", "id": id, "text": text})
    }

    /// Delivered to `agent`; one `receive`'s markers share their time.
    fn delivered(id: &str, agent: u64, at_ms: u64) -> Value {
        json!({"kind": "delivered", "id": id, "received_by": agent, "at_ms": at_ms})
    }

    fn by(agent: u64, mut entry: Value) -> Value {
        entry[POSTED_BY] = json!(agent);
        entry
    }

    fn step(agent: u64) -> Value {
        ended(agent, "stop")
    }

    /// A response the model ended as `finish` says.
    fn ended(agent: u64, finish: &str) -> Value {
        let call = json!({"kind": "model_call", "step": 1, "served_by": "m", "finish_reason": finish,
                          "input_tokens": 1, "output_tokens": 1, "duration_ms": 1, "streamed": false});
        by(agent, call)
    }

    fn says(agent: u64, id: &str, text: &str) -> Value {
        by(agent, json!({"kind": "assistant", "message_id": id, "text": text}))
    }

    fn call(agent: u64, id: &str, name: &str) -> Value {
        by(agent, json!({"kind": "tool_call", "call_id": id, "name": name, "arguments": {"path": "x"}}))
    }

    fn result(agent: u64, id: &str, summary: &str) -> Value {
        by(agent, json!({"kind": "tool_result", "call_id": id, "error_code": null, "summary": summary}))
    }

    fn done(agent: u64) -> Value {
        by(agent, json!({"kind": "turn_complete"}))
    }

    /// The rebuilt history, one line per message: its role and text, then
    /// the calls it makes or the call it answers.
    fn lines(restored: &Restored) -> Vec<String> {
        let line = |m: &Message| {
            let calls: Vec<&str> = m.tool_calls.iter().map(|call| call.id.as_str()).collect();
            let calls = if calls.is_empty() { String::new() } else { format!(" calls={}", calls.join(",")) };
            let answers = m.tool_call_id.as_ref().map_or(String::new(), |id| format!(" for={id}"));
            format!("{:?} {}{calls}{answers}", m.role, m.content)
        };
        restored.messages.iter().map(line).collect()
    }

    /// An image a message carries is in the log, so a continuation shows it
    /// again after the message's text when the model takes images, and
    /// names it as not shown when it takes none.
    #[test]
    fn a_continuation_shows_the_images_a_message_carries_again() {
        let image = json!({"name": "plot.png", "media_type": "image/png", "data_base64": "iVBORw0KGgo="});
        let log = [
            json!({"kind": "user", "id": "u1", "text": "look", "images": [image]}),
            delivered("u1", 5, 1_790_000_100_000),
            step(5),
            says(5, "a1", "a plot"),
            done(5),
        ];
        let shown = rebuilt_for(1_000_000, true, &log);
        assert!(
            shown.messages[0].content.contains("- \"plot.png\" (image/png, "),
            "{}",
            shown.messages[0].content
        );
        assert!(shown.messages[0].content.ends_with("): shown below]"), "{}", shown.messages[0].content);
        assert_eq!(shown.images.iter().map(|(at, held)| (*at, held.len())).collect::<Vec<_>>(), [(0, 1)]);
        let mut history = crate::images::History::default();
        history.restore(shown.messages[0].clone(), shown.images[0].1.clone());
        history.settle(|_| Err("not read".to_owned()));
        let parts = &history.iter().next().unwrap().parts;
        assert!(parts.iter().any(|part| part.image_data() == Some("iVBORw0KGgo=")), "{parts:?}");

        let named = rebuilt_for(1_000_000, false, &log);
        assert!(named.images.is_empty());
        assert!(named.messages[0].content.contains("not shown: this session's model takes no images"));
    }

    /// What the model saw, rebuilt from what the log records, row by row:
    /// each message where it was delivered and in its frame, never one that
    /// was not; the agent's own records and no other process's; an
    /// interrupted turn's open call answered and the interruption said; a
    /// compaction's summary, or that it was not kept.
    #[test]
    fn the_history_is_what_the_model_saw_as_far_as_the_log_keeps_it() {
        let t = 1_790_000_100_000;
        type Row<'a> = (&'a str, Vec<Value>, Vec<&'a str>, Vec<&'a str>);
        let rows: Vec<Row> = vec![
            (
                "a message said during a turn is where it was delivered, after that turn",
                vec![
                    said("u1", "first question"),
                    delivered("u1", 5, t),
                    step(5),
                    said("u2", "said while it worked"),
                    says(5, "a1", "first answer"),
                    done(5),
                    delivered("u2", 5, t + 60_000),
                    step(5),
                    says(5, "a2", "second answer"),
                    done(5),
                ],
                vec![
                    "User first question",
                    "Assistant first answer",
                    "User said while it worked",
                    "Assistant second answer",
                ],
                vec![],
            ),
            (
                "a withdrawn message and one still waiting are never replayed",
                vec![
                    said("u1", "go"),
                    delivered("u1", 5, t),
                    said("u2", "withdrawn text"),
                    json!({"kind": "message_withdrawn", "id": "u2", "message_author": null}),
                    step(5),
                    says(5, "a1", "went"),
                    done(5),
                    said("u3", "still waiting"),
                ],
                vec!["User go", "Assistant went"],
                vec!["withdrawn text", "still waiting"],
            ),
            (
                "a compaction's summary is where the history starts again",
                vec![
                    said("u1", "old question"),
                    delivered("u1", 5, t),
                    step(5),
                    by(
                        5,
                        json!({"kind": "compacted", "messages": 3, "tokens": 900, "summary": "SUMMARY-17"}),
                    ),
                    step(5),
                    says(5, "a1", "answer after the summary"),
                    done(5),
                ],
                vec!["User Another model was working on this", "Assistant answer after the summary"],
                vec!["old question"],
            ),
            (
                "an interrupted turn: its open call gets a result, and the stop is said",
                vec![
                    said("u1", "read then append"),
                    delivered("u1", 5, t),
                    step(5),
                    says(5, "a1", "reading"),
                    call(5, "c1", "fs_read"),
                    result(5, "c1", "the launch code is 42"),
                    step(5),
                    call(5, "c2", "fs_append"),
                    json!({"kind": "interrupted", "posted_by": 2, "at_ms": 1_790_000_200_000_u64}),
                ],
                vec![
                    "User read then append",
                    "Assistant reading calls=c1",
                    "Tool the launch code is 42 for=c1",
                    "Assistant  calls=c2",
                    "\"unknown_outcome\",\"message\":\"this call did not return: the turn was stopped at 2026-09-21 14:16:40 UTC",
                    "User [The turn above was stopped at 2026-09-21 14:16:40 UTC",
                ],
                vec![],
            ),
            (
                "a message taken between two steps is in the turn, after the step's results",
                vec![
                    said("u1", "long job"),
                    delivered("u1", 5, t),
                    step(5),
                    call(5, "c1", "shell_exec"),
                    said("u2", "also do this"),
                    result(5, "c1", "done"),
                    delivered("u2", 5, t + 30_000),
                    step(5),
                    says(5, "a1", "did both"),
                    done(5),
                ],
                vec![
                    "User long job",
                    "Assistant  calls=c1",
                    "Tool done for=c1",
                    "User also do this",
                    "Assistant did both",
                ],
                vec!["did not finish"],
            ),
            (
                "where a turn went on, the agent's own continuation, which the log implies",
                vec![
                    said("u1", "fix it"),
                    delivered("u1", 5, t),
                    ended(5, "length"),
                    says(5, "a1", "a long first part"),
                    ended(5, "toolcalls"),
                    call(5, "c1", "fs_read"),
                    result(5, "c1", "read"),
                    step(5),
                    step(5),
                    says(5, "a2", "fixed"),
                    done(5),
                ],
                vec![
                    "User fix it",
                    "Assistant a long first part",
                    "User The previous response was truncated and is incomplete.",
                    "Assistant  calls=c1",
                    "Tool read for=c1",
                    "User Your last reply was empty",
                    "Assistant fixed",
                ],
                vec![],
            ),
            (
                "the images an answer referenced: the note on those not shown follows it, none is the model's",
                vec![
                    said("u1", "plot it"),
                    delivered("u1", 5, t),
                    step(5),
                    by(
                        5,
                        json!({"kind": "assistant", "message_id": "a1", "text": "![p](p.png) ![q](q.png)",
                               "images": [{"path": "p.png", "media_type": "image/png",
                                           "data_base64": "SHOWN64="}],
                               "images_refused": [{"path": "q.png", "reason": "not_found"}]}),
                    ),
                    done(5),
                    said("u2", "and now?"),
                    delivered("u2", 5, t + 60_000),
                    step(5),
                    says(5, "a2", "fixed"),
                    done(5),
                ],
                vec![
                    "User plot it",
                    "Assistant ![p](p.png)",
                    "show it again if it matters:\n- q.png: not_found",
                    "User and now?",
                    "Assistant fixed",
                ],
                vec!["SHOWN64=", "- p.png"],
            ),
            (
                "a turn its agent left open: the next agent's message follows a note",
                vec![
                    said("u1", "long job"),
                    delivered("u1", 5, t),
                    step(5),
                    call(5, "c1", "shell_exec"),
                    said("u2", "are you there?"),
                    delivered("u2", 8, t + 600_000),
                    step(8),
                    says(8, "b1", "yes"),
                    done(8),
                ],
                vec![
                    "User long job",
                    "Assistant  calls=c1",
                    "agent stopped while it ran",
                    "User [The turn above did not finish",
                    "User are you there?",
                    "Assistant yes",
                ],
                vec![],
            ),
            (
                "what another process posted into the session is not the agent's",
                vec![
                    said("u1", "start"),
                    delivered("u1", 5, t),
                    step(5),
                    step(6),
                    says(6, "h1", "another process's text"),
                    call(6, "h2", "fs_read"),
                    done(6),
                    says(5, "a1", "my own answer"),
                    done(5),
                ],
                vec!["User start", "Assistant my own answer"],
                vec!["another process's text", "h2"],
            ),
        ];
        for (name, records, expected, absent) in rows {
            let restored = rebuilt(800_000, &records);
            let lines = lines(&restored);
            assert_eq!(lines.len(), expected.len(), "{name}: {lines:#?}");
            for (line, wanted) in lines.iter().zip(&expected) {
                assert!(line.contains(wanted), "{name}: {wanted:?} not in {line}");
            }
            for unwanted in absent {
                assert!(
                    !lines.iter().any(|line| line.contains(unwanted)),
                    "{name}: {unwanted:?} in {lines:#?}"
                );
            }
            assert!(restored.context_tokens < 800_000, "{name}: nothing was left out");
        }
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00 UTC");
    }

    /// Beyond the compaction threshold the oldest messages are left out,
    /// with a note, and the history counts as full, so it is summarised
    /// before the next model call.
    #[test]
    fn a_history_over_the_threshold_keeps_its_newest_messages_and_is_summarised_first() {
        let mut records = Vec::new();
        for turn in 0..40 {
            let id = format!("u{turn}");
            records.push(said(&id, &format!("question {turn} {}", "x".repeat(200))));
            records.push(delivered(&id, 5, 1_790_000_000_000 + turn * 60_000));
            records.push(step(5));
            records.push(says(5, &format!("a{turn}"), &format!("answer {turn}")));
            records.push(done(5));
        }
        let restored = rebuilt(1_000, &records);
        assert!(restored.messages[0].content.contains("[The start of this conversation is left out"));
        assert_eq!(restored.messages.last().unwrap().content, "answer 39");
        assert!(!lines(&restored).iter().any(|line| line.contains("question 0 ")));
        assert_eq!(restored.context_tokens, 1_000);
        let whole = rebuilt(800_000, &records);
        assert_eq!(whole.messages.len(), 80);
        assert!(whole.context_tokens < 800_000);
    }
}
