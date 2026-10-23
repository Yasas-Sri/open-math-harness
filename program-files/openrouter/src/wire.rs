//! Translation between the `model` interface and OpenRouter's OpenAI-compatible
//! chat completions format. Pure functions; no I/O.
//!
//! It is the one profile the adapter speaks with its gateway (in the VM the
//! composition's `model_base_url`): `POST <base>/chat/completions` with the
//! key as a bearer token, of which only the first choice is read. Readable
//! reasoning is read from `reasoning` alone; `reasoning_details` is kept
//! opaque and sent back with the assistant message it came with. Nothing here
//! chooses a field by provider. A provider error can arrive as the status,
//! inside a 200 body or as a stream event, and a stream that ends without a
//! finish reason or `[DONE]` is an error, never a completed reply.

use nucleus_model_interface::{
    CompletionRequest, CompletionResponse, FinishReason, Message, Part, ReasoningEffort, Role, ToolCall,
    ToolChoice, Usage,
};
use nucleus_protocol::{Error, ErrorCode};
use serde::Deserialize;
use serde_json::{Value, json};

fn role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// A message's `content`: its text, or, for a user message with parts, the
/// OpenAI content array: the text, then each part in order, an image as an
/// `image_url` with a `data:` URL. A tool result is always text; the caller
/// puts a tool's images in a user message.
fn content(message: &Message) -> Value {
    if message.parts.is_empty() {
        return json!(message.content);
    }
    let text = |text: &str| json!({"type": "text", "text": text});
    let mut parts: Vec<Value> = Vec::with_capacity(message.parts.len() + 1);
    if !message.content.is_empty() {
        parts.push(text(&message.content));
    }
    for part in &message.parts {
        parts.push(match part {
            Part::Text { text: more } => text(more),
            Part::Image { media_type, data_base64 } => json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{media_type};base64,{data_base64}")},
            }),
        });
    }
    Value::Array(parts)
}

/// Two user messages in a row as one, in order. Texts are joined by a blank
/// line; with images, the parts follow one another.
fn join_user(into: &mut Value, next: Value) {
    let parts = |content: Value| -> Vec<Value> {
        match content {
            Value::Array(parts) => parts,
            Value::String(text) if text.is_empty() => Vec::new(),
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            other => vec![other],
        }
    };
    into["content"] = match (into["content"].take(), next) {
        (Value::String(a), Value::String(b)) if a.is_empty() || b.is_empty() => Value::String(a + &b),
        (Value::String(a), Value::String(b)) => Value::String(format!("{a}\n\n{b}")),
        (a, b) => Value::Array(parts(a).into_iter().chain(parts(b)).collect()),
    };
}

/// Body of `POST /chat/completions`.
pub fn request_body(model: &str, request: &CompletionRequest) -> Value {
    let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len());
    for m in &request.messages {
        if m.role == Role::User
            && let Some(last) = messages.last_mut().filter(|last| last["role"] == "user")
        {
            join_user(last, content(m));
            continue;
        }
        messages.push({
            let mut out = json!({ "role": role(m.role), "content": content(m) });
            if !m.tool_calls.is_empty() {
                out["tool_calls"] = m
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            // The wire format carries arguments as JSON text.
                            "function": { "name": call.name, "arguments": call.arguments.to_string() },
                        })
                    })
                    .collect();
            }
            if let Some(id) = &m.tool_call_id {
                out["tool_call_id"] = json!(id);
            }
            if let Some(details) = &m.reasoning_details {
                // Handed back exactly as the provider produced it (the model
                // interface's contract for this field).
                out["reasoning_details"] = details.clone();
            }
            out
        });
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if request.max_output_tokens != 0 {
        body["max_tokens"] = json!(request.max_output_tokens);
    }
    // Asks the endpoint to price the call itself and return the figure in
    // `usage.cost` (model interface, `Usage::cost_nano_usd`).
    body["usage"] = json!({ "include": true });
    // Without this the final chunk carries no token counts.
    body["stream_options"] = json!({ "include_usage": true });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    match &request.tool_choice {
        Some(ToolChoice::Named(name)) => {
            body["tool_choice"] = json!({"type": "function", "function": {"name": name}});
        }
        None => {}
    }
    // OpenRouter's unified reasoning controls.
    let mut reasoning = serde_json::Map::new();
    match request.reasoning_effort {
        Some(ReasoningEffort::Off) => {
            reasoning.insert("enabled".to_owned(), json!(false));
        }
        Some(ReasoningEffort::On) => {
            reasoning.insert("enabled".to_owned(), json!(true));
        }
        Some(effort) => {
            reasoning.insert("effort".to_owned(), json!(effort.as_str()));
        }
        None => {}
    }
    if !reasoning.is_empty() {
        body["reasoning"] = Value::Object(reasoning);
    }
    body
}

