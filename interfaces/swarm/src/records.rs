//! Swarm records: the swarm's progress, which the swarm service writes to a
//! file of the swarm's own, never into the session's conversation, and which
//! the session's agent reads with `records`, as the UI service does for a
//! team's window. Each record's `event` says what happened; the file adds its
//! `seq` and `at_ms`. New events and optional fields are additive.
//!
//! The swarm service writes them; they are as trustworthy as it is, no more.
//! Directions, findings, reports and refusals are text written by the
//! members' models, a verdict's reason by the session agent's: untrusted for
//! whoever reads them.

use nucleus_protocol::ProcessId;
use serde::{Deserialize, Serialize};

/// Most members of one swarm.
pub const MAX_MEMBERS: usize = 16;
/// Largest task a swarm is started with, in bytes.
pub const MAX_TASK_BYTES: usize = 16 * 1024;
/// Largest direction (a short paragraph), in bytes.
pub const MAX_DIRECTION_BYTES: usize = 600;
/// Largest reason a direction was abandoned or a registration refused, in bytes.
pub const MAX_REASON_BYTES: usize = 2000;
/// Largest finding, in bytes.
pub const MAX_FINDING_BYTES: usize = 1500;
/// Largest report a member submits to the agent that started its swarm, in
/// bytes: a finding's bound. A longer one is refused, never cut; a long
/// derivation goes into the member's files, named by its paths.
pub const MAX_REPORT_BYTES: usize = MAX_FINDING_BYTES;
/// Largest verdict reason, in bytes: the critique a rejected member gets.
pub const MAX_VERDICT_BYTES: usize = 4000;
/// Most file paths one finding or solution points at, and the longest one.
pub const MAX_PATHS: usize = 4;
pub const MAX_PATH_BYTES: usize = 200;
/// Longest swarm, member, direction or finding id, in bytes.
pub const MAX_ID_BYTES: usize = 64;

/// One thing that happened to a swarm, a line of its records file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum SwarmRecord {
    /// The swarm began: whose session started it, its task, the directory
    /// its members work in (relative to the project's repository), its
    /// members, the model they run and its limits.
    Started {
        swarm: String,
        project: String,
        session: String,
        task: String,
        dir: String,
        members: Vec<String>,
        model: String,
        /// The reasoning effort its members run at. Absent: `default`, the
        /// provider's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<nucleus_model_interface::ReasoningEffort>,
        /// Model calls each run of a member may make.
        member_steps: u32,
        /// Model calls the whole swarm may reserve, duplicate checks included.
        budget: u64,
        /// UNIX milliseconds after which no member runs.
        deadline_at_ms: u64,
    },
    /// A member's process started, ended, or was stopped. `process` is the
    /// kernel's id of the run that started, so what a status of the VM says
    /// about a process can be told to be this member's.
    Member {
        swarm: String,
        member: String,
        state: MemberState,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<ProcessId>,
    },
    /// A direction was registered or changed state.
    Direction {
        swarm: String,
        member: String,
        direction: String,
        state: DirectionState,
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },
    /// A registration the swarm refused, with the direction it duplicates
    /// when that was the reason.
    Refused {
        swarm: String,
        member: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duplicate_of: Option<String>,
        reason: String,
    },
    /// Something a member posted under its direction.
    Finding {
        swarm: String,
        member: String,
        direction: String,
        finding: String,
        label: FindingKind,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// A member submitted its report of what it believes solves the task,
    /// which reached the session's agent as a reminder.
    Submission {
        swarm: String,
        member: String,
        direction: String,
        /// At most [`MAX_REPORT_BYTES`], whole.
        report: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// The session's agent judged a submission.
    Verdict { swarm: String, member: String, direction: String, accepted: bool, reason: String },
    /// The model calls the swarm has made so far: its members' runs as each
    /// member's own loop counts them, and its duplicate checks. Posted at most
    /// once a minute, while the number changes.
    Usage { swarm: String, calls: u64 },
    /// The swarm ended; `calls` is the model calls it made, `reserved_calls`
    /// the most it could have spent, `checks` its duplicate checks.
    Ended {
        swarm: String,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        calls: u64,
        reserved_calls: u64,
        checks: u64,
    },
    /// The session's agent resumed the swarm after it was interrupted: it
    /// runs again until `deadline_at_ms`, with `calls` model calls made
    /// before. `members` are the ones it was to start again on their
    /// transcripts; whether each started, the member records that follow say.
    Resumed { swarm: String, deadline_at_ms: u64, calls: u64, members: Vec<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    /// Its process runs.
    Running,
    /// It submitted a solution; its process ended and it waits for the verdict.
    Submitted,
    /// Its process ended without a solution waiting (its step or time budget,
    /// or its model ended the run).
    Ended,
    /// The swarm stopped it: its own solution was accepted, a verdict
    /// finished the swarm, the swarm was stopped, or its time ran out.
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionState {
    Active,
    /// Its member gave it up (with the reason), or ended while holding it.
    Abandoned,
    /// Its solution was rejected twice.
    Dead,
    /// Its solution was accepted.
    Succeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Lemma,
    Partial,
    Counterexample,
    DeadEnd,
    File,
    Note,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// At least one submitted solution was accepted, however the swarm ended.
    Solved,
    Stopped,
    /// Every member ended and nothing waits for a verdict.
    Exhausted,
    /// Its time ran out.
    Deadline,
    /// The swarm service ended first (a restart of the VM's Nucleus); the
    /// next one records this. The session's agent may resume it, which a
    /// [`SwarmRecord::Resumed`] record says.
    Interrupted,
}

/// Whether `id` is a valid swarm, member, direction or finding id: 1 to
/// [`MAX_ID_BYTES`] of ASCII letters, digits, `-` and `_`.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `path` may be shown as a file a finding or solution points at: a
/// relative `/`-separated path of at most [`MAX_PATH_BYTES`], with no empty,
/// `.` or `..` segment and no control character.
#[must_use]
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.contains(['\\', '\0'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn finding_with(paths: Vec<String>) -> SwarmRecord {
        SwarmRecord::Finding {
            swarm: "s1-1759312345".into(),
            member: "m2".into(),
            direction: "d1".into(),
            finding: "f3".into(),
            label: FindingKind::DeadEnd,
            text: "n = 4 is a counterexample to the first lemma".into(),
            paths,
        }
    }

    /// A record says what happened in its `event`, and nothing else passes.
    #[test]
    fn swarm_records_are_strict() {
        let record = finding_with(vec![".ultramath/s1-1759312345/m2/check.py".into()]);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            (&value["event"], &value["label"]),
            (&serde_json::json!("finding"), &serde_json::json!("dead_end"))
        );
        assert_eq!(serde_json::from_value::<SwarmRecord>(value.clone()).unwrap(), record);
        let mut forged = value;
        forged["event"] = serde_json::json!("approved");
        assert!(serde_json::from_value::<SwarmRecord>(forged).is_err());
    }
}
