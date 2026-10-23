//! The worker's workspace as the harness touches it: its own reads and
//! writes through the worker's `shell exec` (the agent holds no filesystem
//! capability of its own), and what it fills into the model's tool calls —
//! the file behind a `submit` or an argument-less `verify`, the
//! address of a `lean` call, a command's limit — before they are dispatched.

use nucleus_explore_interface as explore;
use nucleus_lean_interface as lean;
use nucleus_model_interface::{Message, Role, ToolCall};
use nucleus_protocol::{Error, ErrorCode, ResourceFrom, ToolInfo};
use nucleus_sdk::tool_name;
use nucleus_shell_interface as shell;
use serde_json::{Map, Value, json};

use super::{Agent, BRANCH_INDEX_FILE, MAX_FILE_BYTES, STEP_DETAIL_CHARS, Step};
use crate::excerpt;

/// A call to one of this worker's `lean` tools is about this worker's own
/// statement, which the `lean` service takes from the call's resource and
/// from nowhere else. A `frozen` the model writes into its arguments is
/// dropped: the service refuses the field anyway, and this worker never
/// passes one on.
///
/// When the tool's capability names `problem` (`[key, problem, task]`,
/// `key` the run's), a resource argument that
/// is missing, blank or names `problem` in any segment becomes that
/// capability's own path, so the model never needs to know the run it works
/// for. Any other value is left as the model wrote it, and the kernel
/// refuses what this worker does not hold. This is addressing, not
/// authority: the kernel still checks the call against the capability.
fn about_own_statement(tool: &ToolInfo, problem: &str, arguments: &mut Map<String, Value>) {
    arguments.remove("frozen");
    let ResourceFrom::Segments { param } = &tool.resource_from else { return };
    let own = tool.resource.segments();
    if !own.iter().any(|segment| segment == problem) {
        return;
    }
    let named = arguments.get(param).and_then(Value::as_str).unwrap_or_default();
    if named.trim().is_empty() || named.split('/').any(|segment| segment == problem) {
        arguments.insert(param.clone(), json!(own.join("/")));
    }
}