// Unknown fields are tolerated on purpose: providers add fields (reasoning
// traces, routing metadata) and the adapter only forwards what the interface
// defines.
#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
    /// OpenRouter's structured reasoning blocks, kept opaque and handed back.
    #[serde(default)]
    reasoning_details: Option<Value>,
    /// The readable reasoning.
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// USD for this call, which the request asks for (`usage.include`); only
    /// the total is read.
    #[serde(default)]
    cost: Option<f64>,
}

impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Self {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cost_nano_usd: u.cost.map_or(0, nano_usd),
        }
    }
}

/// A reported USD cost as nano-USD, rounded up so a call is never free for
/// having been cheap. A missing, negative or non-finite figure is no figure:
/// it becomes 0, which the interface defines as "nothing reported".
fn nano_usd(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    let nano = (usd * 1e9).ceil();
    if nano >= u64::MAX as f64 { u64::MAX } else { nano as u64 }
}

#[derive(Deserialize)]
struct WireError {
    /// A number or a string, depending on who wrote the error.
    #[serde(default)]
    code: Value,
    #[serde(default)]
    message: Option<String>,
}

impl WireError {
    /// The provider's own structured statement that the account has no
    /// credit left. Read from the error's `code`, never from its text.
    fn is_credit_refusal(&self) -> bool {
        self.code.as_u64() == Some(402)
            || matches!(self.code.as_str(), Some("402" | "insufficient_credits" | "insufficient_quota"))
    }
}

/// Longest provider error text passed on to a caller.
const MAX_ERROR_CHARS: usize = 300;

/// How every refusal of the account for want of credit begins.
const CREDIT_REFUSAL: &str = "provider reports insufficient credit";

/// Whether `error` is a refusal of the account for want of credit, which no
/// retry can fix: the person has to top up. The test is on the start of the
/// message, which is always this module's own wording; the provider's text
/// only ever follows it.
pub fn is_credit_refusal(error: &Error) -> bool {
    error.message.starts_with(CREDIT_REFUSAL)
}

fn provider_error(status: u16, reported: Option<&WireError>) -> Error {
    // A 200 body or an event of the stream can carry the refusal too, with the
    // status in its `code`: the envelope does not make it less final.
    let kind = if status == 402 || reported.is_some_and(WireError::is_credit_refusal) {
        CREDIT_REFUSAL
    } else {
        match status {
            401 | 403 => "provider rejected the adapter's credentials or permissions",
            408 | 504 => "provider timed out",
            429 => "provider rate limit reached",
            400 | 404 | 422 => "provider rejected the request",
            500..=599 => "provider failed",
            _ => "unexpected provider response",
        }
    };
    let detail: String =
        reported.and_then(|e| e.message.as_deref()).unwrap_or("").chars().take(MAX_ERROR_CHARS).collect();
    let code =
        if matches!(status, 400 | 404 | 422) { ErrorCode::InvalidInput } else { ErrorCode::ServiceError };
    Error::new(code, format!("{kind} (http {status}): {detail}"))
}

/// Tool arguments travel as JSON text. Unparseable text is kept as a string for the caller to reject.
fn parse_arguments(text: String) -> Value {
    if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }
}

fn finish_reason(reported: Option<&str>, has_tool_calls: bool) -> FinishReason {
    match reported {
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        _ if has_tool_calls => FinishReason::ToolCalls,
        _ => FinishReason::Other,
    }
}

