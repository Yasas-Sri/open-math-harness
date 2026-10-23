//! The `memory` service interface, version 4: what a session is told it
//! remembers, which of it the session used, and when a session is worth
//! reading back.
//!
//! The **resource path is the session**, `[project, session]` as it is for
//! `conversation`, so which sessions a process may read memory for, report
//! against, or hand back is an ordinary capability scope. An agent holds this
//! for its own session and can say nothing about anyone else's. Each project
//! remembers apart: a session is told what its own project's sessions left.
//!
//! | Action | Effect class | Resource | Payload | Result | Held by |
//! | --- | --- | --- | --- | --- | --- |
//! | `index` | observation | `[project, session]` | `{}` | [`IndexResponse`] | an agent, at its start |
//! | `used` | reversible mutation | `[project, session]` | [`UsedRequest`] | `{}` | an agent, after an answer |
//! | `remember` | reversible mutation | `[project, session]` | `{}` | `{}` | an agent, after a turn |
//! | `contents` | observation | `[project]` | `{}` | [`ContentsResponse`] | a front end |
//! | `stop` | irreversible | `[project]` | `{}` | `{}` | the UI service's archive |
//! | `forget` | irreversible | `[project]` | `{}` | `{}` | the UI service's deletion |
//!
//! `contents` is what a person reads: one project's consolidated file itself,
//! without the framing a session is given, and what the service is doing with
//! it. It is answered **only on a project's path**, so a capability scoped to
//! one session cannot call it at all -- an agent is told what it remembers,
//! and does not get to read the system's memory as an object. A front end
//! holds the projects' scope and can.
//!
//! `stop` on `[project]` stops its consolidation and waits for cleanup, preserving
//! memory and job records. With project folders configured, archived projects
//! run no extraction or consolidation.
//!
//! `forget` is the UI service's while it deletes a project: the project's
//! consolidation is stopped, what waited for extraction is dropped, and its
//! workspace and job store are removed; it answers `{}` once nothing of the
//! project's memory is left. `contents` of a project with no memory says so
//! without making one.
//!
//! None of them is tool-callable. Reading memory is something a session does
//! at its start, not something a model decides to do; reporting what was used
//! is the harness's bookkeeping *about* the model, and a model that could
//! choose what to report could keep a memory alive by citing it; and
//! `remember` says only that the session moved on, which the agent knows and
//! the model has no reason to be asked about.
//!
//! What this interface does not have is a way to write a memory. Memories are
//! extracted from sessions by the service and consolidated by an agent it
//! starts with write access to the memory workspace and nothing else. No
//! conversation can add to what the system remembers by asking it to.

use std::collections::BTreeMap;

use nucleus_protocol::{ActionDecl, ActionName, EffectClass, EndpointName, Error, InterfaceDecl};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "memory";
pub const ACTION_INDEX: &str = "index";
pub const ACTION_USED: &str = "used";
pub const ACTION_REMEMBER: &str = "remember";
pub const ACTION_CONTENTS: &str = "contents";
pub const ACTION_FORGET: &str = "forget";
pub const ACTION_STOP: &str = "stop";
pub const VERSION: u32 = 4;

/// Largest injection one `index` returns. What is remembered is read at the
/// start of every session, so its size is a tax on every conversation. Past
/// this the consolidation has stopped doing its job, and the answer is to say
/// so at the cut rather than to quietly send more.
pub const MAX_INDEX_BYTES: usize = 32 * 1024;

/// Most paths one `used` call may name. A reply cites what it used; a reply
/// naming more memories than this is not reporting, it is padding.
pub const MAX_USED_PATHS: usize = 64;

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_INDEX)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_USED)?, ActionDecl::internal(EffectClass::ReversibleMutation)),
            (ActionName::new(ACTION_REMEMBER)?, ActionDecl::internal(EffectClass::ReversibleMutation)),
            (ActionName::new(ACTION_CONTENTS)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_STOP)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_FORGET)?, ActionDecl::internal(EffectClass::Irreversible)),
        ]),
    })
}

/// What to put in front of the model, already framed as data rather than as
/// instruction.
///
/// `text` is empty when nothing is remembered yet. An agent given an empty
/// index injects nothing: a heading over an empty list teaches a model that
/// this system remembers nothing useful, which is a worse start than silence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexResponse {
    pub text: String,
}

/// What one session used: one entry per memory, however many times it was
/// cited, because a model citing the same line five times has not made it five
/// times more useful.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsedRequest {
    pub paths: Vec<String>,
}

/// Payload of `remember`: this session has moved on and is worth reading back
/// once it goes quiet. It carries nothing — which session it is, is the
/// resource path, and when it was is now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RememberRequest {}

/// What is remembered, for a person to read, and what the service is doing.
///
/// `index` here is the consolidated file as the agent that maintains it wrote
/// it -- not [`IndexResponse::text`], which wraps the same content in the
/// framing a model needs. A person reading their own memory does not need to
/// be told it is untrusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentsResponse {
    pub index: String,
    /// When that file was last written, unix ms. Zero when nothing is
    /// remembered yet, which is not the same as "written at the epoch".
    pub updated_at_ms: u64,
    /// Sessions successful consolidations have worked from.
    pub memories: u64,
    /// Sessions that have said they moved on and have not been read back yet.
    pub waiting: u64,
    /// A consolidation is running now, so the index may change under the
    /// reader.
    pub consolidating: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn no_action_is_offered_to_a_model_as_a_tool() {
        let decl = interface(EndpointName::new("memory").expect("name")).expect("interface");
        for (action, spec) in &decl.actions {
            assert!(
                spec.tool.is_none(),
                "`{action}` must not be tool-callable: a model that chose what to report could keep \
                 a memory alive by citing it"
            );
        }
    }

    #[test]
    fn there_is_no_action_that_writes_a_memory() {
        let decl = interface(EndpointName::new("memory").expect("name")).expect("interface");
        let names: Vec<&str> = decl.actions.keys().map(ActionName::as_str).collect();
        // `stop` cancels work and `forget` removes memory; neither writes a memory.
        assert_eq!(names, ["contents", "forget", "index", "remember", "stop", "used"]);
    }

    #[test]
    fn an_empty_index_is_empty_rather_than_a_heading() {
        assert_eq!(IndexResponse::default().text, "");
    }
}
