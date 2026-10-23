//! The `shell` service interface, version 4: run a command in a workspace.
//!
//! | Action | Payload | Result | Effect |
//! | --- | --- | --- | --- |
//! | `exec` | [`ExecRequest`] | [`ExecResponse`] | irreversible (an arbitrary command) |
//! | `detach`, `stop` | [`JobRef`] | `{}`, [`JobStatus`] | the projects service's, on a host |
//! | `wait` | [`WaitRequest`] | [`JobStatus`] | the projects service's, on a host |
//! | `output` | [`OutputRequest`] | [`OutputChunk`] | the projects service's, on a host |
//!
//! `exec` is the one tool: reading and writing files, copying a workspace,
//! running Lean are all commands (`cat`, `cp`, `lean`). Script or
//! file content can be supplied separately through `stdin`, without a heredoc.
//!
//! **The workspace is the call's resource.** A workspace is a directory
//! beneath the provider's root, named by a resource path (`[run, problem,
//! task]`); a capability scoped to that path reaches that workspace and what
//! is beneath it, and nothing beside it. The kernel checks the resource
//! against the caller's capability before the provider is reached; the
//! provider then runs the command with that directory as its working
//! directory. The workspace is where a command starts, not a bound on what
//! it reaches: a command has the permissions of the Linux user the provider
//! runs as, plus the limits the provider sets; see the provider's
//! documentation for what that is and is not.
//!
//! `exec` runs `bash -e -o pipefail -c <command>` with an environment the operator
//! configured (nothing inherited), a wall-clock limit and a bound on the
//! output returned.
//! Everything a command prints is untrusted input; it carries no authority.
//! This crate defines data only.
//!
//! **A command can become a background job**: in the VM a
//! session's agent may start one so (`background`), and a person may move
//! a running one there (the [`jobs`] endpoint, which the projects service
//! serves). The call then answers at once with [`ExecResponse::job`] and
//! the command keeps running in its host, which the projects service drives
//! with the host-only actions `detach`, `wait`, `stop` and `output`; they
//! are not tools and no agent holds them. A provider not started with jobs
//! (`--jobs on`) refuses `background` and keeps none.

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, ResourceFrom,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub mod jobs;

pub const ENDPOINT: &str = "shell";
pub const ACTION_EXEC: &str = "exec";
/// Makes the running command named by a job id a background job.
pub const ACTION_DETACH: &str = "detach";
/// Answers when a job ends, or after a while with how it runs.
pub const ACTION_WAIT: &str = "wait";
/// Ends a job: its command and everything it started.
pub const ACTION_STOP: &str = "stop";
/// A piece of a job's output.
pub const ACTION_OUTPUT: &str = "output";
pub const VERSION: u32 = 4;

/// What a provider's description says of its commands' network, when
/// whoever started it said which ([`network_of`] reads it back): a service
/// that hands the shell to others can then tell them the same thing. What is
/// cut is the IP network (a network namespace of its own): it is all a
/// command has to download, install or look anything up with. Socket files
/// of the machine and `AF_VSOCK` are outside a network namespace.
pub const NETWORK_ON: &str = "Commands have network access.";
pub const NETWORK_OFF: &str = "Commands have no IP network: nothing can be downloaded or installed, and no \
                               host on the internet or on a local network can be reached.";