impl Agent<'_> {
    /// Writes [`BRANCH_INDEX_FILE`] from the assignment's branch
    /// references, replacing whatever the parent's workspace clone left at
    /// that name, also when there are none. Pointers and the harness's own
    /// evaluation only; what the model finds behind them is untrusted. A
    /// write that fails is a step the model is told about, not the end of
    /// the worker: a parent must not be able to stop its continuations
    /// before their first model call.
    pub(crate) fn write_branch_index(
        &mut self,
        references: &[explore::BranchReference],
    ) -> Result<(), String> {
        let content = serde_json::to_string_pretty(&json!({
            "v": 1,
            "note": "Completed branches of this run and problem. The check and `verified` describe the \
                     file each branch submitted, not what is in its directory now, which may differ or be \
                     gone; neither certifies a lemma, note or counterexample there. Recheck what you reuse. \
                     You may read these paths; you write only in your own working directory.",
            "branches": references,
        }))
        .map_err(|e| e.to_string())?;
        self.steps += 1;
        let mut step = Step::new(self.steps, "branch_index");
        match self.write_named(BRANCH_INDEX_FILE, &content) {
            Ok(()) => step.detail = Some(format!("{} reference(s)", references.len())),
            Err(why) => {
                // The shell's error can quote names the parent's model chose,
                // so it stays in the record and out of the model's context.
                step.detail = Some(excerpt(&why, STEP_DETAIL_CHARS));
                self.push(Message::text(
                    Role::User,
                    format!(
                        "Harness notice: `{BRANCH_INDEX_FILE}` could not be written in your working \
                         directory, so whatever is at that name is not the harness's index of other \
                         branches."
                    ),
                ))?;
            }
        }
        self.record(&step)
    }

    /// One command of the loop's own (not the model's), with the whole
    /// output cap; a failure to run it is this worker's failure.
    fn own_exec(&mut self, command: &str) -> Result<shell::ExecResponse, String> {
        let request = shell::ExecRequest {
            command: command.to_owned(),
            stdin: None,
            timeout_ms: Some(self.options.verify_deadline_ms),
            max_output_bytes: Some(MAX_FILE_BYTES),
            ..Default::default()
        };
        let deadline = self.options.verify_deadline_ms.saturating_add(30_000);
        self.shell_call::<shell::ExecResponse>(shell::ACTION_EXEC, &request, deadline)
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("cannot run a command in the workspace ({:?}): {}", e.code, e.message))
    }

    /// The workspace file, whole (`cat`); `None` when it does not exist.
    pub(super) fn read_file(&mut self, name: &str) -> Result<Option<String>, String> {
        let quoted = shell_quote(name);
        let done = self.own_exec(&format!("if [ -e {quoted} ]; then cat {quoted}; else exit 44; fi"))?;
        match done.exit_code {
            Some(44) => Ok(None),
            Some(0) if !done.truncated => Ok(Some(done.output)),
            Some(0) => Err(format!("{name} is larger than {MAX_FILE_BYTES} bytes")),
            other => Err(format!("cannot read {name}: exit {other:?}: {}", excerpt(&done.output, 300))),
        }
    }

    /// Writes the workspace file through a `base64` pipe, so no byte of the
    /// content meets the shell.
    pub(super) fn write_file(&mut self, content: &str) -> Result<(), String> {
        let name = self.file_name.clone();
        self.write_named(&name, content)
    }

    /// Writes one file of the workspace by name, through the worker's own
    /// `shell exec`: the agent holds no filesystem capability of its own.
    /// Whatever is at that name is removed first and never followed: a
    /// workspace cloned from a parent's may hold a directory, a read-only
    /// file or a link to the proof file there.
    fn write_named(&mut self, name: &str, content: &str) -> Result<(), String> {
        let encoded = base64_encode(content.as_bytes());
        let quoted = shell_quote(name);
        let done =
            self.own_exec(&format!("rm -rf -- {quoted} && printf '%s' '{encoded}' | base64 -d > {quoted}"))?;
        if done.exit_code == Some(0) {
            Ok(())
        } else {
            Err(format!("cannot write {name}: exit {:?}: {}", done.exit_code, excerpt(&done.output, 300)))
        }
    }

    /// This worker's own tool on the `lean` endpoint named `name`.
    fn lean_tool(&self, name: &str) -> Option<&ToolInfo> {
        self.tools
            .iter()
            .find(|t| t.endpoint.as_str() == self.lean_endpoint && tool_name(&t.endpoint, &t.action) == name)
    }

    /// A `shell exec` call that names no `timeout_ms` gets `--bash-timeout-ms`;
    /// one that names its own keeps it. The shell clamps either to its maximum.
    pub(super) fn with_exec_timeout(&self, mut call: ToolCall) -> ToolCall {
        if self.is_exec_tool(&call.name)
            && let Value::Object(map) = &mut call.arguments
            && map.get("timeout_ms").is_none_or(Value::is_null)
        {
            map.insert("timeout_ms".to_owned(), json!(self.options.bash_timeout_ms));
        }
        call
    }

    /// The notice for a workspace file that does not hold the frozen theorem
    /// statement verbatim, or `None` when it does (or when the frozen file has
    /// no statement to look for, or the file cannot be read: this is advice,
    /// and a failed read is not a finding).
    pub(super) fn statement_notice(&mut self) -> Option<String> {
        let statement = lean::extract_statement(&self.frozen)?.to_owned();
        let name = self.file_name.clone();
        let file = self.read_file(&name).ok()?;
        lost_statement(&name, &statement, file.as_deref())
    }

    /// A `submit` call names a file; this reads it out of the worker's own
    /// workspace (the model's whole answer for the cost of one line, however
    /// long the proof) and puts the worker's accounting in — the model calls
    /// it made and the tokens it spent. The harness
    /// refunds the run's budget from those counters, so they are measured
    /// here and never taken from the model's arguments. The name is read
    /// through the worker's own `shell exec`, so the shell service — not this
    /// code — is what keeps it inside the workspace.
    pub(super) fn with_submission(&mut self, call: &ToolCall) -> Result<Result<ToolCall, Error>, String> {
        let named = call.arguments.get("name").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
        let name = if named.is_empty() { self.file_name.clone() } else { named };
        let file = match self.read_file(&name) {
            Ok(Some(text)) => text,
            Ok(None) => {
                return Ok(Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "there is no `{name}` in your workspace to hand over; write the file first, \
                         then submit its name"
                    ),
                )));
            }
            Err(why) => return Ok(Err(Error::new(ErrorCode::InvalidInput, why))),
        };
        if file.trim().is_empty() {
            return Ok(Err(Error::new(
                ErrorCode::InvalidInput,
                format!("`{name}` is empty; there is nothing to grade in it"),
            )));
        }
        if let Some(statement) = lean::extract_statement(&self.frozen)
            && let Some(notice) = lost_statement(&name, statement, Some(&file))
        {
            return Ok(Err(Error::new(ErrorCode::InvalidInput, notice)));
        }
        let submission = explore::Submission {
            file,
            model_calls: self.calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_nano_usd: self.cost_nano_usd,
            // Only terminal collection permits partial artifacts. This flag
            // is assigned here, never accepted from model arguments.
            final_offer: self.collecting,
        };
        let arguments = serde_json::to_value(&submission).map_err(|e| e.to_string())?;
        Ok(Ok(ToolCall { arguments, ..call.clone() }))
    }

    /// A call to one of this worker's `lean` tools names this worker's own
    /// statement and carries no `frozen` ([`about_own_statement`]), and a
    /// `verify` whose `candidate` is missing or blank gets the
    /// workspace file put in, so that the model can ask about the artifact
    /// without sending it. Nothing else is added. Any other call is passed
    /// through untouched.
    pub(super) fn with_lean_arguments(&mut self, call: &ToolCall) -> Result<Result<ToolCall, Error>, String> {
        let Some(tool) = self.lean_tool(&call.name).cloned() else {
            return Ok(Ok(call.clone()));
        };
        let mut call = call.clone();
        if call.arguments.is_null() {
            call.arguments = json!({});
        }
        let Value::Object(map) = &mut call.arguments else {
            return Ok(Err(Error::new(ErrorCode::InvalidInput, "arguments must be an object".to_owned())));
        };
        about_own_statement(&tool, &self.problem, map);
        let takes_file = tool.action.as_str() == lean::ACTION_VERIFY;
        if !takes_file || !map.get("candidate").and_then(Value::as_str).unwrap_or_default().trim().is_empty()
        {
            return Ok(Ok(call));
        }
        let name = self.file_name.clone();
        let Some(text) = self.read_file(&name)? else {
            return Ok(Err(Error::new(
                ErrorCode::InvalidInput,
                format!("no `candidate` given and no `{name}` in the workspace to read"),
            )));
        };
        if let Value::Object(map) = &mut call.arguments {
            map.insert("candidate".to_owned(), json!(text));
        }
        Ok(Ok(call))
    }
}

