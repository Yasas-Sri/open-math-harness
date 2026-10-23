//! The `jobs` interface, version 2: a session's background shell jobs. The
//! projects service provides it on `[project, session]`.
//!
//! | Action | Payload | Result | Who |
//! | --- | --- | --- | --- |
//! | `register` | `{}` | `{}` | the session's agent, before each command |
//! | `list` | [`ListRequest`] | [`ListResponse`] | the session's agent |
//! | `output` | [`OutputRequest`](crate::OutputRequest) | [`OutputResponse`] | the session's agent |
//! | `stop` | [`StopRequest`] | [`JobResponse`] | the session's agent |
//! | `background` | [`BackgroundRequest`] | [`JobResponse`] | a person, in the window |
//!
//! `list` and `output` reach every job of the session's project, so an agent
//! sees what any of its project's sessions left running. A job is stopped
//! through its own session only: a session's agent, whose capability covers
//! that session alone, stops only its own jobs.
//!
//! **Who moved a job is what the kernel checked**, never the request:
//! `background` is a person's, and only the UI service holds it. So the
//! person a job's record names (`author`, the account id of the window's
//! label, never a nickname) comes from the UI service alone. A record may
//! name a person who stopped the job (`stopped_by`, `stopped_author`).
//!
//! `register` says the caller is the session's agent: the kernel checked
//! that it holds `jobs register` on that session, which only that agent
//! (and the service that starts agents) does. Its `shell exec` calls may
//! then run in the background, and a person may move one that runs there by
//! its call. The service forgets the oldest registrations, so the agent
//! registers before each command. Every action is internal: the chat agent
//! offers its own tool for them, bound to its session.
//!
//! **Nothing is forgotten.** Every job's record and output file
//! is kept until the agent or a person removes the file; a job runs until
//! its command ends, it is stopped, the time limit its command named
//! (`timeout_ms`) passes, or its host ends; there is no other limit on its
//! time. At most [`MAX_RUNNING_PER_PROJECT`] run in one project at once,
//! its sessions together: one more is refused, saying which sessions run
//! them. Stopping a session's agent stops none of its jobs.
//!
//! **`list` is paged.** A page holds the running jobs first, then the ended
//! ones, each newest first, at most `limit` ([`DEFAULT_LIST`] when absent, at
//! most [`MAX_LIST`]), and `next`, the `after` of the following page, absent
//! at the end. The cursor is opaque. A job that ends between two pages may be
//! listed twice, never missed. `scope` `session` lists the named session's
//! jobs only; the default, `project`, every job of the project.

use std::collections::BTreeMap;

use nucleus_protocol::{ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "jobs";
pub const VERSION: u32 = 2;
pub const ACTION_REGISTER: &str = "register";
pub const ACTION_LIST: &str = "list";
pub const ACTION_OUTPUT: &str = "output";
pub const ACTION_STOP: &str = "stop";
pub const ACTION_BACKGROUND: &str = "background";

/// Running jobs one project may have, its sessions together.
pub const MAX_RUNNING_PER_PROJECT: usize = 16;
/// Jobs a `list` page holds when its request names no `limit`, and the most
/// it may name.
pub const DEFAULT_LIST: usize = 50;
pub const MAX_LIST: usize = 200;
/// Output kept on disk for one job; past it the file says so and ends.
pub const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Where a job's output goes, beneath the shell's root.
pub const OUTPUT_DIR: &str = ".nucleus/jobs";
/// The last lines of a job's output that a listing and a wake-up carry.
pub const TAIL_BYTES: usize = 2048;
/// What `output` returns without `after`: the end of the output.
pub const OUTPUT_TAIL_BYTES: u64 = 16 * 1024;
/// What `output` returns from `after`, at most.
pub const OUTPUT_PAGE_BYTES: u64 = 64 * 1024;
/// Longest call label.
pub const MAX_CALL_BYTES: usize = 128;
/// A job's command as listed: its first line, at most this many characters.
pub const MAX_COMMAND_CHARS: usize = 200;

/// How a command became a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    /// Started in the background by the agent.
    Agent,
    /// Moved to the background by a person.
    Person,
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    /// Ended by itself.
    Exited,
    /// Stopped by the agent or a person: its whole tree was killed.
    Stopped,
    /// Reached the time limit its command named.
    TimedOut,
    /// The VM's Nucleus restarted, or its host ended, while it ran.
    Interrupted,
}

