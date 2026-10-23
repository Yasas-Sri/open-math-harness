//! The `approvals` service interface, version 2: a guard between a process and
//! actions it may only perform with a person's consent.
//!
//! The requester never holds the capability for the protected action. The guard
//! does. A requester holds `request`, and asks the guard to perform one exact
//! call. The guard's policy answers `allow`, `deny`, or `ask`; when it asks, the
//! request waits until a process holding `decide` (a front end) decides it. If
//! approved, **the guard itself makes the call**, with the parameters it
//! recorded, so nothing can change between what was approved and what runs.
//!
//! | Action | Held by | Resource | Payload | Result |
//! | --- | --- | --- | --- | --- |
//! | `request` | agents | `[endpoint, ...target resource]` | [`Request`] | the protected call's own result |
//! | `list` | front ends | `[]`, or a target prefix | [`ListRequest`] | [`ListResponse`] |
//! | `decide` | front ends | `[]`, or a target prefix | [`Decision`] | `{}` |
//! | `forget` | the UI service's deletion | `[project]` | `{}` | `{}` |
//!
//! The resource of `request` starts with the target endpoint, so a requester's
//! capability can be scoped to, say, `["fs", "work"]`: it may ask about files
//! under `work` and about nothing else. `list` and `decide` take the same
//! form: on `[]` every pending request, on `["fs", "projects", "p"]` only the
//! requests whose target is within it, and a request outside it is not there
//! for that `decide`. `list` is paged: oldest first, from `after` (the
//! `next` of the page before, a request's id), at most `limit`
//! ([`DEFAULT_LIST`] when absent, at most [`MAX_LIST`]); `next` is absent at
//! the end.
//!
//! `forget` is the UI service's while it deletes a project. Its resource is
//! the project, not a target: every request about the project's files (a
//! target resource within `["projects", project]`) is answered as refused,
//! and the guard's journal is rewritten without the lines about them, so
//! nothing the project asked for is kept. It answers `{}` once that is done.
//!
//! An approval is bound to the requester (as asserted by the kernel), the
//! target, the exact parameters (through [`Pending::digest`]), and the guard's
//! policy version. A decision must quote the digest it was shown. Approvals are
//! single-use, die with the requester's call, and cannot be given by the
//! requester. `request` is classified by its worst case: irreversible.

use std::collections::BTreeMap;

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, Payload, ProcessId,
    ResourcePath,
};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "approvals";
pub const ACTION_REQUEST: &str = "request";
pub const ACTION_LIST: &str = "list";
pub const ACTION_DECIDE: &str = "decide";
pub const ACTION_FORGET: &str = "forget";
pub const VERSION: u32 = 2;
/// Requests a `list` page holds when its request names no `limit`, and the
/// most it may name.
pub const DEFAULT_LIST: usize = 50;
pub const MAX_LIST: usize = 200;

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_REQUEST)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_LIST)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_DECIDE)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_FORGET)?, ActionDecl::internal(EffectClass::Irreversible)),
        ]),
    })
}

/// Payload of `forget`: the project is the call's resource, `[project]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetRequest {}

/// Payload of `request`. The target endpoint and resource are the call's
/// resource path; this carries the rest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub action: ActionName,
    /// The protected call's payload, passed on untouched if approved.
    pub payload: Payload,
    /// Why, in the requester's words. Shown to the person deciding; untrusted.
    #[serde(default)]
    pub reason: String,
}

/// A request waiting for a decision, as a front end sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub id: u64,
    /// Asserted by the kernel, not claimed by the requester.
    pub requester: ProcessId,
    pub endpoint: EndpointName,
    pub action: ActionName,
    pub resource: ResourcePath,
    pub payload: Payload,
    pub reason: String,
    pub policy_version: u64,
    /// Covers everything above. A decision must quote it.
    pub digest: String,
    pub waiting_ms: u64,
}

/// Payload of `list` (module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListResponse {
    pub pending: Vec<Pending>,
    /// The `after` of the next page; absent at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

/// Payload of `decide`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub id: u64,
    /// The digest of the request as it was shown to the person deciding.
    pub digest: String,
    pub allow: bool,
}

/// The digest an approval is bound to. FNV-1a over a length-prefixed encoding,
/// so no two different requests share an encoding. It guards against mix-ups
/// and changed parameters between showing and deciding; it is not a signature,
/// and nothing relies on it being unguessable.
pub fn digest(
    requester: ProcessId,
    endpoint: &EndpointName,
    action: &ActionName,
    resource: &ResourcePath,
    payload: &Payload,
    policy_version: u64,
) -> String {
    let payload_text = serde_json::to_string(payload).unwrap_or_default();
    let mut parts: Vec<String> = vec![
        requester.0.to_string(),
        endpoint.as_str().to_owned(),
        action.as_str().to_owned(),
        policy_version.to_string(),
        payload_text,
    ];
    parts.extend(resource.segments().iter().cloned());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for part in &parts {
        feed(&(part.len() as u64).to_le_bytes());
        feed(part.as_bytes());
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_changes_with_every_bound_field() -> Result<(), Error> {
        let endpoint = EndpointName::new("fs")?;
        let action = ActionName::new("remove")?;
        let resource = ResourcePath::new(["work", "old.txt"])?;
        let payload = Payload::encode(&serde_json::json!({}))?;
        let base = digest(ProcessId(7), &endpoint, &action, &resource, &payload, 1);
        assert_eq!(base, digest(ProcessId(7), &endpoint, &action, &resource, &payload, 1), "stable");
        let others = [
            digest(ProcessId(8), &endpoint, &action, &resource, &payload, 1),
            digest(ProcessId(7), &EndpointName::new("shell")?, &action, &resource, &payload, 1),
            digest(ProcessId(7), &endpoint, &ActionName::new("create")?, &resource, &payload, 1),
            digest(ProcessId(7), &endpoint, &action, &ResourcePath::new(["work", "old.txt2"])?, &payload, 1),
            digest(
                ProcessId(7),
                &endpoint,
                &action,
                &resource,
                &Payload::encode(&serde_json::json!({"x": 1}))?,
                1,
            ),
            digest(ProcessId(7), &endpoint, &action, &resource, &payload, 2),
            // Segment boundaries matter: ["work", "old.txt"] is not ["workold.txt"].
            digest(ProcessId(7), &endpoint, &action, &ResourcePath::new(["workold.txt"])?, &payload, 1),
        ];
        for other in others {
            assert_ne!(base, other);
        }
        Ok(())
    }
}
