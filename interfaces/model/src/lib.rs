//! The `model` service interface, version 8.
//!
//! The **resource path is the model identifier**, one segment per
//! `/`-separated part: `["deepseek", "deepseek-v4-flash"]`. A capability scoped
//! to `["deepseek"]` therefore allows every model under that prefix and nothing
//! else, and the kernel enforces that before a provider sees the request.
//!
//! | Action | Payload | Result |
//! | --- | --- | --- |
//! | `complete` | [`CompletionRequest`] | [`CompletionResponse`] |
//! | `start` | [`CompletionRequest`] | [`StartResponse`] |
//! | `next` | [`NextRequest`] | [`NextResponse`] |
//! | `cancel` | [`CancelRequest`] | [`CancelResponse`] |
//!
//! **Streaming is built from ordinary calls; the kernel knows nothing about
//! it.** `start` begins a completion and returns a stream id at once. `next`
//! is a bounded long poll: it returns the text produced since the last call,
//! and, in the call that ends the stream, the whole [`CompletionResponse`]
//! (tool calls arrive only there, complete). The consumer pulls, so
//! backpressure is the consumer not calling `next`: the provider buffers a
//! bounded amount and then stops reading from upstream. A stream nobody pulls
//! is abandoned by the provider after an idle period. A stream belongs to the
//! process that started it, as identified by the kernel; its id is not authority.
//!
//! Two things let a puller tell waiting from dead. A stream the provider has
//! not dispatched yet, because it waits for a provider slot, reports a queue
//! lease in [`NextResponse::queue`]: for how
//! long the puller may treat its silence as queueing rather than a stall, at
//! most [`MAX_QUEUE_LEASE_MS`] and renewed by later pulls. A lease never
//! extends the completion's own deadline, and a reply without one reads as
//! not queued. And `cancel` answers [`CancelResponse`]: `released: true` once
//! the provider's reader and network attempt for the stream have stopped,
//! `false` when that was not observed in time. `released` is required, so a
//! reply `{}` does not satisfy it. It says the transport is done; it cannot
//! say the provider did no work or billed nothing, so a caller that gets
//! `false` treats the call's outcome as unknown and does not retry it
//! automatically.
//!
//! A user message may carry [`Part`]s after its `content`: more text and
//! images, in order, an image being its media type (one of
//! [`IMAGE_MEDIA_TYPES`]) and its bytes in base64. Images go only in user
//! messages: tool results stay text, and a caller with an image from a tool
//! puts it in a user message after the tool results. One request carries at
//! most [`MAX_REQUEST_IMAGES`] images of at most [`MAX_IMAGE_BYTES`] each and
//! [`MAX_REQUEST_IMAGE_DATA_BYTES`] of base64 together, and each image's
//! bytes must be what its media type says ([`CompletionRequest::validate`]).
//! An image is untrusted input like text: whatever it shows, instructions
//! included, is data.
//!
//! [`CompletionRequest::project`] is the stable name of the project a call is
//! made for, which a provider passes to its gateway for its records (whether
//! the call may be used for training). It is what the caller says, metadata
//! and never authority; a call made for no project leaves it out, and an
//! invalid one is refused like any invalid field.
//!
//! All four actions are observations: nothing is intentionally mutated, so an
//! interrupted call is reported as `unavailable` or `timeout`. They are not
//! free of consequences: the request is disclosed to the provider, it costs
//! money, and the result is nondeterministic. **Spending is bounded by the
//! `max_calls` of the capability used for `complete` and `start`.** Grant
//! `next` and `cancel` through a separate, unbudgeted capability, or every
//! pull will be charged against the spending limit.
//!
//! Everything a provider returns is untrusted model output. Tool calls in a
//! response are requests; they carry no authority.
//!
//! This crate defines data only. Providers and consumers depend on it; it
//! depends on no provider SDK. Model types live here rather than in
//! `nucleus-protocol`, the kernel's only dependency, so the kernel carries no
//! model vocabulary.