/// Whether the shell described by `description` says its commands have
/// network; `None` when it does not say.
pub fn network_of(description: &str) -> Option<bool> {
    match (description.contains(NETWORK_ON), description.contains(NETWORK_OFF)) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Longest command text `exec` accepts, in bytes.
pub const MAX_COMMAND_BYTES: usize = 256 * 1024;
/// Most UTF-8 bytes accepted as one command's standard input.
pub const MAX_STDIN_BYTES: usize = 256 * 1024;
/// Most output one `exec` may return, in bytes (the provider may cap lower).
/// Kept well under the protocol's frame limit with the rest of the reply.
pub const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// Payload of `exec`: the command, and optional tighter limits than the
/// provider's defaults (never looser).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    /// The script `bash -e -o pipefail -c` runs; at most [`MAX_COMMAND_BYTES`].
    pub command: String,
    /// Literal UTF-8 input, not shell source or expanded text. At most
    /// [`MAX_STDIN_BYTES`]. Delivered through a seekable anonymous file,
    /// with EOF after these bytes; this is not an interactive input stream.
    /// Omitted input uses the null device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Wall-clock limit. For a foreground command the provider clamps it to
    /// its maximum and to the call's deadline; a background job keeps the
    /// limit it names, or has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Cap on the output returned; the provider clamps it to its maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
    /// Start it as a background job: the call answers at once and the
    /// command keeps running, up to `timeout_ms` when given, else with no
    /// time limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// The caller's name for this command (the chat agent's tool call id),
    /// by which a person moves it to the background. A label, not authority;
    /// at most [`jobs::MAX_CALL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    /// The job this command becomes if it goes to the background, named by
    /// the projects service, which alone sets it ([`jobs::valid_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

/// Result of `exec`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResponse {
    /// The command's exit status; `None` when it was ended by a signal.
    /// Bash starts with errexit and pipefail; explicit conditionals, error
    /// handling, or option changes still follow Bash semantics. This is not
    /// a guarantee that all constituent commands succeeded, nor rollback.
    pub exit_code: Option<i32>,
    /// True when the wall-clock limit ended it and everything it started.
    pub timed_out: bool,
    /// What the agent should know about how the command ran, when anything:
    /// processes it left running were stopped (`stopped_leftovers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Standard output and standard error, interleaved as produced (both go
    /// to one pipe), cut to the output cap.
    pub output: String,
    /// True when `output` was cut; the tail is lost, the head kept.
    pub truncated: bool,
    pub elapsed_ms: u64,
    /// Processes the command started that were still running when it ended
    /// (in the background, under `setsid`, `nohup` or `timeout`, daemons),
    /// which were stopped with it; absent when there were none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_leftovers: Option<u64>,
    /// The command went on as a background job: it still runs, its output
    /// goes to a file, and this call's `exit_code` and `output` are empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<Detached>,
}

/// A command that went on as a background job, in the answer of its call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detached {
    pub id: String,
    /// The agent started it so, or a person moved it.
    pub by: jobs::By,
    /// The person's account id, when the window that moved it was labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Its whole output, as the command's own shell names the file.
    pub output: String,
    /// How long it may run, from when the command started: the time limit
    /// its command named. Absent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_ms: Option<u64>,
    /// For the model: what happened and how to follow the job.
    pub note: String,
}

/// Names a job (`detach`, `stop`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRef {
    pub job: String,
}

/// Payload of `wait`: answered when the job ends, or after `wait_ms`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    pub job: String,
    pub wait_ms: u64,
}

/// A job as its host knows it (`wait`, `stop`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatus {
    /// `running`, `exited`, `stopped` or `timed_out`; a host never says
    /// `interrupted`.
    pub state: jobs::State,
    /// For `exited`: absent when a signal ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// For `exited`: what the command left running, stopped with it, as
    /// [`ExecResponse::stopped_leftovers`] counts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_leftovers: Option<u64>,
    /// Since the command started.
    pub elapsed_ms: u64,
    /// Bytes of output so far.
    pub size: u64,
    /// The last lines of its output, at most [`jobs::TAIL_BYTES`].
    pub tail: String,
}

/// Payload of `output`: from byte `after`, or the end of the output when
/// absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRequest {
    pub job: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

/// A piece of a job's output: bytes `start..next` of its file, as UTF-8
/// (invalid bytes as U+FFFD), and the file's size now. A piece that more
/// follows ends between characters, so pieces read one after another join
/// into the text. `removed`: the job's output file is not there (the agent
/// or a person removed it, or it could not be made), and the piece is empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputChunk {
    pub text: String,
    pub start: u64,
    pub next: u64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// The declaration, with the provider's own account of the environment its
