//! Provider of the `suggestions` interface (`nucleus-suggestions-interface`):
//! the message a person might send next in a session, asked of this
//! service's own model.
//!
//! ## Authority
//!
//! It holds `kernel provide` for its endpoint and `model complete` covering
//! its `--model` (in the VM, every model), nothing else: no session,
//! conversation or tool capability, no binds and no network. It reads nothing
//! and writes nothing; the only effect of a request is one model call charged
//! to its own grant. The caller (the UI service) sends the session's
//! messages. A compromised service can spend its model grant, disclose the
//! messages it is sent to the model provider, and answer anything, which a
//! window shows as a placeholder the person may or may not send. It cannot
//! act or speak in a session.
//!
//! ## One request per session and action
//!
//! One kernel connection and one thread. A `next` or `title` on `[project, session]`
//! is checked and turned into a model call made with `submit`, so the loop
//! keeps serving while the call runs. Per session and action at most one
//! request is pending: a newer one for the same session and action cancels
//! the older one's model call (a kernel `Cancel`) and answers the older
//! dispatch with no text at once, then starts its own; one with the same
//! messages (another window of the session) joins the pending one and is
//! answered with it. A `title` and a `next` of one session do not displace
//! each other. A `title` with no messages is answered with no title and no
//! model call. A `CancelDispatch` drops its dispatch; the model call is
//! cancelled once no dispatch waits for it. A model answer for a request
//! that is no longer pending is dropped. A refused request (bad payload or
//! resource) changes nothing that is pending. Sessions do not affect each
//! other, except that a new request beyond `--max-sessions` in progress (a
//! session's `next` and its `title` count apart) is answered with no text at
//! once; the bound keeps this process's model calls
//! within the kernel's default limit of pending calls per caller (16).
//!
//! ## The model call
//!
//! Without tools or reasoning, with a small output cap, and a deadline of the
//! dispatch's remaining time less a margin, at most 20 s. A `next` with skill
//! candidates sends the SKILLS text as the system message and a JSON object
//! of the transcript and the candidates as the user message. Otherwise, with
//! messages, the NEXT or TITLE text is the system message and the messages
//! follow as one transcript in a user message; without, the START text is the
//! one user message (`next` only). The reply is cut to its first nonempty
//! line without wrapping quotes (nor, for a title, a trailing full stop); one
//! longer than `MAX_SUGGESTION_BYTES` (`MAX_TITLE_BYTES` for a title), or a
//! reply that did not stop normally, is no text. A failed model call is
//! answered with its error.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::rc::Rc;

use nucleus_model_interface::{
    self as model, CompletionRequest, CompletionResponse, FinishReason, ReasoningEffort,
};
use nucleus_protocol::{
    ACTION_PROVIDE, DispatchId, EndpointName, Error, ErrorCode, HandleId, KERNEL_ENDPOINT, Payload,
    RequestId, ResourcePath,
};
use nucleus_sdk::{ChannelError, Connection, Dispatch, Event};
use nucleus_suggestions_interface::{
    self as suggestions, MAX_SUGGESTION_BYTES, MAX_TITLE_BYTES, NextRequest, Role, Suggestion, Title,
    TitleRequest,
};

/// Output cap of one model call: a suggestion is one short line.
const MAX_OUTPUT_TOKENS: u32 = 200;
/// Longest a model call may take; a suggestion that late is not worth offering.
const MAX_MODEL_MS: u64 = 20_000;
/// Time kept back from the caller's deadline to answer it after the model.
const REPLY_MARGIN_MS: u64 = 1_000;
/// The kernel's default limit of pending calls per caller. Every request in
/// progress holds one model call; a cancelled one stops counting at once.
const MAX_PENDING_PER_CALLER: usize = 16;
const DEFAULT_MAX_SESSIONS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The model id, which is also the resource of the `model complete` grant.
    pub model: String,
    /// The endpoint this service provides.
    pub endpoint: EndpointName,
    /// Requests in progress at once (`--max-sessions`).
    pub max_sessions: usize,
}