/// Interprets an HTTP response. OpenRouter can report an upstream failure with
/// status 200 and an `error` object, so the body decides as well as the status.
pub fn parse_response(status: u16, body: &str, requested_model: &str) -> Result<CompletionResponse, Error> {
    let parsed: Option<WireResponse> = serde_json::from_str(body).ok();
    if !(200..300).contains(&status) {
        return Err(provider_error(status, parsed.as_ref().and_then(|p| p.error.as_ref())));
    }
    let Some(parsed) = parsed else {
        return Err(Error::new(ErrorCode::ServiceError, "provider returned a body that is not valid JSON"));
    };
    if let Some(error) = &parsed.error {
        return Err(provider_error(status, Some(error)));
    }
    let Some(choice) = parsed.choices.into_iter().next() else {
        return Err(Error::new(ErrorCode::ServiceError, "provider returned no choices"));
    };

    let tool_calls: Vec<ToolCall> = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: parse_arguments(call.function.arguments),
        })
        .collect();

    let finish_reason = finish_reason(choice.finish_reason.as_deref(), !tool_calls.is_empty());
    let usage = parsed.usage.map_or_else(Usage::default, Usage::from);
    Ok(CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content: choice.message.content.unwrap_or_default(),
            tool_calls,
            tool_call_id: None,
            reasoning_details: choice.message.reasoning_details.filter(|d| !d.is_null()),
            reasoning: choice.message.reasoning.filter(|r| !r.trim().is_empty()),
            parts: Vec::new(),
        },
        finish_reason,
        usage,
        served_by: parsed.model.unwrap_or_else(|| requested_model.to_owned()),
    })
}

#[derive(Deserialize)]
struct WireChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<WireChunkChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireChunkChoice {
    #[serde(default)]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCallDelta>>,
    #[serde(default)]
    reasoning_details: Option<Vec<Value>>,
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunctionDelta>,
}

#[derive(Deserialize)]
struct WireFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Most tool calls accepted in one streamed completion.
const MAX_STREAMED_TOOL_CALLS: usize = 64;
/// Most reasoning blocks kept from one streamed completion.
const MAX_STREAMED_REASONING_BLOCKS: usize = 256;

/// Folds one streamed reasoning fragment into the blocks so far: a fragment
/// with the `index` of a known block extends that block's text fields; any
/// other fragment starts a new block. Nothing is interpreted.
fn merge_reasoning_block(blocks: &mut Vec<Value>, fragment: Value) {
    let index = fragment.get("index").and_then(Value::as_u64);
    let existing =
        index.and_then(|i| blocks.iter_mut().find(|b| b.get("index").and_then(Value::as_u64) == Some(i)));
    match (existing, fragment) {
        (Some(Value::Object(block)), Value::Object(fragment)) => {
            for (key, value) in fragment {
                match (block.get_mut(&key), value) {
                    (Some(Value::String(so_far)), Value::String(more))
                        if matches!(key.as_str(), "text" | "summary" | "data") =>
                    {
                        so_far.push_str(&more);
                    }
                    (_, value) => {
                        block.insert(key, value);
                    }
                }
            }
        }
        (_, fragment) => {
            if blocks.len() < MAX_STREAMED_REASONING_BLOCKS {
                blocks.push(fragment);
            }
        }
    }
}

/// Reassembles a streamed completion from server-sent event lines. Text is
/// handed out as it arrives; tool calls arrive in fragments and are only
/// meaningful once whole, so they are kept until [`Self::finish`].
#[derive(Default)]
pub struct StreamAssembler {
    content: String,
    /// The readable reasoning as it streams (the provider's `reasoning`).
    reasoning_text: String,
    tools: std::collections::BTreeMap<usize, (String, String, String)>,
    /// Bytes of tool-call names and arguments streamed so far.
    tool_call_bytes: usize,
    /// Reasoning blocks as streamed, merged by their `index` (a block's
    /// `text`/`summary`/`data` fragments are concatenated; other fields take
    /// the latest value), so what is handed back is one block per index.
    reasoning_blocks: Vec<Value>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
    done: bool,
}