use std::collections::BTreeMap;

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, ResourcePath,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

/// Conventional endpoint name.
pub const ENDPOINT: &str = "model";
pub const ACTION_COMPLETE: &str = "complete";
pub const ACTION_START: &str = "start";
pub const ACTION_NEXT: &str = "next";
pub const ACTION_CANCEL: &str = "cancel";
/// Interface version declared by providers.
pub const VERSION: u32 = 8;

/// Longest queue lease one `next` may report ([`QueueStatus`]).
pub const MAX_QUEUE_LEASE_MS: u64 = 30_000;

/// Longest a single `next` may wait for text before returning empty-handed.
pub const MAX_NEXT_WAIT_MS: u64 = 2_000;

/// Upper bounds a provider must enforce on requests it accepts.
pub const MAX_TOOLS: usize = 128;

/// The image formats a request may carry, by media type.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
/// Largest image, in bytes before base64. A whole request travels in one
/// kernel frame (`nucleus_protocol::MAX_FRAME_BYTES`, 8 MiB) with its text,
/// so an image this size still fits beside a long conversation.
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Most images in one request.
pub const MAX_REQUEST_IMAGES: usize = 16;
/// Most base64 of all of a request's images together: room for one image of
/// [`MAX_IMAGE_BYTES`], and the rest of the frame for the conversation.
pub const MAX_REQUEST_IMAGE_DATA_BYTES: usize = 6 * 1024 * 1024;

/// The media type the first bytes of an image show, if it is one of
/// [`IMAGE_MEDIA_TYPES`]: what an image is, whatever it was called.
#[must_use]
pub fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// Length of the base64 (standard, padded) of `bytes` bytes.
#[must_use]
pub const fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// The declaration a provider passes to `Provide`.
pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: [ACTION_COMPLETE, ACTION_START, ACTION_NEXT, ACTION_CANCEL]
            .iter()
            .map(|action| Ok((ActionName::new(*action)?, ActionDecl::internal(EffectClass::Observation))))
            .collect::<Result<BTreeMap<_, _>, Error>>()?,
    })
}

/// Resource path for a provider model id such as `deepseek/deepseek-v4-flash`.
pub fn resource_for(model_id: &str) -> Result<ResourcePath, Error> {
    ResourcePath::new(model_id.split('/'))
}

/// Provider model id for a resource path. Empty paths name no model.
pub fn model_id(resource: &ResourcePath) -> Result<String, Error> {
    if resource.segments().is_empty() {
        return Err(Error::new(ErrorCode::InvalidInput, "the resource path must name a model"));
    }
    Ok(resource.segments().join("/"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool invocation requested by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed arguments. Providers that receive unparseable argument text
    /// return it as a JSON string so nothing is silently dropped.
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Only on assistant messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Only on tool messages: the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Only on assistant messages: the provider's opaque record of the
    /// reasoning behind this message (OpenRouter's `reasoning_details`),
    /// returned by the provider and handed back **verbatim** on every later
    /// request of the same conversation, so a reasoning model can continue
    /// its own chain of thought across tool calls. Untrusted model output
    /// like the content; no provider reads it as anything but its own prior
    /// state. Absent when the provider sent none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Value>,
    /// Only on assistant messages: the provider's readable account of the
    /// thinking behind this message. Kept for the record and an independent
    /// reasoning display; it is **not** sent back: `reasoning_details` is.
    /// Untrusted model output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Only on user messages: what follows `content`, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
}

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_details: None,
            reasoning: None,
            parts: Vec::new(),
        }
    }
}

/// Part of a user message after its `content`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Part {
    Text {
        text: String,
    },
    /// An image: one of [`IMAGE_MEDIA_TYPES`], its bytes in standard padded
    /// base64.
    Image {
        media_type: String,
        data_base64: String,
    },
}

impl Part {
    /// The base64 of an image part; `None` for text.
    #[must_use]
    pub fn image_data(&self) -> Option<&str> {
        match self {
            Self::Image { data_base64, .. } => Some(data_base64),
            Self::Text { .. } => None,
        }
    }
}

