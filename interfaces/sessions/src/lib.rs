//! The `sessions` service interface, version 10.
//!
//! A session is one conversation with one working agent, in one project.
//! The resource path names it: `["default", "s1"]`, its project and its
//! name, as in the `conversation` interface. `list` is called on the root
//! path, for every project's sessions, or on `[project]` for one project's,
//! with a capability that covers them. It is paged: by project and session
//! name, from `after` (the `next` of the page before), at most `limit`
//! sessions ([`DEFAULT_LIST`] when absent, at most [`MAX_LIST`]); `next` is
//! absent at the end.
//!
//! | Action | Effect class | Payload | Result |
//! | --- | --- | --- | --- |
//! | `create` | compensatable (by `stop`) | [`CreateRequest`] | [`SessionInfo`] |
//! | `stop` | compensatable (by `create`) | `{}` | `{}` |
//! | `interrupt` | irreversible | `{}` | `{}` |
//! | `list` | observation | [`ListRequest`] | [`ListResponse`] |
//! | `forget` | irreversible | `{}` | `{}` |
//!
//! `create` starts the session's agent, which works in its project's
//! repository (`repo/` of the project's folder), as every session does. Stopping a
//! session ends its agent process. Neither operation erases conversation
//! history.
//!
//! `stop` also accepts `[project]` for the UI archive operation: it stops all
//! known sessions, closes turns and clears reminders without erasing history.
//!
//! `interrupt` ends the session's current turn and leaves an agent ready for
//! the next message: the agent is ended and started again with the same
//! model. Both `stop` and `interrupt` close an open turn in the conversation
//! (`interrupted`), so a started agent does not take it up again by itself:
//! like every start, it continues the conversation from its log, which says
//! that the turn was stopped. Interrupting an idle session leaves completed
//! turns unchanged. A session whose agent runs is ended and
//! started again even with no turn under way; one with no agent only has an
//! open turn closed.
//! `stop` also withdraws the session's pending reminders (conversation
//! `clear_reminders`); `interrupt` leaves them, as it leaves queued messages.
//! Both pause reminder delivery until the next user message is delivered:
//! a queued or later background completion cannot resume stopped work.
//! Even if the turn just finished, the stop decision is persisted without
//! marking that completed turn interrupted.
//!
//! `create` names the model the new agent runs, a provider/model id; the
//! provider keeps no list of models to choose from. A `create` that names none
//! gets the model the provider's own configuration gives its agents, and is
//! refused as `invalid_input` when that gives none. A message may name another
//! model for its turn (conversation `MessageSettings`). To change a session's
//! own model, `stop` it and `create` it again with the new one: the new agent
//! continues the conversation from its log.
//!
//! An agent has a reasoning effort, chosen like its model; `on` enables
//! reasoning without a depth. `create` may name one ([`EffortSetting`]),
//! `default` when absent, which leaves the provider's own.
//! [`SessionInfo::reasoning_effort`] says what the session's agent was started
//! with, and `interrupt` starts the next agent with the same. To change it,
//! `stop` the session and `create` it again, naming the model too: a `create`
//! inherits nothing from the agent before it. This service keeps a session's
//! model and effort only while it runs. What outlasts it is the session's
//! conversation, whose `model_call` records say both: a caller starting an
//! agent for a session this service does not list reads them from the last
//! such record and names them.
//!
//! `forget`, on `[project]`, is the UI service's while it deletes the
//! project: every agent of the project's sessions is ended, without closing
//! its turn (its conversation is forgotten next), and the sessions are
//! dropped from what `list` shows. It answers `{}` once no agent of the
//! project runs. A provider that knows the projects' folders starts no agent
//! for a project without one, so none starts again for a project being
//! deleted.

use std::collections::{BTreeMap, BTreeSet};

pub use nucleus_model_interface::{EffortSetting, ReasoningEffort};
use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, ProcessId,
    ResourcePath,
};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "sessions";
pub const ACTION_CREATE: &str = "create";
pub const ACTION_STOP: &str = "stop";
pub const ACTION_INTERRUPT: &str = "interrupt";
pub const ACTION_LIST: &str = "list";
pub const ACTION_FORGET: &str = "forget";
pub const VERSION: u32 = 10;
/// Sessions a `list` page holds when its request names no `limit`, and the
/// most it may name.
pub const DEFAULT_LIST: usize = 50;
pub const MAX_LIST: usize = 200;

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_CREATE)?, ActionDecl::internal(EffectClass::Compensatable)),
            (ActionName::new(ACTION_STOP)?, ActionDecl::internal(EffectClass::Compensatable)),
            (ActionName::new(ACTION_INTERRUPT)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_LIST)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_FORGET)?, ActionDecl::internal(EffectClass::Irreversible)),
        ]),
    })
}

/// Authority a session's agent was started with. Informational: it describes
/// what was delegated, it is not itself a capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantInfo {
    pub endpoint: EndpointName,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub project: String,
    pub name: String,
    /// Whether the session's agent runs.
    pub live: bool,
    pub process: ProcessId,
    pub grants: Vec<GrantInfo>,
    /// The model id the agent was started with, when the provider knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort the agent was started with; `null` is `default`,
    /// the provider's own.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Payload of `list` (module doc). `{}` is valid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl ListRequest {
    /// The page's size, or why the request is refused.
    pub fn validate(&self) -> Result<usize, Error> {
        match self.limit.unwrap_or(DEFAULT_LIST) {
            limit @ 1..=MAX_LIST => Ok(limit),
            _ => Err(Error::new(ErrorCode::InvalidInput, format!("limit is 1 to {MAX_LIST}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResponse {
    /// A page of the sessions whose agents this provider started, by project
    /// and name.
    pub sessions: Vec<SessionInfo>,
    /// The `after` of the next page; absent at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Payload of `create`. `{}` is valid when the provider's configuration gives
/// its agents a model (module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    /// The provider/model id the agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Absent means `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<EffortSetting>,
}