/// What the model is told when the workspace file `name` (`file`, `None` when
/// it is gone) does not hold the frozen `statement` byte for byte, or `None`
/// when it does. The statement is quoted, because a reflowed copy reads the
/// same in any description of the difference.
fn lost_statement(name: &str, statement: &str, file: Option<&str>) -> Option<String> {
    if file.is_some_and(|file| file.contains(statement)) {
        return None;
    }
    Some(format!(
        "Harness notice: `{name}` does not contain the frozen theorem statement verbatim, so it cannot be \
         graded or submitted as it is. The harness does not change your file. The statement, byte for \
         byte:\n\n```lean\n{statement}\n```"
    ))
}

/// A single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Standard base64 (RFC 4648, padded); the decoder is coreutils' `base64 -d`.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use nucleus_protocol::{ActionName, EffectClass, EndpointName, ResourcePath};

    #[test]
    fn the_seed_write_encodes_any_bytes_and_quotes_the_file_name() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode("théorème\n".as_bytes()), "dGjDqW9yw6htZQo=");
        assert_eq!(shell_quote("it's.lean"), "'it'\\''s.lean'");
    }

    /// A discovery worker's `lean` capability is `[key, problem, task]`, and
    /// the model names only the problem: the call goes to the worker's own
    /// capability, or, when it names something else, as written. A `frozen`
    /// the model writes, which would name the standard itself, never leaves.
    #[test]
    fn a_lean_call_names_the_workers_own_capability_and_carries_no_frozen_text() {
        let tool = |resource: &[&str], from: ResourceFrom| ToolInfo {
            endpoint: EndpointName::new("lean").unwrap(),
            action: ActionName::new(lean::ACTION_VERIFY).unwrap(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            effect: EffectClass::Observation,
            description: String::new(),
            params: json!({}),
            resource_from: from,
            endpoint_description: String::new(),
        };
        let segments = || ResourceFrom::Segments { param: "problem".into() };
        let worker = tool(&["deep", "demo_a", "t000002"], segments());
        let forged = "import Mathlib\n\ntheorem demo_a : True :=\nsorry\n";
        for (tool, given, expected) in [
            (&worker, json!({"problem": "demo_a", "candidate": "c"}), json!("deep/demo_a/t000002")),
            (&worker, json!({"candidate": "c"}), json!("deep/demo_a/t000002")),
            (&worker, json!({"problem": " ", "candidate": "c"}), json!("deep/demo_a/t000002")),
            (&worker, json!({"problem": "deep/demo_a", "candidate": "c"}), json!("deep/demo_a/t000002")),
            (
                &worker,
                json!({"problem": "demo_a", "candidate": "c", "frozen": forged}),
                json!("deep/demo_a/t000002"),
            ),
            (
                &worker,
                json!({"problem": "putnam_1962_a1", "candidate": "c", "frozen": forged}),
                json!("putnam_1962_a1"),
            ),
            (&worker, json!({"problem": "putnam_1962_a1", "candidate": "c"}), json!("putnam_1962_a1")),
            (&tool(&["demo_a"], segments()), json!({"problem": "demo_a", "candidate": "c"}), json!("demo_a")),
            (&tool(&[], segments()), json!({"problem": "demo_a", "candidate": "c"}), json!("demo_a")),
            (&tool(&["deep", "demo_a"], ResourceFrom::Handle), json!({"problem": "demo_a"}), json!("demo_a")),
            (
                &tool(&["deep", "demo_a"], ResourceFrom::Handle),
                json!({"problem": "demo_a", "frozen": forged}),
                json!("demo_a"),
            ),
        ] {
            let Value::Object(mut map) = given.clone() else { unreachable!() };
            about_own_statement(tool, "demo_a", &mut map);
            assert_eq!(map["problem"], expected, "{given}");
            assert!(!map.contains_key("frozen"), "the model's frozen text is dropped: {given}");
            assert!(map.get("candidate").is_none_or(|c| c == "c"), "only the address changes: {given}");
        }
    }
}