/// Whether `data` is the standard padded base64 of an image of at most
/// [`MAX_IMAGE_BYTES`] whose first bytes show it is a `media_type`.
fn valid_image(media_type: &str, data: &str) -> bool {
    valid_image_data(media_type, data, MAX_IMAGE_BYTES)
}

/// Whether `data` is the standard padded base64 of an image of at most
/// `most` bytes whose first bytes show it is a `media_type`, one of
/// [`IMAGE_MEDIA_TYPES`]: what a request's image part must be, and what
/// another interface that carries an image in base64 checks with its own
/// bound.
#[must_use]
pub fn valid_image_data(media_type: &str, data: &str, most: usize) -> bool {
    use base64::Engine;
    let alphabet = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'/';
    let body = data.trim_end_matches('=');
    let padding = data.len() - body.len();
    let first = data.get(..16.min(data.len())).unwrap_or_default();
    IMAGE_MEDIA_TYPES.contains(&media_type)
        && !data.is_empty()
        && data.len().is_multiple_of(4)
        && padding <= 2
        && data.len() / 4 * 3 - padding <= most
        && body.bytes().all(alphabet)
        && base64::engine::general_purpose::STANDARD
            .decode(first)
            .is_ok_and(|head| image_media_type(&head) == Some(media_type))
}

/// A tool the model may ask for. `parameters` is a JSON Schema object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// How much a reasoning model should think before answering. Providers that
/// have no such knob ignore it. An explicit `max_output_tokens` budget includes
/// thinking plus answer. Without one, the provider chooses the output budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// No reasoning at all (OpenRouter `reasoning.enabled: false`).
    Off,
    /// Reasoning on, at the provider's default depth, stated rather than
    /// assumed: an absent field leaves the provider's default, this one asks
    /// for it (OpenRouter `reasoning.enabled: true`).
    On,
    Low,
    Medium,
    High,
    Max,
    /// Above `max`, where the provider offers it.
    Xhigh,
}

impl ReasoningEffort {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            "xhigh" => Some(Self::Xhigh),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::Xhigh => "xhigh",
        }
    }
}

/// A reasoning effort as a person or an agent sets it for an agent (a
/// session's, a swarm's members): `default` sends nothing, so the provider
/// decides, and the others ask for that [`ReasoningEffort`]. `on` explicitly
/// enables reasoning without requesting a particular depth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortSetting {
    #[default]
    Default,
    On,
    Off,
    Low,
    Medium,
    High,
    Max,
    Xhigh,
}

/// The session harness strategy, independent of provider reasoning effort.
/// Never sent as part of a model completion request or inherited by swarm members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    #[default]
    Standard,
    Ultramath,
}

impl EffortSetting {
    /// Every setting.
    pub const ALL: [Self; 8] =
        [Self::Default, Self::On, Self::Off, Self::Low, Self::Medium, Self::High, Self::Max, Self::Xhigh];

    /// The setting whose requests carry `effort`: `default` for none.
    #[must_use]
    pub fn of(effort: Option<ReasoningEffort>) -> Option<Self> {
        Self::ALL.into_iter().find(|setting| setting.effort() == effort)
    }

    /// What a completion request carries for it: nothing for `default`.
    #[must_use]
    pub fn effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Default => None,
            Self::On => Some(ReasoningEffort::On),
            Self::Off => Some(ReasoningEffort::Off),
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::Max => Some(ReasoningEffort::Max),
            Self::Xhigh => Some(ReasoningEffort::Xhigh),
        }
    }
}

/// Payload of `complete`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// Optional caller budget for thinking plus answer. Zero (or absent) leaves
    /// the output budget to the provider; this interface imposes no token ceiling.
    #[serde(default)]
    pub max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional; absent means the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The tool the model must call. Absent is the provider's default: the
    /// model chooses whether to call one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// The project the call is made for: its stable name
    /// ([`nucleus_protocol::valid_project`]), the platform's and the VM
    /// folder's. Absent for a call made for no project. Metadata for the
    /// gateway's records, never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// What the caller requires of the model's next turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model must call this tool. It must be one of `tools`.
    Named(String),
}