impl Options {
    /// Parses `--model <id> [--endpoint <name>] [--max-sessions <n>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut model, mut endpoint, mut max_sessions) = (None, suggestions::ENDPOINT.to_owned(), None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--model" => model = Some(value),
                "--endpoint" => endpoint = value,
                "--max-sessions" => {
                    max_sessions = Some(value.parse::<usize>().map_err(|_| "--max-sessions needs a number")?)
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let model = model.ok_or("--model is required")?;
        model::resource_for(&model).map_err(|e| format!("--model: {}", e.message))?;
        let max_sessions = max_sessions.unwrap_or(DEFAULT_MAX_SESSIONS);
        if !(1..=MAX_PENDING_PER_CALLER).contains(&max_sessions) {
            return Err(format!("--max-sessions must be between 1 and {MAX_PENDING_PER_CALLER}"));
        }
        let endpoint = EndpointName::new(endpoint).map_err(|e| format!("--endpoint: {}", e.message))?;
        Ok(Self { model, endpoint, max_sessions })
    }
}

/// What the service does with its kernel channel.
trait Kernel {
    fn submit(
        &mut self,
        handle: HandleId,
        resource: &ResourcePath,
        request: &CompletionRequest,
        deadline_ms: u64,
    ) -> Result<Result<RequestId, Error>, ChannelError>;
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError>;
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError>;
}

impl<R: Read, W: Write> Kernel for Connection<R, W> {
    fn submit(
        &mut self,
        handle: HandleId,
        resource: &ResourcePath,
        request: &CompletionRequest,
        deadline_ms: u64,
    ) -> Result<Result<RequestId, Error>, ChannelError> {
        Connection::submit(self, handle, model::ACTION_COMPLETE, resource, request, Some(deadline_ms))
    }
    fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
        Connection::cancel(self, request)
    }
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError> {
        Connection::reply(self, dispatch, result)
    }
}

/// The action a pending request came in on; the pending map's key is the
/// session's resource and this, so a `next` and a `title` of one session are
/// in flight at the same time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Next,
    Title,
}

/// A checked request of either action.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Next(NextRequest),
    Title(TitleRequest),
}

impl Request {
    fn kind(&self) -> Kind {
        match self {
            Self::Next(_) => Kind::Next,
            Self::Title(_) => Kind::Title,
        }
    }
    fn messages(&self) -> &[suggestions::Message] {
        match self {
            Self::Next(request) => &request.messages,
            Self::Title(request) => &request.messages,
        }
    }
}

/// The request in progress for one session and action: its model call and
/// the dispatches waiting for it.
struct Pending {
    request: Request,
    call: RequestId,
    waiting: Vec<DispatchId>,
}

/// A pending request's place in the map.
type Slot = (ResourcePath, Kind);

struct Service {
    model: HandleId,
    model_resource: ResourcePath,
    max_sessions: usize,
    /// Keyed by the dispatch's resource, `[project, session]`, and action.
    pending: BTreeMap<Slot, Pending>,
}

impl Service {
    fn new(model: HandleId, model_resource: ResourcePath, max_sessions: usize) -> Self {
        Self { model, model_resource, max_sessions, pending: BTreeMap::new() }
    }

    fn dispatch(&mut self, kernel: &mut impl Kernel, dispatch: &Dispatch) -> Result<(), ChannelError> {
        let request = match admit(dispatch) {
            Ok(request) => request,
            Err(e) => return kernel.reply(dispatch.id, Err(e)),
        };
        let slot = (dispatch.resource.clone(), request.kind());
        if let Some(pending) = self.pending.get_mut(&slot)
            && pending.request == request
        {
            pending.waiting.push(dispatch.id);
            return Ok(());
        }
        if let Some(older) = self.pending.remove(&slot) {
            kernel.cancel(older.call)?;
            for waiting in older.waiting {
                kernel.reply(waiting, no_text(&slot))?;
            }
        }
        // A title without messages has nothing to name: no model call.
        if slot.1 == Kind::Title && request.messages().is_empty() {
            return kernel.reply(dispatch.id, no_text(&slot));
        }
        let deadline_ms = dispatch.remaining_ms.saturating_sub(REPLY_MARGIN_MS).min(MAX_MODEL_MS);
        if self.pending.len() >= self.max_sessions || deadline_ms == 0 {
            return kernel.reply(dispatch.id, no_text(&slot));
        }
        match kernel.submit(self.model, &self.model_resource, &completion(&request, &slot.0), deadline_ms)? {
            Ok(call) => {
                self.pending.insert(slot, Pending { request, call, waiting: vec![dispatch.id] });
                Ok(())
            }
            Err(e) => kernel.reply(dispatch.id, Err(e)),
        }
    }