/// commands run in (`description`, [`InterfaceDecl::description`]): the
/// working directory, what is on PATH, the limits, what is absent. A model
/// given the `exec` tool reads it before its first command. With `jobs`, the
/// tool offers `background` (in the VM, the projects service's `shell`).
pub fn interface(endpoint: EndpointName, description: String, jobs: bool) -> Result<InterfaceDecl, Error> {
    let mut params = json!({"type": "object", "properties": {
        "command": {"type": "string", "description": "The command line bash runs"},
        "stdin": {"type": "string", "description": "Optional literal standard input, at most 262144 UTF-8 bytes. No shell expansion. Use python3 - to run source, or cat > a-file to write text; > replaces that file."},
        "timeout_ms": {"type": "integer", "minimum": 1, "description": "Optional wall-clock limit in milliseconds. In the foreground a command without it stops at the default the environment names, and a longer one is cut to that environment's maximum and to this call's deadline."}
    }, "required": ["command"]});
    if jobs && let Some(properties) = params.get_mut("properties").and_then(serde_json::Value::as_object_mut)
    {
        properties.insert(
            "background".into(),
            json!({"type": "boolean", "description": "Run it as a background job: the call answers at once \
                with the job's id and the command keeps running, up to timeout_ms when you give one, else \
                with no time limit. You are woken with its exit status, its last lines of output and the path \
                of its full output when it ends; shell_jobs reads its output or stops it. Use it for a \
                computation that may take more than a few minutes, and tell the person you did."}),
        );
    }
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description,
        actions: BTreeMap::from([(
            ActionName::new(ACTION_EXEC)?,
            ActionDecl::tool(
                EffectClass::Irreversible,
                "Run bash with errexit and pipefail in the workspace. An unhandled command or pipeline \
                 failure stops the script; explicit error handling follows Bash semantics. Pass script \
                 or file text in stdin instead of embedding a heredoc: for example command=python3 - \
                 with Python source in stdin. Return output and exit status; prior side effects remain.",
                params,
                ResourceFrom::Handle,
            ),
        )]),
    })
}

/// The declaration of a shell host the projects service runs jobs in: the
/// public one, and the job actions only that service holds, which are no
/// tools (module documentation).
pub fn host_interface(endpoint: EndpointName, description: String) -> Result<InterfaceDecl, Error> {
    let mut declared = interface(endpoint, description, true)?;
    declared.actions.extend([
        (ActionName::new(ACTION_DETACH)?, ActionDecl::internal(EffectClass::ReversibleMutation)),
        (ActionName::new(ACTION_WAIT)?, ActionDecl::internal(EffectClass::Observation)),
        (ActionName::new(ACTION_STOP)?, ActionDecl::internal(EffectClass::Irreversible)),
        (ActionName::new(ACTION_OUTPUT)?, ActionDecl::internal(EffectClass::Observation)),
    ]);
    Ok(declared)
}

/// Splits a workspace written with `/` into resource segments, skipping
/// empty ones and refusing `.`, `..`, anything outside `[A-Za-z0-9._-]` and a
/// workspace with no segment, so a workspace name can never leave the
/// provider's root.
pub fn workspace_segments(text: &str) -> Result<Vec<String>, Error> {
    let invalid = |why: &str| Error::new(ErrorCode::InvalidInput, format!("workspace: {why}"));
    let segments: Vec<String> = text.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
    if segments.is_empty() {
        return Err(invalid("must name at least one segment"));
    }
    for segment in &segments {
        if segment == "." || segment == ".." {
            return Err(invalid("`.` and `..` are not allowed"));
        }
        if !segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(invalid("segments may contain only letters, digits, `.`, `_` and `-`"));
        }
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// The network a shell declares is read back from its description, and
    /// not from one that does not say.
    #[test]
    fn a_description_s_network_sentence_is_read_back() {
        let declared = |sentence: &str| format!("Commands run with bash. {sentence} Killed after 25 s.");
        assert_eq!(network_of(&declared(NETWORK_ON)), Some(true));
        assert_eq!(network_of(&declared(NETWORK_OFF)), Some(false));
        assert_eq!(network_of(&declared("")), None);
    }

    #[test]
    fn workspace_names_are_plain_segments() {
        assert_eq!(workspace_segments("run1/putnam_1962_a1/t000007").unwrap().len(), 3);
        assert_eq!(workspace_segments("/a//b/").unwrap(), vec!["a", "b"]);
        for bad in ["", "/", "a/../b", "./a", "a b", "a/\u{e9}", "a/b\0"] {
            assert_eq!(workspace_segments(bad).map_err(|e| e.code), Err(ErrorCode::InvalidInput), "{bad:?}");
        }
    }
}