impl CompletionRequest {
    /// Structural limits every provider applies before doing any work.
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |why: &str| Err(Error::new(ErrorCode::InvalidInput, why.to_owned()));
        if self.messages.is_empty() {
            return invalid("messages must not be empty");
        }
        if self.tools.len() > MAX_TOOLS {
            return invalid("too many tools");
        }
        if self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return invalid("temperature out of range");
        }
        match &self.tool_choice {
            // A requirement to call a tool with no tool to call is a request
            // no provider can serve; refuse it here rather than spend a round
            // trip to learn it.
            Some(_) if self.tools.is_empty() => {
                return invalid("tool_choice requires at least one tool");
            }
            Some(ToolChoice::Named(name)) if !self.tools.iter().any(|t| &t.name == name) => {
                return invalid("tool_choice names a tool this request does not offer");
            }
            _ => {}
        }
        if self.project.as_deref().is_some_and(|project| !nucleus_protocol::valid_project(project)) {
            return invalid(
                "project must be 1-40 lowercase letters, digits and '-', starting with a letter or digit",
            );
        }
        for message in &self.messages {
            let consistent = match message.role {
                Role::Tool => {
                    message.tool_call_id.is_some()
                        && message.tool_calls.is_empty()
                        && message.reasoning_details.is_none()
                        && message.reasoning.is_none()
                }
                Role::Assistant => message.tool_call_id.is_none(),
                Role::System | Role::User => {
                    message.tool_call_id.is_none()
                        && message.tool_calls.is_empty()
                        && message.reasoning_details.is_none()
                        && message.reasoning.is_none()
                }
            };
            if !consistent || (message.role != Role::User && !message.parts.is_empty()) {
                return invalid("message fields do not match its role");
            }
        }
        let images: Vec<(&str, &str)> = self
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                Part::Image { media_type, data_base64 } => Some((media_type.as_str(), data_base64.as_str())),
                Part::Text { .. } => None,
            })
            .collect();
        if images.len() > MAX_REQUEST_IMAGES
            || images.iter().map(|(_, data)| data.len()).sum::<usize>() > MAX_REQUEST_IMAGE_DATA_BYTES
        {
            return invalid("too many images, or too many bytes of them, in one request");
        }
        if !images.iter().all(|(media_type, data)| valid_image(media_type, data)) {
            return invalid(
                "an image must be a PNG, JPEG, WebP or GIF of at most 4 MiB, in standard base64, whose bytes \
                 are what its media type says",
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// What the provider says this one call cost, in nano-USD (USD × 10⁹), as
    /// it reports it — not a figure derived here from token counts and a price
    /// table.
    ///
    /// Zero when the provider reports no cost, which is not a claim that the
    /// call was free.
    #[serde(default)]
    pub cost_nano_usd: u64,
}

/// Result of `complete`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// Always an assistant message.
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    /// The model the provider reports having used; may differ from the request
    /// when the provider routes or aliases.
    pub served_by: String,
}

/// Result of `start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartResponse {
    /// Names the stream in `next` and `cancel`. Meaningful only to the process
    /// that started it and only at the provider that issued it.
    pub stream: String,
}

/// Payload of `next`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextRequest {
    pub stream: String,
    /// How long to wait for text, capped at [`MAX_NEXT_WAIT_MS`].
    pub wait_ms: u64,
}