    /// The caller gave up (it was cancelled, timed out or exited); the kernel
    /// has already answered it.
    fn cancel_dispatch(
        &mut self,
        kernel: &mut impl Kernel,
        dispatch: DispatchId,
    ) -> Result<(), ChannelError> {
        let Some((slot, pending)) = self.pending.iter_mut().find(|(_, p)| p.waiting.contains(&dispatch))
        else {
            return Ok(());
        };
        pending.waiting.retain(|waiting| *waiting != dispatch);
        if !pending.waiting.is_empty() {
            return Ok(());
        }
        let (slot, call) = (slot.clone(), pending.call);
        self.pending.remove(&slot);
        kernel.cancel(call)
    }

    fn answer(
        &mut self,
        kernel: &mut impl Kernel,
        call: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        // Superseded or cancelled: nobody is waiting for it.
        let Some(slot) = self.pending.iter().find(|(_, p)| p.call == call).map(|(s, _)| s.clone()) else {
            return Ok(());
        };
        let Some(pending) = self.pending.remove(&slot) else { return Ok(()) };
        let reply = result
            .map_err(|e| Error::new(e.code, format!("model: {}", e.message)))
            .and_then(|payload| {
                payload
                    .decode::<CompletionResponse>()
                    .map_err(|e| Error::new(ErrorCode::ServiceError, e.message))
            })
            .and_then(|response| {
                if let Request::Next(request) = &pending.request
                    && !request.skills.is_empty()
                {
                    let (text, skills) = if response.finish_reason == FinishReason::Stop {
                        skill_suggestion(&response.message.content, &request.skills)
                    } else {
                        (None, Vec::new())
                    };
                    return Payload::encode(&Suggestion {
                        project: slot.0.segments()[0].clone(),
                        session: slot.0.segments()[1].clone(),
                        text,
                        skills,
                    });
                }
                let text = (response.finish_reason == FinishReason::Stop)
                    .then(|| match slot.1 {
                        Kind::Next => suggestion_text(&response.message.content),
                        Kind::Title => title_text(&response.message.content),
                    })
                    .flatten();
                answered(&slot, text)
            });
        for waiting in pending.waiting {
            kernel.reply(waiting, reply.clone())?;
        }
        Ok(())
    }
}

/// The request a dispatch carries, if it is one this service serves, checked.
fn admit(dispatch: &Dispatch) -> Result<Request, Error> {
    let invalid = |why: &str| Error::new(ErrorCode::InvalidInput, why.to_owned());
    if dispatch.resource.segments().len() != 2 {
        return Err(invalid("the resource names one session: [project, session]"));
    }
    match dispatch.action.as_str() {
        suggestions::ACTION_NEXT => {
            let request: NextRequest = dispatch.payload.decode()?;
            request.validate()?;
            Ok(Request::Next(request))
        }
        suggestions::ACTION_TITLE => {
            let request: TitleRequest = dispatch.payload.decode()?;
            request.validate()?;
            Ok(Request::Title(request))
        }
        _ => Err(invalid("unknown action")),
    }
}

