//! The `suggestions` service interface, version 2: the message a person might
//! send next in a session, which a window offers in an empty composer, and a
//! short title for a session, which a window saves as the chat's title when
//! it has none.
//!
//! The provider is a microservice of its own, beside the sessions. The caller
//! sends the session's messages so far (the window has them); the provider
//! asks its own model for the next one. Per session at most one request is
//! being worked on per action: a newer request for the same session and
//! action interrupts the older one, whose model call is cancelled and which
//! is answered with no text (superseded), so work is always for the latest
//! messages. A `next` never displaces a `title` of the same session, nor the
//! other way. A request with the same messages as the one in progress
//! (another window of the session) waits for that one's answer instead.
//! Sessions do not affect each other.
//!
//! It reads nothing and writes nothing but the model call, holds no
//! capability of any session, and no session's agent knows it exists: a slow,
//! failing or missing suggestions service changes nothing else, and its model
//! budget is its own. A suggestion is untrusted model text; the person decides
//! whether to send it, and a window offers it only while the messages it was
//! written for are still the last ones.
//!
//! | Action | Payload | Result |
//! | --- | --- | --- |
//! | `next` | [`NextRequest`] on `[project, session]` | [`Suggestion`] for that session |
//! | `title` | [`TitleRequest`] on `[project, session]` | [`Title`] for that session |
//!
//! `next` is an observation (it spends the provider's model budget) and not a
//! tool. An empty `messages` asks for an opening. `text` is `None` when there
//! is nothing to offer: superseded, cancelled by the caller, the model gave
//! nothing usable, or the provider is at its limit of requests in progress.
//!
//! `title` is the same kind of observation. An empty `messages` has nothing
//! to title and is answered with no title, without a model call. `title` is
//! untrusted model text, like a suggestion; the window that saves it checks
//! it against the title rules of the platform first.

use nucleus_protocol::{ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "suggestions";
pub const ACTION_NEXT: &str = "next";
pub const ACTION_TITLE: &str = "title";
pub const VERSION: u32 = 2;

/// Longest suggestion, in bytes.
pub const MAX_SUGGESTION_BYTES: usize = 1024;
/// Longest title, in bytes.
pub const MAX_TITLE_BYTES: usize = 256;
/// Most messages in one request; send the latest.
pub const MAX_MESSAGES: usize = 40;
/// Longest message text, in bytes; cut a longer one before sending.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// Most bytes of message text in one request.
pub const MAX_TOTAL_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// One message of the conversation, as the person saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// Payload of `next`: the session's messages so far, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextRequest {
    pub messages: Vec<Message>,
    /// Available skill metadata; never instructions or authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

pub const MAX_SKILLS: usize = 32;
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 2400;
pub const MAX_RECOMMENDED_SKILLS: usize = 2;

fn within_bounds(messages: &[Message]) -> Result<(), Error> {
    let total: usize = messages.iter().map(|m| m.text.len()).sum();
    if messages.len() > MAX_MESSAGES
        || messages.iter().any(|m| m.text.len() > MAX_MESSAGE_BYTES)
        || total > MAX_TOTAL_BYTES
    {
        return Err(Error::new(ErrorCode::InvalidInput, "messages exceed the request's bounds"));
    }
    Ok(())
}

impl NextRequest {
    pub fn validate(&self) -> Result<(), Error> {
        within_bounds(&self.messages)?;
        let mut seen = std::collections::BTreeSet::new();
        if self.skills.len() > MAX_SKILLS
            || self.skills.iter().any(|skill| {
                skill.name.len() > 64
                    || !skill.name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                    || !skill.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                    || skill.description.len() > MAX_SKILL_DESCRIPTION_BYTES
                    || !seen.insert(&skill.name)
            })
        {
            return Err(Error::new(ErrorCode::InvalidInput, "invalid skill candidates"));
        }
        Ok(())
    }
}

/// Payload of `title`: the caller sends the first accepted user message, without
/// waiting for an assistant response. Validation bounds the transcript's size.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleRequest {
    pub messages: Vec<Message>,
}

impl TitleRequest {
    pub fn validate(&self) -> Result<(), Error> {
        within_bounds(&self.messages)
    }
}

/// Result of `next`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The session asked about, the request's resource: its project and name.
    pub project: String,
    pub session: String,
    /// One line the person might send, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// At most two names from the request's candidates; selection sends nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

/// Result of `title`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Title {
    /// The session asked about, the request's resource: its project and name.
    pub project: String,
    pub session: String,
    /// A short title in the conversation's language, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_NEXT)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_TITLE)?, ActionDecl::internal(EffectClass::Observation)),
        ]),
    })
}