/// Result of `next`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NextResponse {
    /// Text produced since the previous `next`. May be empty.
    pub delta: String,
    /// Provider-supplied readable reasoning since the previous `next`, kept
    /// separate from the answer. Never synthesized from opaque or encrypted
    /// `reasoning_details`. Empty means no display text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning_delta: String,
    /// Present in the call that ends the stream: the whole completion. The
    /// stream is gone afterwards.
    pub done: Option<CompletionResponse>,
    /// Bytes the provider has produced for this stream so far, **including
    /// the ones that never appear in `delta`** — a reasoning model's thinking
    /// above all. The only way a puller can tell a provider that is thinking
    /// from one that has stopped answering: compare successive values, not
    /// `delta`. Unchanged means silent, rising means thinking.
    #[serde(default)]
    pub produced: u64,
    /// The stream is not dispatched yet and waits in a queue.
    /// `None` means not known to be queued (dispatched, or never observed),
    /// which is no evidence of progress either. Model output, dispatch and a
    /// new attempt end the lease; a lease changes neither `delta`,
    /// `produced`, usage nor the completion's deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueStatus>,
}

/// A queue lease: how long, from this reply, the puller may treat the
/// stream's silence as queueing. Control state, not model output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueStatus {
    /// At most [`MAX_QUEUE_LEASE_MS`]. Zero means an earlier lease expired
    /// without renewal: silence from then on counts as silence again. The
    /// puller also lets a lease expire on its own clock.
    pub valid_for_ms: u64,
}

impl QueueStatus {
    /// The lease is within [`MAX_QUEUE_LEASE_MS`]. A consumer refuses a
    /// longer one rather than trusting it.
    pub fn validate(&self) -> Result<(), Error> {
        if self.valid_for_ms > MAX_QUEUE_LEASE_MS {
            return Err(Error::new(
                ErrorCode::ServiceError,
                format!("queue lease {} ms exceeds {MAX_QUEUE_LEASE_MS} ms", self.valid_for_ms),
            ));
        }
        Ok(())
    }
}

/// Payload of `cancel`. Cancelling an unknown or finished stream succeeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub stream: String,
}