/// The action's answer for the session the slot names, `[project, session]`,
/// with or without a text.
fn answered(slot: &Slot, text: Option<String>) -> Result<Payload, Error> {
    let segments = slot.0.segments();
    let part = |at: usize| segments.get(at).cloned().unwrap_or_default();
    match slot.1 {
        Kind::Next => {
            Payload::encode(&Suggestion { project: part(0), session: part(1), text, skills: Vec::new() })
        }
        Kind::Title => Payload::encode(&Title { project: part(0), session: part(1), title: text }),
    }
}

fn no_text(slot: &Slot) -> Result<Payload, Error> {
    answered(slot, None)
}

/// The conversation as a transcript the model reads, not turns of its own.
fn transcript(messages: &[suggestions::Message]) -> String {
    let mut transcript = String::new();
    for message in messages {
        let who = match message.role {
            Role::User => "person",
            Role::Assistant => "assistant",
        };
        for part in ["<", who, ">\n", message.text.as_str(), "\n</", who, ">\n"] {
            transcript.push_str(part);
        }
    }
    transcript
}

/// The model call for `request` about `session`, made for the session's
/// project.
fn completion(request: &Request, session: &ResourcePath) -> CompletionRequest {
    use nucleus_prompts::suggest::{NEXT, START, TITLE};
    let messages = match request {
        Request::Next(request) if !request.skills.is_empty() => vec![
            model::Message::text(model::Role::System, nucleus_prompts::suggest::SKILLS.trim_end()),
            model::Message::text(
                model::Role::User,
                serde_json::json!({"transcript": request.messages, "available_skills": request.skills})
                    .to_string(),
            ),
        ],
        Request::Next(request) if request.messages.is_empty() => {
            vec![model::Message::text(model::Role::User, START.trim_end())]
        }
        Request::Next(request) => vec![
            model::Message::text(model::Role::System, NEXT.trim_end()),
            model::Message::text(model::Role::User, transcript(&request.messages)),
        ],
        Request::Title(request) => vec![
            model::Message::text(model::Role::System, TITLE.trim_end()),
            model::Message::text(model::Role::User, transcript(&request.messages)),
        ],
    };
    CompletionRequest {
        messages,
        tools: Vec::new(),
        max_output_tokens: MAX_OUTPUT_TOKENS,
        temperature: None,
        reasoning_effort: Some(ReasoningEffort::Off),
        tool_choice: None,
        project: session.segments().first().filter(|p| nucleus_protocol::valid_project(p)).cloned(),
    }
}