/// One job of a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job: String,
    /// The session whose agent's command it is, which its end wakes.
    pub session: String,
    /// The command's first line, at most [`MAX_COMMAND_CHARS`].
    pub command: String,
    pub state: State,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// For `exited`: absent when a signal ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// For `exited`: processes its command left running (in the background,
    /// under `setsid`, `nohup` or `timeout`), stopped when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_leftovers: Option<u64>,
    pub by: By,
    /// The account of the person who moved it, when the window was labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<By>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_author: Option<String>,
    /// Its whole output, as the session's shell names the file.
    pub output: String,
    /// Bytes of output so far.
    pub size: u64,
    /// The last lines of its output, at most [`TAIL_BYTES`].
    pub tail: String,
    /// How long it may run from its start: the time limit its command
    /// named. Absent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_ms: Option<u64>,
    /// Why an `interrupted` one was. English text for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Which jobs a `list` names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Every job of the session's project.
    #[default]
    Project,
    /// The named session's jobs only.
    Session,
}

/// Payload of `list` (module doc, "`list` is paged").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// The `next` of the previous page.
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

/// Result of `list`: a page of jobs, running before ended, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListResponse {
    pub jobs: Vec<Job>,
    /// How many jobs of the scope run now, on this page or not.
    pub running: usize,
    /// The `after` of the next page; absent at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Result of `output`: the job, and a piece of its output
/// ([`OutputChunk`](crate::OutputChunk)'s fields). `removed`: its output
/// file is no longer there, which is said, not refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputResponse {
    pub job: Job,
    pub text: String,
    pub start: u64,
    pub next: u64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// Payload of `stop`: the session's agent stops a job of its own session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRequest {
    pub job: String,
}

/// Payload of `background`: the call of the session's agent's running
/// command. `author`, the person's account, is set by the UI service from
/// the window's label, never by the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundRequest {
    pub call: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// Result of `stop` and `background`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResponse {
    pub job: Job,
}

/// A job id: `j` and 1 to 15 of `[0-9a-z]`. It names a file, so nothing
/// else ever passes.
pub fn valid_id(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 16
        && id.starts_with('j')
        && id[1..].bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// A call label: 1 to [`MAX_CALL_BYTES`] of `[A-Za-z0-9_.:-]`.
pub fn valid_call(call: &str) -> bool {
    !call.is_empty()
        && call.len() <= MAX_CALL_BYTES
        && call.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// A command as a job lists it: its first non-blank line, at most
/// [`MAX_COMMAND_CHARS`] characters.
pub fn command_line(command: &str) -> String {
    let line = command.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    line.chars().take(MAX_COMMAND_CHARS).collect()
}

/// The interface declaration: internal actions only.
pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_REGISTER)?, ActionDecl::internal(EffectClass::ReversibleMutation)),
            (ActionName::new(ACTION_LIST)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_OUTPUT)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_STOP)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_BACKGROUND)?, ActionDecl::internal(EffectClass::ReversibleMutation)),
        ]),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A job id names a file beneath the jobs directory, so only the
    /// grammar passes: no separator, dot or upper case.
    #[test]
    fn only_plain_job_ids_and_call_labels_pass() {
        for good in ["j1", "jmgx3k0wp", "j0123456789abcde"] {
            assert!(valid_id(good), "{good}");
        }
        for bad in ["", "j", "x12", "j/..", "j.log", "jA", "j0123456789abcdef", "j1\n"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert!(valid_call("call_0a1-b.c:d") && valid_call("toolu_01AbC"));
        assert!(!valid_call("") && !valid_call("a b") && !valid_call(&"c".repeat(MAX_CALL_BYTES + 1)));
        assert_eq!(command_line("\n  gap -q x.g  \nmore"), "gap -q x.g");
    }

    /// A page's size is 1 to [`MAX_LIST`], [`DEFAULT_LIST`] when unnamed.
    #[test]
    fn a_list_page_is_bounded_and_strict() {
        assert_eq!(ListRequest::default().validate().unwrap(), DEFAULT_LIST);
        let sized = |limit| ListRequest { limit: Some(limit), ..ListRequest::default() }.validate();
        assert_eq!(sized(MAX_LIST).unwrap(), MAX_LIST);
        assert!(sized(0).is_err() && sized(MAX_LIST + 1).is_err());
        let parsed = serde_json::from_str::<ListRequest>(r#"{"scope":"session","after":"x"}"#).unwrap();
        assert_eq!(parsed.scope, Some(Scope::Session));
        assert!(serde_json::from_str::<ListRequest>(r#"{"session":"s2"}"#).is_err());
    }
}