/// Result of `cancel`. `released` is true once the provider's
/// reader and network attempt for the stream have stopped; an unknown,
/// finished or another caller's stream answers the same, without saying
/// which. False when the release was not observed in time: the stream's
/// outcome is then unknown, and the caller does not retry it automatically.
/// Required: a reply without it does not satisfy this contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelResponse {
    pub released: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_queue_lease_is_distinct_from_an_expired_one_and_bounded() -> Result<(), serde_json::Error> {
        let mut reply: NextResponse = serde_json::from_value(json!({"delta":"", "done":null, "produced":0}))?;
        assert_eq!(reply.queue, None, "a reply without a lease is not queued");
        assert!(serde_json::to_value(&reply)?.get("queue").is_none());
        reply.queue = Some(QueueStatus { valid_for_ms: 0 });
        let encoded = serde_json::to_value(&reply)?;
        assert_eq!(encoded["queue"], json!({"valid_for_ms": 0}));
        assert_eq!(serde_json::from_value::<NextResponse>(encoded)?.queue, reply.queue);
        assert!(QueueStatus { valid_for_ms: MAX_QUEUE_LEASE_MS }.validate().is_ok());
        assert!(QueueStatus { valid_for_ms: MAX_QUEUE_LEASE_MS + 1 }.validate().is_err());
        assert!(serde_json::from_value::<QueueStatus>(json!({"valid_for_ms": 1, "position": 3})).is_err());
        Ok(())
    }

    #[test]
    fn cancellation_requires_an_explicit_release_acknowledgement() {
        assert!(serde_json::from_value::<CancelResponse>(json!({})).is_err(), "a release is never implied");
        assert!(
            serde_json::from_value::<CancelResponse>(json!({"released": true, "billed": false})).is_err()
        );
        assert_eq!(
            serde_json::from_value::<CancelResponse>(json!({"released": false})).ok(),
            Some(CancelResponse { released: false })
        );
    }

    #[test]
    fn readable_reasoning_is_an_optional_addition_to_stream_replies() -> Result<(), serde_json::Error> {
        let plain = serde_json::json!({"delta":"answer","done":null,"produced":6});
        let mut reply: NextResponse = serde_json::from_value(plain.clone())?;
        assert!(reply.reasoning_delta.is_empty());
        assert_eq!(serde_json::to_value(&reply)?, plain);
        reply.reasoning_delta = "Thinking first.".into();
        assert_eq!(serde_json::to_value(&reply)?["reasoning_delta"], "Thinking first.");
        Ok(())
    }

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            messages,
            tools: vec![],
            max_output_tokens: 64,
            temperature: None,
            reasoning_effort: None,
            tool_choice: None,
            project: None,
        }
    }

    #[test]
    fn model_ids_and_resource_paths_round_trip() -> Result<(), Error> {
        let resource = resource_for("deepseek/deepseek-v4-flash:batch")?;
        assert_eq!(resource.segments(), ["deepseek", "deepseek-v4-flash:batch"]);
        assert_eq!(model_id(&resource)?, "deepseek/deepseek-v4-flash:batch");
        assert!(resource.is_within(&resource_for("deepseek")?));
        assert!(!resource_for("deepseek-evil/x")?.is_within(&resource_for("deepseek")?));
        assert!(resource_for("deepseek/../openai").is_err());
        assert!(model_id(&ResourcePath::root()).is_err());
        Ok(())
    }

    #[test]
    fn requests_are_structurally_valid_and_role_consistent() {
        assert!(request(vec![Message::text(Role::User, "hi")]).validate().is_ok());
        assert!(request(vec![]).validate().is_err());
        assert!(request(vec![Message::text(Role::User, "short"); 5_000]).validate().is_ok());
        let mut unbounded = request(vec![Message::text(Role::User, "hi")]);
        unbounded.max_output_tokens = 0;
        assert!(unbounded.validate().is_ok());
        let mut orphan_tool = Message::text(Role::Tool, "result");
        assert!(request(vec![orphan_tool.clone()]).validate().is_err());
        orphan_tool.tool_call_id = Some("call_1".to_owned());
        assert!(request(vec![orphan_tool]).validate().is_ok());
        let mut forged = Message::text(Role::User, "hi");
        forged.tool_call_id = Some("call_1".to_owned());
        assert!(request(vec![forged]).validate().is_err());
        let longest = "a".repeat(40);
        let too_long = "a".repeat(41);
        for (project, valid) in [
            ("alpha-1", true),
            ("0", true),
            (longest.as_str(), true),
            (too_long.as_str(), false),
            ("", false),
            ("-alpha", false),
            ("Alpha", false),
            ("a_b", false),
            ("a/b", false),
        ] {
            let mut tagged = request(vec![Message::text(Role::User, "hi")]);
            tagged.project = Some(project.to_owned());
            assert_eq!(tagged.validate().is_ok(), valid, "{project:?}");
        }
    }

    /// Settings carry the requested control, or nothing for `default`.
    #[test]
    fn an_effort_setting_carries_only_its_requested_control() {
        assert_eq!(
            serde_json::to_value(EffortSetting::ALL).ok(),
            Some(json!(["default", "on", "off", "low", "medium", "high", "max", "xhigh"]))
        );
        for setting in EffortSetting::ALL {
            let name = serde_json::to_value(setting).ok();
            let carried = setting.effort().map(|effort| json!(effort.as_str()));
            assert_eq!(carried, name.filter(|name| name != "default"), "{setting:?}");
            assert_eq!(EffortSetting::of(setting.effort()), Some(setting));
        }
        for refused in [json!("ultra"), json!("MAX"), json!(""), json!(3), json!(null)] {
            assert!(serde_json::from_value::<EffortSetting>(refused.clone()).is_err(), "{refused}");
        }
    }

    #[test]
    fn output_budgets_are_optional_and_have_no_model_independent_ceiling() -> Result<(), serde_json::Error> {
        let mut call = request(vec![Message::text(Role::User, "hi")]);
        for budget in [0, 65_537, 393_216, u32::MAX] {
            call.max_output_tokens = budget;
            assert!(call.validate().is_ok());
        }
        let decoded: CompletionRequest = serde_json::from_value(json!({
            "messages": [{"role": "user", "content": "hi"}]
        }))?;
        assert_eq!(decoded.max_output_tokens, 0);
        assert!(decoded.validate().is_ok());
        Ok(())
    }

    /// Images ride in user messages only, as parts after the text; a message
    /// without parts reads and writes without the field; an image is bounded
    /// and must be what its media type says.
    #[test]
    fn images_are_bounded_parts_of_user_messages() -> Result<(), serde_json::Error> {
        use base64::Engine;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let png = encode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let image = |media_type: &str, data: &str| Part::Image {
            media_type: media_type.to_owned(),
            data_base64: data.to_owned(),
        };
        let mut user = Message::text(Role::User, "what is this?");
        user.parts = vec![Part::Text { text: "plot.png:".into() }, image("image/png", &png)];
        let encoded = serde_json::to_value(&user)?;
        assert_eq!(
            encoded["parts"][1],
            json!({"type": "image", "media_type": "image/png", "data_base64": png})
        );
        assert_eq!(serde_json::from_value::<Message>(encoded)?, user);
        let plain = json!({"role": "user", "content": "hi"});
        assert_eq!(serde_json::to_value(serde_json::from_value::<Message>(plain.clone())?)?, plain);
        assert!(serde_json::from_value::<Part>(json!({"type": "image_url", "url": "https://x"})).is_err());
        assert!(request(vec![user.clone()]).validate().is_ok());

        let mut tool = Message::text(Role::Tool, "done");
        tool.tool_call_id = Some("call_1".into());
        tool.parts = user.parts.clone();
        assert!(request(vec![tool]).validate().is_err(), "a tool result stays text");
        let jpeg_named_png = encode(b"\xff\xd8\xff\xe0\0\x10JFIF\0");
        let too_large = "A".repeat(base64_len(MAX_IMAGE_BYTES) + 4);
        for (media_type, data) in [
            ("image/png", jpeg_named_png.as_str()),
            ("image/svg+xml", png.as_str()),
            ("image/png", "iVBORw0KGgo=AAAA"),
            ("image/png", too_large.as_str()),
            ("image/png", ""),
        ] {
            let mut bad = user.clone();
            bad.parts = vec![image(media_type, data)];
            assert!(request(vec![bad]).validate().is_err(), "{media_type} {:.20}", data);
        }
        let mut many = user.clone();
        many.parts = vec![image("image/png", &png); MAX_REQUEST_IMAGES + 1];
        assert!(request(vec![many]).validate().is_err());
        let big = encode(&[b"\x89PNG\r\n\x1a\n".as_slice(), &vec![0; MAX_IMAGE_BYTES - 8]].concat());
        let mut heavy = user;
        heavy.parts = vec![image("image/png", &big), image("image/png", &big)];
        assert!(request(vec![heavy.clone()]).validate().is_err(), "past the request's image bytes");
        heavy.parts.pop();
        assert!(request(vec![heavy]).validate().is_ok(), "one image of the largest size fits");
        Ok(())
    }

    #[test]
    fn image_media_types_come_from_the_bytes() {
        let webp = [b"RIFF".as_slice(), &[0; 4], b"WEBPVP8 "].concat();
        for (bytes, expected) in [
            (b"\x89PNG\r\n\x1a\nrest".as_slice(), Some("image/png")),
            (b"\xff\xd8\xff\xdb", Some("image/jpeg")),
            (b"GIF89a..", Some("image/gif")),
            (b"GIF87a..", Some("image/gif")),
            (webp.as_slice(), Some("image/webp")),
            (b"RIFF\0\0\0\0WAVE", None),
            (b"%PDF-1.7", None),
            (b"", None),
        ] {
            assert_eq!(image_media_type(bytes), expected, "{bytes:?}");
        }
    }

    #[test]
    fn unknown_request_fields_are_rejected() {
        let json = r#"{"messages":[{"role":"user","content":"hi"}],"max_output_tokens":8,"api_key":"x"}"#;
        assert!(serde_json::from_str::<CompletionRequest>(json).is_err());
    }
}