/// The first nonempty line of what the model wrote, without wrapping quotes;
/// `None` when there is nothing usable or it is longer than one suggestion.
fn suggestion_text(content: &str) -> Option<String> {
    let line = content.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”' | '「' | '」' | '`')).trim();
    // A command is never offered: a suggestion is sent with one key, and `/skill URL` would
    // load what a description in the catalog steered the model to name.
    (!line.is_empty() && !line.starts_with('/') && line.len() <= MAX_SUGGESTION_BYTES)
        .then(|| line.to_owned())
}

/// The JSON object a model answered, also inside a code fence or with words around it.
fn answer_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    (start < end).then(|| &content[start..=end])
}

/// The line and skills of a `next` that carried skill candidates (at most
/// `MAX_SKILLS` names, each with a description of at most
/// `MAX_SKILL_DESCRIPTION_BYTES`): the model answered a JSON object with its
/// line and skill names. Only names among the candidates are kept, each once,
/// at most `MAX_RECOMMENDED_SKILLS`. The model sees no skill's body, and a
/// recommendation grants nothing.
fn skill_suggestion(content: &str, candidates: &[suggestions::Skill]) -> (Option<String>, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Answer {
        text: Option<String>,
        #[serde(default)]
        skills: Vec<String>,
    }
    let Some(Ok(answer)) = answer_object(content).map(serde_json::from_str::<Answer>) else {
        return (None, Vec::new());
    };
    let text = answer.text.as_deref().and_then(suggestion_text);
    let mut skills = Vec::new();
    for name in answer.skills {
        if candidates.iter().any(|skill| skill.name == name) && !skills.contains(&name) {
            skills.push(name);
        }
        if skills.len() == suggestions::MAX_RECOMMENDED_SKILLS {
            break;
        }
    }
    (text, skills)
}

/// A title as the model was asked to write it: the suggestion's line, also
/// without a trailing full stop, and never longer than one title.
fn title_text(content: &str) -> Option<String> {
    let line = suggestion_text(content)?;
    let line = line.trim_end_matches(['.', '。', '…']).trim_end();
    (!line.is_empty() && line.len() <= MAX_TITLE_BYTES).then(|| line.to_owned())
}

/// Provides the endpoint and serves it until the kernel terminates this process.
pub fn run(options: &Options) -> Result<(), String> {
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let model_resource = model::resource_for(&options.model).map_err(|e| e.message)?;
    let model = kernel
        .find_handle_for(model::ENDPOINT, model::ACTION_COMPLETE, &model_resource)
        .ok_or_else(|| format!("started without model complete on {}", options.model))?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    let service = Rc::new(RefCell::new(Service::new(model, model_resource, options.max_sessions)));
    // Dispatches are served as they arrive, also while `next_answer` waits for
    // the model; neither borrows the service while the other runs.
    let serving = Rc::clone(&service);
    kernel.set_event_handler(move |kernel, event| {
        match event {
            Event::Dispatch(dispatch) => serving.borrow_mut().dispatch(kernel, dispatch)?,
            Event::CancelDispatch(dispatch) => serving.borrow_mut().cancel_dispatch(kernel, *dispatch)?,
            Event::Terminate => return Ok(false),
            Event::ChildExited(_) | Event::ChildReaped { .. } => {}
        }
        Ok(true)
    });
    let interface = suggestions::interface(options.endpoint.clone()).map_err(|e| e.message)?;
    kernel.provide(authority, interface).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    loop {
        let outcome = match kernel.next_answer() {
            Ok(Some((call, result))) => service.borrow_mut().answer(&mut kernel, call, result),
            Ok(None) => Ok(()),
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => Err(e),
        };
        outcome.map_err(|e| e.to_string())?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use nucleus_protocol::{ActionName, ProcessId};
    use serde_json::{Value, json};

    /// What the service sent the kernel: model calls with their deadlines,
    /// cancelled model calls, and replies to dispatches.
    #[derive(Default)]
    struct Recorder {
        submitted: Vec<(u64, u64)>,
        cancelled: Vec<u64>,
        replies: Vec<(u64, Result<Value, ErrorCode>)>,
    }

    impl Kernel for Recorder {
        fn submit(
            &mut self,
            _: HandleId,
            _: &ResourcePath,
            request: &CompletionRequest,
            deadline_ms: u64,
        ) -> Result<Result<RequestId, Error>, ChannelError> {
            assert!(request.tools.is_empty() && request.reasoning_effort == Some(ReasoningEffort::Off));
            assert_eq!(request.project.as_deref(), Some("p"), "made for the session's project");
            let id = self.submitted.len() as u64 + 1;
            self.submitted.push((id, deadline_ms));
            Ok(Ok(RequestId(id)))
        }
        fn cancel(&mut self, request: RequestId) -> Result<(), ChannelError> {
            self.cancelled.push(request.0);
            Ok(())
        }
        fn reply(
            &mut self,
            dispatch: DispatchId,
            result: Result<Payload, Error>,
        ) -> Result<(), ChannelError> {
            let result = result.map(|p| p.decode::<Value>().unwrap()).map_err(|e| e.code);
            self.replies.push((dispatch.0, result));
            Ok(())
        }
    }

    impl Recorder {
        /// Each dispatch's replies, by dispatch, whenever they were sent.
        fn replies(&self) -> Vec<(u64, Result<Value, ErrorCode>)> {
            let mut replies = self.replies.clone();
            replies.sort_by_key(|(dispatch, _)| *dispatch);
            replies
        }
    }

    fn next(id: u64, session: &[&str], payload: Value, remaining_ms: u64) -> Dispatch {
        with_action(id, suggestions::ACTION_NEXT, session, payload, remaining_ms)
    }

    fn title(id: u64, session: &[&str], payload: Value, remaining_ms: u64) -> Dispatch {
        with_action(id, suggestions::ACTION_TITLE, session, payload, remaining_ms)
    }

    fn with_action(id: u64, action: &str, session: &[&str], payload: Value, remaining_ms: u64) -> Dispatch {
        Dispatch {
            id: DispatchId(id),
            endpoint: nucleus_protocol::EndpointName::new(suggestions::ENDPOINT).unwrap(),
            caller: ProcessId(3),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(session.iter().copied()).unwrap(),
            payload: Payload::encode(&payload).unwrap(),
            remaining_ms,
        }
    }

    fn said(text: &str) -> Value {
        json!({"messages": [{"role": "user", "text": text}]})
    }

    fn model_says(content: &str) -> Result<Payload, Error> {
        Payload::encode(&json!({
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "served_by": "deepseek/flash",
        }))
    }

    fn service(max_sessions: usize) -> Service {
        Service::new(HandleId(1), ResourcePath::new(["deepseek", "flash"]).unwrap(), max_sessions)
    }

    /// A newer `next` or `title` supersedes its own session's older one: the
    /// older call is cancelled and answered with no text, and its late answer
    /// dropped. Another session's request, or a refused one, displaces nothing.
    #[test]
    fn a_newer_request_supersedes_only_its_own_sessions_older_one() {
        let (mut service, mut kernel) = (service(8), Recorder::default());
        service.dispatch(&mut kernel, &next(1, &["p", "a"], said("first"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(2, &["p", "b"], said("other"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(3, &["p", "a"], said("second"), 30_000)).unwrap();
        service.answer(&mut kernel, RequestId(1), model_says("late")).unwrap();
        service.answer(&mut kernel, RequestId(2), model_says("Open the notes.")).unwrap();
        service
            .dispatch(&mut kernel, &next(4, &["p", "a"], json!({"messages": [], "x": 1}), 30_000))
            .unwrap();
        service.cancel_dispatch(&mut kernel, DispatchId(3)).unwrap();
        service.answer(&mut kernel, RequestId(3), Err(Error::new(ErrorCode::Cancelled, "gone"))).unwrap();
        service.dispatch(&mut kernel, &title(5, &["p", "c"], said("first"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &title(6, &["p", "c"], said("second"), 30_000)).unwrap();
        service.answer(&mut kernel, RequestId(5), model_says("Better title.")).unwrap();
        assert_eq!(
            kernel.replies(),
            [
                (1, Ok(json!({"project": "p", "session": "a"}))),
                (2, Ok(json!({"project": "p", "session": "b", "text": "Open the notes."}))),
                (4, Err(ErrorCode::InvalidInput)),
                (5, Ok(json!({"project": "p", "session": "c"}))),
                (6, Ok(json!({"project": "p", "session": "c", "title": "Better title"}))),
            ]
        );
        assert_eq!(kernel.cancelled, [1, 3, 4]);
        assert!(service.pending.is_empty());
    }

    /// Two windows of a session ask with the same messages: one model call
    /// answers both, and it is cancelled only once neither waits for it.
    #[test]
    fn a_request_with_the_same_messages_waits_for_the_pending_call() {
        let (mut service, mut kernel) = (service(8), Recorder::default());
        service.dispatch(&mut kernel, &next(1, &["p", "a"], said("x"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(2, &["p", "a"], said("x"), 30_000)).unwrap();
        service.cancel_dispatch(&mut kernel, DispatchId(1)).unwrap();
        service.dispatch(&mut kernel, &next(3, &["p", "a"], said("x"), 30_000)).unwrap();
        service.answer(&mut kernel, RequestId(1), model_says("Next.")).unwrap();
        service.dispatch(&mut kernel, &next(4, &["p", "a"], said("y"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(5, &["p", "a"], said("y"), 30_000)).unwrap();
        service.cancel_dispatch(&mut kernel, DispatchId(4)).unwrap();
        service.cancel_dispatch(&mut kernel, DispatchId(5)).unwrap();
        let next = json!({"project": "p", "session": "a", "text": "Next."});
        assert_eq!(kernel.replies(), [(2, Ok(next.clone())), (3, Ok(next))]);
        assert_eq!((kernel.submitted.len(), kernel.cancelled), (2, vec![2]));
        assert!(service.pending.is_empty());
    }

    /// A request that is not one is refused; one the service has no room or
    /// time for is answered with no text at once; a model call never outlasts
    /// its caller's deadline.
    #[test]
    fn beyond_its_bounds_a_request_is_refused_or_answered_with_no_text_at_once() {
        let (mut service, mut kernel) = (service(1), Recorder::default());
        let too_many = json!({"messages": vec![json!({"role": "user", "text": "x"}); 41]});
        service.dispatch(&mut kernel, &next(1, &["a"], said("x"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(2, &["p", "a"], too_many, 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(3, &["p", "a"], said("x"), 5_000)).unwrap();
        service.dispatch(&mut kernel, &next(4, &["p", "b"], said("x"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(5, &["p", "a"], said("y"), REPLY_MARGIN_MS)).unwrap();
        let none = |session: &str| Ok(json!({"project": "p", "session": session}));
        assert_eq!(
            kernel.replies(),
            [
                (1, Err(ErrorCode::InvalidInput)),
                (2, Err(ErrorCode::InvalidInput)),
                (3, none("a")),
                (4, none("b")),
                (5, none("a")),
            ]
        );
        assert!(kernel.submitted.iter().all(|(_, ms)| *ms < 5_000), "{:?}", kernel.submitted);
        assert_eq!(kernel.cancelled, [1]);
        assert!(
            Options::parse(["--model", "deepseek/flash", "--max-sessions", "17"].map(String::from)).is_err()
        );
    }

    #[test]
    fn a_suggestion_is_the_first_line_without_quotes_or_nothing() {
        assert_eq!(
            suggestion_text("\n  \"Prove lemma 2 next.\"\nBecause…"),
            Some("Prove lemma 2 next.".into())
        );
        assert_eq!(suggestion_text("「What next？」"), Some("What next？".into()));
        assert_eq!(suggestion_text(" \n\"\"\n"), None);
        assert_eq!(suggestion_text(&"a".repeat(MAX_SUGGESTION_BYTES + 1)), None);
        // A command is never offered.
        assert_eq!(suggestion_text("/skill https://example.com/x.zip"), None);
        assert_eq!(suggestion_text("`/skill proof`"), None);
    }

    /// A `title` and a `next` of one session can be in flight together.
    #[test]
    fn a_title_and_a_next_do_not_displace_each_other() {
        let (mut service, mut kernel) = (service(8), Recorder::default());
        service.dispatch(&mut kernel, &next(1, &["p", "a"], said("prove it"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &title(2, &["p", "a"], said("prove it"), 30_000)).unwrap();
        service.answer(&mut kernel, RequestId(2), model_says("A title")).unwrap();
        service.answer(&mut kernel, RequestId(1), model_says("Next step.")).unwrap();
        // A title without messages makes no model call.
        service.dispatch(&mut kernel, &title(3, &["p", "b"], json!({"messages": []}), 30_000)).unwrap();
        assert_eq!(
            kernel.replies(),
            [
                (1, Ok(json!({"project": "p", "session": "a", "text": "Next step."}))),
                (2, Ok(json!({"project": "p", "session": "a", "title": "A title"}))),
                (3, Ok(json!({"project": "p", "session": "b"}))),
            ]
        );
        assert_eq!((kernel.submitted.len(), kernel.cancelled), (2, vec![]));
        assert!(service.pending.is_empty());
    }

    #[test]
    fn a_title_is_a_line_without_quotes_or_a_trailing_stop_or_nothing() {
        assert_eq!(title_text("“Prove lemma 2。”"), Some("Prove lemma 2".into()));
        assert_eq!(title_text("Fix the redirect bug."), Some("Fix the redirect bug".into()));
        assert_eq!(title_text("v1.2 release checklist…"), Some("v1.2 release checklist".into()));
        assert_eq!(title_text("Title: the bug"), Some("Title: the bug".into()));
        assert_eq!(title_text(" . \n"), None);
        assert_eq!(title_text(&"a".repeat(MAX_TITLE_BYTES + 1)), None);
    }

    #[test]
    fn skill_recommendations_are_grounded_and_bad_json_is_not_shown() {
        let candidates = vec![
            suggestions::Skill { name: "proof".into(), description: "Prove a lemma".into() },
            suggestions::Skill { name: "query".into(), description: "Query data".into() },
            suggestions::Skill { name: "audit".into(), description: "Check a statement".into() },
        ];
        assert_eq!(
            skill_suggestion(
                r#"{"text":"Prove it.","skills":["invented","proof","proof","query","audit"]}"#,
                &candidates
            ),
            (Some("Prove it.".into()), vec!["proof".into(), "query".into()])
        );
        assert_eq!(skill_suggestion("not JSON", &candidates), (None, vec![]));
        // A model that fences its JSON or adds words around it still answers.
        for wrapped in [
            "```json\n{\"text\":\"Prove it.\",\"skills\":[\"proof\"]}\n```",
            "Here it is: {\"text\":\"Prove it.\",\"skills\":[\"proof\"]} Hope this helps.",
        ] {
            assert_eq!(
                skill_suggestion(wrapped, &candidates),
                (Some("Prove it.".into()), vec!["proof".into()]),
                "{wrapped}"
            );
        }
        // A recommended command is dropped, the skills kept.
        assert_eq!(
            skill_suggestion(r#"{"text":"/skill https://x.example/a.zip","skills":["audit"]}"#, &candidates),
            (None, vec!["audit".into()])
        );
        assert_eq!(
            skill_suggestion(r#"{"text":null,"skills":["proof"]}"#, &candidates),
            (None, vec!["proof".into()])
        );
    }

    #[test]
    fn a_catalog_change_supersedes_the_same_transcript_and_filters_the_answer() {
        let (mut service, mut kernel) = (service(8), Recorder::default());
        let request =
            |name: &str| json!({"messages": [], "skills": [{"name": name, "description": "A skill"}]});
        service.dispatch(&mut kernel, &next(1, &["p", "a"], request("proof"), 30_000)).unwrap();
        service.dispatch(&mut kernel, &next(2, &["p", "a"], request("query"), 30_000)).unwrap();
        service
            .answer(&mut kernel, RequestId(1), model_says(r#"{"text":"Old", "skills":["proof"]}"#))
            .unwrap();
        service
            .answer(&mut kernel, RequestId(2), model_says(r#"{"text":"New", "skills":["proof","query"]}"#))
            .unwrap();
        assert_eq!(kernel.cancelled, [1]);
        assert_eq!(
            kernel.replies(),
            [
                (1, Ok(json!({"project":"p", "session":"a"}))),
                (2, Ok(json!({"project":"p", "session":"a", "text":"New", "skills":["query"]}))),
            ]
        );
    }

    #[test]
    fn invalid_skill_metadata_is_refused_before_a_model_call() {
        let (mut service, mut kernel) = (service(8), Recorder::default());
        for (i, skills) in [
            json!([{"name":"../bad", "description":""}]),
            json!([{"name":"a", "description":""}, {"name":"a", "description":""}]),
            json!([{"name":"a", "description":"x".repeat(2401)}]),
            json!((0..33).map(|n| json!({"name":format!("s{n}"),"description":""})).collect::<Vec<_>>()),
        ]
        .into_iter()
        .enumerate()
        {
            service
                .dispatch(
                    &mut kernel,
                    &next(i as u64 + 1, &["p", "a"], json!({"messages":[],"skills":skills}), 30_000),
                )
                .unwrap();
        }
        assert!(kernel.submitted.is_empty());
        assert!(kernel.replies().iter().all(|(_, result)| *result == Err(ErrorCode::InvalidInput)));
    }
}