impl StreamAssembler {
    /// Readable reasoning supplied through `delta.reasoning`. Its new suffix
    /// is delivered separately from answer text; opaque reasoning blocks are
    /// retained for the final response but never become display text.
    #[must_use]
    pub fn reasoning_text(&self) -> &str {
        &self.reasoning_text
    }

    /// Bytes of tool-call names and arguments streamed so far. Tool calls are
    /// handed over only whole, with the completion, but their fragments are
    /// output all the same: a caller watching for progress must count them,
    /// or a long tool call looks like a provider that stopped.
    #[must_use]
    pub fn tool_call_bytes(&self) -> usize {
        self.tool_call_bytes
    }

    /// Feeds one line of the event stream. Returns the answer text it added, if any.
    pub fn feed(&mut self, line: &str) -> Result<Option<String>, Error> {
        // Blank separators, `: keep-alive` comments and other SSE fields carry nothing for us.
        let Some(data) = line.strip_prefix("data:").map(str::trim) else { return Ok(None) };
        if data == "[DONE]" {
            self.done = true;
            return Ok(None);
        }
        let chunk: WireChunk = serde_json::from_str(data)
            .map_err(|_| Error::new(ErrorCode::ServiceError, "provider sent a malformed stream event"))?;
        if let Some(error) = chunk.error {
            return Err(provider_error(200, Some(&error)));
        }
        if chunk.model.is_some() {
            self.model = chunk.model;
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into());
        }
        let mut added = String::new();
        for choice in chunk.choices.into_iter().take(1) {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            if let Some(text) = choice.delta.content {
                added.push_str(&text);
            }
            if let Some(text) = choice.delta.reasoning {
                self.reasoning_text.push_str(&text);
            }
            for block in choice.delta.reasoning_details.unwrap_or_default() {
                merge_reasoning_block(&mut self.reasoning_blocks, block);
            }
            for fragment in choice.delta.tool_calls.unwrap_or_default() {
                if fragment.index >= MAX_STREAMED_TOOL_CALLS {
                    return Err(Error::new(ErrorCode::ServiceError, "provider streamed too many tool calls"));
                }
                let entry = self.tools.entry(fragment.index).or_default();
                if let Some(id) = fragment.id {
                    entry.0 = id;
                }
                if let Some(function) = fragment.function {
                    let name = function.name.as_deref().unwrap_or("");
                    let arguments = function.arguments.as_deref().unwrap_or("");
                    self.tool_call_bytes += name.len() + arguments.len();
                    entry.1.push_str(name);
                    entry.2.push_str(arguments);
                }
            }
        }
        self.content.push_str(&added);
        Ok(if added.is_empty() { None } else { Some(added) })
    }

    /// The whole completion. An event stream that ended without `[DONE]` or a
    /// finish reason was cut off, and is an error rather than a short answer.
    pub fn finish(self, requested_model: &str) -> Result<CompletionResponse, Error> {
        if !self.done && self.finish_reason.is_none() {
            return Err(Error::new(
                ErrorCode::ServiceError,
                "provider's stream ended before the completion did",
            ));
        }
        let tool_calls: Vec<ToolCall> = self
            .tools
            .into_values()
            .map(|(id, name, arguments)| ToolCall { id, name, arguments: parse_arguments(arguments) })
            .collect();
        let finish_reason = finish_reason(self.finish_reason.as_deref(), !tool_calls.is_empty());
        let reasoning_details =
            if self.reasoning_blocks.is_empty() { None } else { Some(Value::Array(self.reasoning_blocks)) };
        Ok(CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content,
                tool_calls,
                tool_call_id: None,
                reasoning_details,
                reasoning: (!self.reasoning_text.trim().is_empty()).then_some(self.reasoning_text),
                parts: Vec::new(),
            },
            finish_reason,
            usage: self.usage.unwrap_or_default(),
            served_by: self.model.unwrap_or_else(|| requested_model.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use nucleus_model_interface::ToolSpec;

    use super::*;

    #[test]
    fn requests_use_the_chat_completions_shape() {
        let request = CompletionRequest {
            messages: vec![
                Message::text(Role::System, "be brief"),
                Message::text(Role::User, "list the files"),
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "fs_list".into(),
                        arguments: json!({"path": ["work"]}),
                    }],
                    tool_call_id: None,
                    reasoning_details: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
                Message {
                    role: Role::Tool,
                    content: "[\"a.txt\"]".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call_1".into()),
                    reasoning_details: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
            ],
            tools: vec![ToolSpec {
                name: "fs_list".into(),
                description: "List a directory".into(),
                parameters: json!({"type": "object"}),
            }],
            max_output_tokens: 256,
            temperature: Some(0.0),
            reasoning_effort: None,
            tool_choice: None,
            project: None,
        };
        let body = request_body("deepseek/deepseek-v4-flash", &request);
        assert_eq!(body["messages"][3]["content"], "[\"a.txt\"]", "a tool result stays text");
        // Asks the provider to price the call.
        assert_eq!(body["usage"]["include"], json!(true));
        assert_eq!((&body["stream"], &body["stream_options"]["include_usage"]), (&json!(true), &json!(true)));
        assert_eq!(body["model"], "deepseek/deepseek-v4-flash");
        assert_eq!(body["max_tokens"], 256);
        let mut configured = request.clone();
        configured.max_output_tokens = 0;
        assert!(request_body("m", &configured).get("max_tokens").is_none());
        configured.max_output_tokens = 393_216;
        assert_eq!(request_body("m", &configured)["max_tokens"], 393_216);
        assert!(body.get("reasoning").is_none(), "absent effort sends no reasoning field");
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"path\":[\"work\"]}");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
        assert!(body["messages"][1].get("tool_calls").is_none());
        assert!(body["messages"][2].get("reasoning_details").is_none(), "absent stays absent");
        assert_eq!(body["tools"][0]["function"]["name"], "fs_list");

        // Reasoning the provider returned goes back exactly as it came.
        let details = json!([{"type": "reasoning.text", "text": "think", "id": "r1", "format": "x"}]);
        let mut with_reasoning = request.clone();
        with_reasoning.messages[2].reasoning_details = Some(details.clone());
        let body = request_body("deepseek/deepseek-v4-flash", &with_reasoning);
        assert_eq!(body["messages"][2]["reasoning_details"], details);

        // Successive user messages go as one, in order, images included.
        let mut seen = Message::text(Role::User, "the plot:");
        seen.parts = vec![Part::Image { media_type: "image/png".into(), data_base64: "AA==".into() }];
        let mut successive = request.clone();
        successive.messages.extend([
            Message::text(Role::User, "[the turn above was stopped]"),
            Message::text(Role::User, "what happened?"),
            seen,
        ]);
        let body = request_body("deepseek/deepseek-v4-flash", &successive);
        let sent = body["messages"].as_array().unwrap();
        assert_eq!(sent.len(), 5, "{sent:#?}");
        let image = json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}});
        assert_eq!(
            sent[4]["content"],
            json!([{"type": "text", "text": "[the turn above was stopped]\n\nwhat happened?"},
                   {"type": "text", "text": "the plot:"}, image])
        );

        // A tool's image follows its result as a user message; an empty text is left out.
        let mut viewed = Message::text(Role::User, "");
        viewed.parts = vec![Part::Image { media_type: "image/png".into(), data_base64: "AA==".into() }];
        let mut shown = request.clone();
        shown.messages.push(viewed);
        let body = request_body("deepseek/deepseek-v4-flash", &shown);
        assert_eq!(body["messages"][3]["content"], "[\"a.txt\"]");
        assert_eq!(body["messages"][4], json!({"role": "user", "content": [image]}));
    }

    #[test]
    fn responses_with_tool_calls_and_extra_fields_parse() {
        let body = r#"{"id":"gen-1","model":"deepseek/deepseek-v4-flash","provider":"DeepSeek",
            "choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
              "reasoning":"thinking...","tool_calls":[
                {"id":"call_9","type":"function","function":{"name":"fs_read","arguments":"{\"path\":[\"work\",\"a.txt\"]}"}},
                {"id":"call_10","type":"function","function":{"name":"fs_read","arguments":"{not json"}}]}}],
            "usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}}"#;
        let response = parse_response(200, body, "requested").unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.message.content, "");
        assert!(response.message.reasoning_details.is_none(), "a bare `reasoning` string is not the record");
        let structured = body.replace(
            r#""reasoning":"thinking...","#,
            r#""reasoning_details":[{"type":"reasoning.text","text":"t","id":"a","format":"f"}],"#,
        );
        let kept = parse_response(200, &structured, "requested").unwrap().message.reasoning_details;
        assert_eq!(kept.map(|d| d[0]["text"].clone()), Some(json!("t")));
        assert_eq!(response.message.tool_calls[0].arguments["path"][1], "a.txt");
        assert_eq!(response.message.tool_calls[1].arguments, json!("{not json"));
        assert_eq!(response.usage, Usage { input_tokens: 12, output_tokens: 7, cost_nano_usd: 0 });
        assert_eq!(response.served_by, "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn provider_failures_are_errors_even_with_status_200() {
        let upstream = r#"{"error":{"code":502,"message":"upstream provider unavailable"}}"#;
        assert_eq!(parse_response(200, upstream, "m").unwrap_err().code, ErrorCode::ServiceError);
        assert_eq!(parse_response(429, "{}", "m").unwrap_err().code, ErrorCode::ServiceError);
        assert_eq!(parse_response(400, upstream, "m").unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(parse_response(200, "<html>", "m").unwrap_err().code, ErrorCode::ServiceError);
        assert_eq!(parse_response(200, r#"{"choices":[]}"#, "m").unwrap_err().code, ErrorCode::ServiceError);
        let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(5_000));
        assert!(parse_response(500, &long, "m").unwrap_err().message.len() < 500);
    }

    fn feed_all(lines: &[&str]) -> (Vec<String>, Result<CompletionResponse, Error>) {
        let mut assembler = StreamAssembler::default();
        let mut deltas = Vec::new();
        for line in lines {
            match assembler.feed(line) {
                Ok(Some(text)) => deltas.push(text),
                Ok(None) => {}
                Err(e) => return (deltas, Err(e)),
            }
        }
        (deltas, assembler.finish("requested"))
    }

    #[test]
    fn readable_reasoning_never_comes_from_opaque_blocks() {
        let mut assembler = StreamAssembler::default();
        let blocks = json!([
            {"type":"reasoning.encrypted","data":"opaque secret","index":0},
            {"type":"reasoning.text","text":"not the display channel","index":1}
        ]);
        assert!(
            assembler
                .feed(&format!(
                    "data: {}",
                    json!({
                        "choices":[{"delta":{"reasoning_details":blocks}}]
                    })
                ))
                .unwrap()
                .is_none()
        );
        assert!(assembler.reasoning_text().is_empty());
        assert!(
            assembler.feed(r#"data: {"choices":[{"delta":{"reasoning":"thinking"}}]}"#).unwrap().is_none()
        );
        assert_eq!(assembler.reasoning_text(), "thinking");
        assert_eq!(assembler.feed(r#"data: {"choices":[{"delta":{"content":"answer","reasoning":"🙂"},"finish_reason":"stop"}]}"#).unwrap(), Some("answer".into()));
        let done = assembler.finish("model").unwrap();
        assert_eq!(done.message.reasoning.as_deref(), Some("thinking🙂"));
        assert_eq!(done.message.content, "answer");
        assert_eq!(done.message.reasoning_details, Some(blocks));
    }

    #[test]
    fn streamed_text_comes_out_as_it_arrives_and_adds_up() {
        let (deltas, done) = feed_all(&[
            ": OPENROUTER PROCESSING",
            "",
            r#"data: {"model":"deepseek/deepseek-v4-flash","choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            "event: ignored",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "data: [DONE]",
        ]);
        let done = done.unwrap();
        assert_eq!(deltas, ["Hel", "lo"]);
        assert_eq!(done.message.content, "Hello");
        assert!(done.message.reasoning_details.is_none());
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(done.usage, Usage { input_tokens: 5, output_tokens: 2, cost_nano_usd: 0 });
        assert_eq!(done.served_by, "deepseek/deepseek-v4-flash");
    }

    /// The cost is the provider's own `usage.cost`, never derived from token
    /// counts.
    #[test]
    fn the_providers_own_cost_is_reported_on_both_paths() {
        let cost = r#""cost":0.0000303,"cost_details":{"upstream_inference_prompt_cost":0.0000111}"#;
        let body = format!(
            r#"{{"model":"deepseek/deepseek-v4-flash",
                "choices":[{{"finish_reason":"stop","message":{{"role":"assistant","content":"ok"}}}}],
                "usage":{{"prompt_tokens":37,"completion_tokens":16,{cost}}}}}"#
        );
        // 0.0000303 USD, rounded up so a call is never free for being cheap.
        assert_eq!(parse_response(200, &body, "requested").unwrap().usage.cost_nano_usd, 30_300);

        // No `cost` is unknown, not free.
        let bare = body.replace(&format!(",{cost}"), "");
        assert_eq!(parse_response(200, &bare, "requested").unwrap().usage.cost_nano_usd, 0);

        let (_, done) = feed_all(&[
            r#"data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
            &format!(
                r#"data: {{"choices":[],"usage":{{"prompt_tokens":37,"completion_tokens":16,{cost}}}}}"#
            ),
            "data: [DONE]",
        ]);
        assert_eq!(done.unwrap().usage.cost_nano_usd, 30_300);
    }

    #[test]
    fn streamed_tool_calls_are_reassembled_from_fragments() {
        let (deltas, done) = feed_all(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs_read","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"wo"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"fs_list","arguments":"{\"path\":\"work\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"rk/a.txt\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        let done = done.unwrap();
        assert!(deltas.is_empty(), "tool call fragments are not text");
        assert_eq!(done.finish_reason, FinishReason::ToolCalls);
        let calls = &done.message.tool_calls;
        assert_eq!((calls[0].id.as_str(), calls[0].name.as_str()), ("call_1", "fs_read"));
        assert_eq!(calls[0].arguments, json!({"path": "work/a.txt"}));
        assert_eq!(calls[1].arguments, json!({"path": "work"}));
    }

    #[test]
    fn a_stream_that_is_cut_off_or_reports_an_error_is_an_error() {
        let (deltas, cut) = feed_all(&[r#"data: {"choices":[{"delta":{"content":"partial"}}]}"#]);
        assert_eq!(deltas, ["partial"]);
        assert_eq!(cut.unwrap_err().code, ErrorCode::ServiceError);
        let (_, failed) = feed_all(&[r#"data: {"error":{"code":502,"message":"upstream died"}}"#]);
        assert!(failed.unwrap_err().message.contains("upstream died"));
        let (_, garbage) = feed_all(&["data: {not json"]);
        assert_eq!(garbage.unwrap_err().code, ErrorCode::ServiceError);
        let flood = format!(
            r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":{MAX_STREAMED_TOOL_CALLS}}}]}}}}]}}"#
        );
        assert_eq!(feed_all(&[&flood]).1.unwrap_err().code, ErrorCode::ServiceError);
    }
    #[test]
    fn reasoning_effort_maps_to_openrouters_reasoning_field() {
        use nucleus_model_interface::{Message, ReasoningEffort, Role};
        let request = CompletionRequest {
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            max_output_tokens: 64,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Low),
            tool_choice: None,
            project: None,
        };
        let body = request_body("m", &request);
        assert_eq!(body["reasoning"]["effort"], "low");
        let off = CompletionRequest { reasoning_effort: Some(ReasoningEffort::Off), ..request.clone() };
        let body = request_body("m", &off);
        assert_eq!(
            body["reasoning"]["enabled"], false,
            "off is OpenRouter's `enabled: false`, not `effort: off`"
        );
        assert!(body["reasoning"].get("effort").is_none());
        let on = CompletionRequest { reasoning_effort: Some(ReasoningEffort::On), ..request.clone() };
        let body = request_body("m", &on);
        assert_eq!(body["reasoning"]["enabled"], true, "on is `enabled: true`, not `effort: on`");
        assert!(body["reasoning"].get("effort").is_none());
        let max = CompletionRequest {
            reasoning_effort: Some(ReasoningEffort::Max),
            temperature: Some(0.0),
            ..request
        };
        let body = request_body("m", &max);
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["temperature"], 0.0);
    }
}
