//! The `publish` service interface, version 2: the secret scan of a
//! session's records, which the platform's mirror reads beside the records
//! themselves: every record is scanned in the VM and its findings travel
//! with it to the platform, which decides what is public.
//!
//! | Action | Effect class | Resource | Payload | Result |
//! | --- | --- | --- | --- | --- |
//! | `findings` | observation | `[project, session]` | [`FindingsRequest`] | [`Findings`] |
//!
//! It is no tool. Only the UI service holds it, for the platform's mirror
//! connection.
//!
//! # `findings`
//!
//! One page of a session's records, as `conversation history` serves them
//! from `after` (exclusive, a `seq`), scanned for secrets with the service's
//! rules ([`RULES_VERSION`]): every string in a record, and every object or
//! array field as its JSON text, where a name and its value stand side by
//! side. The answer lists, by `seq`, the records that hold a finding, and
//! says which records the page covered: every record with `after < seq <=
//! through` was scanned, and a record of that range that is not listed has
//! no finding. `next` is the `after` of the following page; absent, the page
//! reached the end of the log as it was then, and a later call from
//! `through` reads what was written since. A page ends early rather than
//! answer more than [`MAX_PAGE_FINDINGS`] findings, so it may cover fewer
//! records than the history page it read.
//!
//! The platform reads a session's records with `conversation history` and
//! their findings here from the same `after`, again from `next` until
//! `through` reaches the last record it read. Nothing is cut silently: a
//! record with more distinct findings than [`MAX_RECORD_FINDINGS`] lists
//! that many and says `more`, which means "keep it withheld until the owner
//! decides the record as a whole".
//!
//! A [`Finding`] says where a rule matched, never what: `preview` is the
//! text around the match with every matched secret replaced by `****`, and
//! `fingerprint` is the SHA-256, in hexadecimal, of the request's `salt`, the
//! rule and the matched secret, so the platform can release one secret
//! wherever it appears without learning it. The salt is the platform's, one
//! per project, and the VM keeps none.

use std::collections::BTreeMap;

use nucleus_protocol::{ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "publish";
pub const VERSION: u32 = 2;
pub const ACTION_FINDINGS: &str = "findings";
/// Every action.
pub const ACTIONS: [&str; 1] = [ACTION_FINDINGS];

/// The version of the scan rules, in every answer. Bumped when a rule
/// changes, so that the platform scans again what an earlier version scanned.
pub const RULES_VERSION: u32 = 1;

/// Most findings one record lists; past it the record says `more`.
pub const MAX_RECORD_FINDINGS: usize = 16;
/// Most findings one page answers; a page ends early rather than pass it.
pub const MAX_PAGE_FINDINGS: usize = 1000;
/// Longest [`Finding::preview`], in bytes.
pub const MAX_PREVIEW_BYTES: usize = 160;
/// A salt is 16 to 128 lowercase hexadecimal characters.
pub const MIN_SALT_CHARS: usize = 16;
pub const MAX_SALT_CHARS: usize = 128;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

/// Lowercase hexadecimal of exactly `chars` characters: a SHA-256 is 64.
pub fn is_hex(text: &str, chars: usize) -> bool {
    text.len() == chars && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Payload of `findings`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingsRequest {
    /// The project's salt, the platform's: fingerprints are made with it.
    pub salt: String,
    /// Records with a greater `seq`; from the first record when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

impl FindingsRequest {
    pub fn validate(&self) -> Result<(), Error> {
        if !(MIN_SALT_CHARS..=MAX_SALT_CHARS).contains(&self.salt.len())
            || !is_hex(&self.salt, self.salt.len())
        {
            return Err(invalid(format!(
                "the salt is {MIN_SALT_CHARS}-{MAX_SALT_CHARS} lowercase hexadecimal characters"
            )));
        }
        Ok(())
    }
}

/// One rule's match in a record, without the secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The rule's name, one of the publish service's scan rules.
    pub rule: String,
    /// At most [`MAX_PREVIEW_BYTES`], every matched secret masked.
    pub preview: String,
    /// SHA-256 of the salt, the rule and the secret, in hexadecimal.
    pub fingerprint: String,
}

/// The findings of one record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordFindings {
    pub seq: u64,
    /// Each distinct (rule, secret) once, at most [`MAX_RECORD_FINDINGS`].
    pub findings: Vec<Finding>,
    /// The record holds more distinct findings than are listed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

/// Result of `findings` (module doc).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    /// The records of the page that hold a finding, by `seq`.
    pub records: Vec<RecordFindings>,
    /// The `seq` of the last record the page covered; absent when the log
    /// holds no record after `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<u64>,
    /// Pass it as `after` for the next page; absent when the page reached
    /// the end of the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
    /// [`RULES_VERSION`] of the service that scanned.
    pub rules: u32,
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([(
            ActionName::new(ACTION_FINDINGS)?,
            ActionDecl::internal(EffectClass::Observation),
        )]),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    /// The wire shapes the platform writes and reads, and what is refused.
    #[test]
    fn requests_have_one_shape_and_refuse_everything_else() {
        let salt = "0f".repeat(16);
        let request: FindingsRequest = serde_json::from_value(json!({"salt": salt, "after": 7})).unwrap();
        request.validate().unwrap();
        assert_eq!(request.after, Some(7));
        for bad in
            [json!({"salt": "short"}), json!({"salt": salt.to_uppercase()}), json!({"salt": "zz".repeat(16)})]
        {
            let request: FindingsRequest = serde_json::from_value(bad.clone()).unwrap();
            assert!(request.validate().is_err(), "{bad}");
        }
        for unparsed in [
            json!({"after": 1}),
            json!({"salt": salt, "project": "other"}),
            json!({"salt": salt, "after": -1}),
        ] {
            assert!(serde_json::from_value::<FindingsRequest>(unparsed.clone()).is_err(), "{unparsed}");
        }
        let answer = Findings {
            records: vec![RecordFindings {
                seq: 3,
                findings: vec![Finding {
                    rule: "jwt".into(),
                    preview: "token ****".into(),
                    fingerprint: "ab".repeat(32),
                }],
                more: false,
            }],
            through: Some(9),
            next: None,
            rules: RULES_VERSION,
        };
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            json!({"records": [{"seq": 3, "findings": [{"rule": "jwt", "preview": "token ****", "fingerprint": "ab".repeat(32)}]}],
                   "through": 9, "rules": RULES_VERSION})
        );
        assert_eq!(interface(EndpointName::new(ENDPOINT).unwrap()).unwrap().actions.len(), ACTIONS.len());
    }
}
