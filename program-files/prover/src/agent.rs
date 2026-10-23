//! The tool loop: the model works in a workspace with the
//! tools the **kernel lists** for it — every tool-callable action of the
//! capabilities it holds (`shell exec`, `lean verify`, …),
//! rendered and routed by the SDK's generic tool layer and never named in
//! this code, so the descriptions the model reads are the providers' own and
//! a new tool needs no code here; the loop, not the model, decides when the
//! run ends.
//!
//! Lean is in the loop: the model reads its file, edits it, runs `lean` on
//! it and reads the errors, many short steps instead of a few blind
//! whole-file guesses (the loop's own reads and writes of the workspace file
//! are commands too: `cat`, and a `base64` pipe for the seed). The harness
//! owns the stopping rule: a reply without a tool call is answered with a
//! nudge, a reply cut at the output cap with "continue", and only a `lean
//! verify` reply with `accepted: true` (or a submission, the budget, or a
//! terminate) ends the run. The memory is the **standard message
//! transcript**, reset when it outgrows the context, with the work kept in
//! files. The kernel's `max_calls` and the harness's caps bound every run.
//!
//! # What the harness sets, and what it only reports
//!
//! No call carries frozen text: the `lean` service judges every `verify`
//! against the statement its own resource names (the lean
//! interface), so a call to this worker's `lean` tools names this worker's
//! own capability, a `frozen` the model wrote into it is dropped
//! (`workspace::about_own_statement`), and nothing else is added. A
//! `shell exec` without `timeout_ms` gets `--bash-timeout-ms` instead of the
//! shell's own default; the shell still clamps it to its maximum, and the
//! prompt states the limit for the command tool this worker is listed.
//! After a turn that ran a `shell exec`, a workspace file that no longer
//! contains the frozen theorem statement verbatim gets a notice quoting that
//! statement, and a submission of such a file is refused before the harness
//! would refuse it. The harness never edits or restores the file: the
//! model's work stays the model's.
//!
//! # Plans
//!
//! A worker holding both plan tools publishes and compares a short research
//! plan before it works ([`crate::planning`]): while it plans, each request
//! offers and forces the one plan tool the phase needs; a plan call ends the
//! dispatch of its reply, so the model sees the board before it acts again;
//! a call to a tool the request did not offer is refused. A worker that never
//! gets past planning did no work and yields no child (`plan_incomplete`),
//! unless a model error stopped it, which ends it as `model_error` as it
//! would anywhere else. Plans are untrusted text, delivered only as tool
//! results.
//!
//! # Collection
//!
//! A worker that holds the submit tool and spends its call budget unsolved
//! gets at most [`explore::COLLECTION_CALLS`] more calls to hand over what it
//! has. Only then may it submit a partial file (`final_offer`, which the
//! worker sets and the model cannot). The last of those calls offers the
//! submit tool alone and requires it; a model that still submits nothing has
//! its workspace file submitted for it. At the start of collection
//! `transcript.jsonl` is frozen and the collection dialogue goes to
//! `submission.jsonl`, so a continuation inherits the search's transcript,
//! never the collection.
//!
//! # The transcript is the node
//!
//! A node of the discovery tree records the standard message transcript —
//! system, user, assistant and tool messages in the provider's own format —
//! and `CONTINUE(v)` **appends** to it: a continuation worker starts from its
//! parent's transcript as recorded (a stable prefix the provider can cache
//! across siblings), adds one user message (what the harness knows: the
//! checker's verdict, what the siblings tried), and runs. Nothing is folded
//! into the first message. The transcript lives in the worker's own output
//! directory ([`crate::transcript`]), which the model cannot reach, and the
//! harness copies a parent's into the child's directory as `parent.jsonl`
//! before the child starts; the assignment names it.
//!
//! **Context reset** is the one memory mechanism: when the
//! last call's context reached `--compact-at-tokens` — the provider's own
//! count of it, `usage.input_tokens + output_tokens`, so no tokenizer and no
//! estimate is involved — the buffer becomes `[system, first user, notice]`.
//! There is no summary call and no automatic workspace read: the notice says
//! the work is in the files, and the model decides whether to read them.
//! One transcript line records the boundary with its notice.
//!
//! # Records
//!
//! `attempts.jsonl`: one [`Step`] per model call, tool call, nudge, continue
//! and reset, with tokens and timings. `result.json` is written by the
//! caller from the [`LoopEnd`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agent_prompts::AgentPrompts;
use crate::planning::{Phase, Planning};
use crate::transcript::{
    Header, Line, TRANSCRIPT_FILE, TRANSCRIPT_VERSION, Transcript, kept_prefix, load_transcript,
};
use nucleus_explore_interface as explore;
use nucleus_lean_interface as lean;
use nucleus_model_interface::{FinishReason, Message, Role, ToolCall, ToolChoice, ToolSpec};
use nucleus_protocol::{Error, ErrorCode, HandleId, ResourcePath, ToolInfo};
use nucleus_sdk::{ChannelError, resolve_tool_call, tool_name};
use nucleus_shell_interface as shell;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Kernel, ModelAccess, Options, complete, excerpt};

mod workspace;

/// Written while a model call is in flight and removed when it lands: what
/// the provider has produced so far, so that whether a long call is still
/// producing is answered by the records.
pub const INFLIGHT_FILE: &str = "inflight.json";
/// The branch index a continuation finds in its workspace: the assignment's
/// `branch_references`, written by the harness through the worker's shell.
pub const BRANCH_INDEX_FILE: &str = "BRANCHES.json";
/// `--compact-at-tokens` unless given.
pub const DEFAULT_COMPACT_AT_TOKENS: u64 = 800_000;
/// `--bash-timeout-ms` unless given.
pub const DEFAULT_BASH_TIMEOUT_MS: u64 = 900_000;
/// Longest string field of a tool's reply handed to the model (a command's
/// output, Lean's diagnostics); the service keeps the head, Lean puts its
/// first error first.
const TOOL_RESULT_CHARS: usize = 32 * 1024;
/// Longest workspace file read back for the verifier or the submission: the
/// service's output cap for one command.
const MAX_FILE_BYTES: u64 = shell::MAX_OUTPUT_BYTES;
/// Longest tool-result excerpt kept in a step record.
const STEP_DETAIL_CHARS: usize = 400;
/// Fewest messages a reset can act on: system, task, and something after.
const MIN_MESSAGES_TO_COMPACT: usize = 4;

/// Calls before the unchanged-file reminder, and how often it is sent.
const SEED_HINT_AFTER: u32 = 25;
const SEED_HINTS_MAX: u32 = 3;

/// The collection phase has its own transcript and never enters a child node's context.
const COLLECTION_TRANSCRIPT_FILE: &str = "submission.jsonl";

/// Turns of exactly one tool call before the batch reminder rides along.
const BATCH_HINT_AFTER: u32 = 8;
/// How many times one worker is told; after that it has heard it.
const BATCH_HINTS_MAX: u32 = 3;
// Not so eager that a short chain of dependent calls trips it, and said a
// few times at most: after that the worker has heard it.
const _: () = assert!(BATCH_HINT_AFTER >= 4 && BATCH_HINTS_MAX >= 1 && BATCH_HINTS_MAX <= 5);

/// One line of `attempts.jsonl` in the tool loop.
#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub step: u32,
    /// `model`, `model_retry`, `tool`, `nudge`, `plan_nudge`, `continue`,
    /// `context_reset`, `collection_started`, `branch_index`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Streamed calls only: when the provider's first byte of any kind
    /// arrived, and how many bytes it produced in all — thinking included,
    /// which is most of them on a reasoning model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_byte_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub produced: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// What the provider charged for this one call, in nano-USD, when it says
    /// (model interface, `Usage::cost_nano_usd`). Omitted when it reports none.
    #[serde(skip_serializing_if = "is_zero")]
    pub cost_nano_usd: u64,
    pub elapsed_ms: u64,
    /// A bounded excerpt of the tool's output or the model's reply (untrusted text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Empties `calls` when the reply that carried them was cut at the output
/// cap, and says how many went. Returns 0 otherwise.
///
/// A cut reply is incomplete, and so is whatever tool call the model was in
/// the middle of writing: its arguments are a truncated JSON fragment, which
/// the adapter hands back as a bare string rather than an object. Acting on
/// that runs a command the model never finished.
///
/// All of them go, not only the last: only the final call can be truncated,
/// but telling which one that is from the outside is guesswork, and a call
/// the model still wants it writes again in the continuation.
fn drop_calls_of_a_cut_reply(finish: FinishReason, calls: &mut Vec<ToolCall>) -> usize {
    if finish == FinishReason::Length { std::mem::take(calls).len() } else { 0 }
}

/// `skip_serializing_if` for a counter whose absence and whose zero mean the
/// same thing.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Step {
    fn new(step: u32, kind: &'static str) -> Self {
        Step {
            step,
            kind,
            cost_nano_usd: 0,
            tool: None,
            finish_reason: None,
            first_byte_ms: None,
            produced: None,
            exit_code: None,
            timed_out: None,
            accepted: None,
            rejection: None,
            input_tokens: 0,
            output_tokens: 0,
            elapsed_ms: 0,
            detail: None,
        }
    }
}

/// Where the loop stopped and what it produced.
pub struct LoopEnd {
    /// `complete`, `submitted`, `max_attempts`, `model_budget`,
    /// `model_error`, `model_outcome_unknown`, `plan_incomplete`,
    /// `terminated`.
    pub stopped: &'static str,
    pub error: Option<String>,
    /// Model calls made; a context reset is not one.
    pub calls: u32,
    pub steps: u32,
    pub nudges: u32,
    /// Context resets (the field keeps the record's name).
    pub compactions: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_nano_usd: u64,
    /// True iff `lean verify` reported `accepted: true`.
    pub complete: bool,
    /// The workspace file when the loop ended, when it exists; read only when
    /// the model did not hand its artifact over with `submit`.
    pub file: Option<String>,
    /// What the model submitted, when it did. `None` means it never handed
    /// anything over and the caller must decide what to send in its place.
    pub submitted: Option<String>,
}

/// The tools as the model sees them: one per tool-callable action of the
/// capabilities this worker holds, exactly as the kernel listed them
/// (`endpoint_action`, the provider's description and parameter schema).
pub fn tool_specs(tools: &[ToolInfo]) -> Vec<ToolSpec> {
    tools
        .iter()
        .map(|t| ToolSpec {
            name: tool_name(&t.endpoint, &t.action),
            description: t.description.clone(),
            parameters: t.params.clone(),
        })
        .collect()
}

/// Whether the listing holds the tool whose acceptance ends the run.
pub fn has_verify(tools: &[ToolInfo], lean_endpoint: &str) -> bool {
    tools.iter().any(|t| t.endpoint.as_str() == lean_endpoint && t.action.as_str() == lean::ACTION_VERIFY)
}

/// One worker's loop: its handles, its buffer, its records.
pub struct Agent<'a> {
    prompts: AgentPrompts,
    pub kernel: &'a mut Kernel,
    pub options: &'a Options,
    pub model: ModelAccess,
    pub model_resource: ResourcePath,
    pub shell: HandleId,
    /// The `shell` resource: this worker's workspace.
    pub workspace: ResourcePath,
    /// The kernel's listing of this worker's tool-callable actions.
    tools: Vec<ToolInfo>,
    specs: Vec<ToolSpec>,
    /// The endpoint whose `verify` acceptance ends the run.
    lean_endpoint: String,
    /// The problem id: the theorem's name and the workspace file's stem.
    problem: String,
    /// `<problem>.lean`, the file the model works on.
    pub file_name: String,
    pub transcript: Transcript,
    out: PathBuf,
    steps_log: std::fs::File,
    messages: Vec<Message>,
    /// The size of the buffer as the provider last measured it.
    context_tokens: u64,
    /// The workspace file as this worker found it: the parent's file, or the
    /// frozen statement. A file still identical to it at the end means the
    /// model worked entirely through the checker.
    seed: String,
    /// The frozen file this worker was handed, read only by the statement
    /// guard. It is never sent anywhere: the `lean` service judges every call
    /// against the statement its own resource names.
    frozen: String,
    steps: u32,
    calls: u32,
    /// Consecutive turns that carried exactly one tool call, and how often
    /// this worker has been told it can batch (the template's batch reminder).
    single_call_turns: u32,
    batch_hints: u32,
    /// How often this worker has been told its graded file is untouched
    /// (the template's unchanged-file reminder).
    seed_hints: u32,
    /// What the model handed over with `explore submit`, once it has. The
    /// harness has it by then: the tool call went to the harness, and this is
    /// the loop's own record that the attempt is over and by whose choice.
    submitted: Option<String>,
    /// Whether the collection instruction has been said (it is said once).
    collecting: bool,
    /// Whether this is the last collection call, including a model retry.
    /// While it is, `submit` is the only tool offered and the only one this
    /// worker will dispatch: a message asking the model to submit is a
    /// request it may ignore, and offering nothing else is the harness owning
    /// the stopping rule, as it does everywhere else in this loop.
    submit_only: bool,
    /// Where this worker is in publishing and comparing its plan.
    planning: Planning,
    /// The tools offered in the request whose reply is being dispatched; a
    /// call to any other is refused.
    request_tools: Vec<String>,
    /// A plan call ran in the reply being dispatched: its later calls are
    /// refused, so the model sees the board before it acts again.
    board_changed: bool,
    nudges: u32,
    compactions: u32,
    input_tokens: u64,
    output_tokens: u64,
    cost_nano_usd: u64,
}

/// What starts a loop: a fresh conversation, or a parent's transcript to
/// continue.
pub enum Start {
    /// No parent transcript: the system and task messages, and the same
    /// `resume` message a continuation gets (the parent node's checked status
    /// and what its siblings tried).
    Fresh {
        system: String,
        task: String,
        resume: String,
    },
    Continued {
        system: String,
        parent_transcript: PathBuf,
        resume: String,
    },
}

/// A continuation drops every system message of the parent transcript and
/// starts with the current rendered one, so it works under the current
/// policy; the source transcript remains unchanged and the new inputs are
/// re-recorded.
fn refresh_system(messages: &mut Vec<Message>, system: &str) {
    messages.retain(|message| message.role != Role::System);
    messages.insert(0, Message::text(Role::System, system.to_owned()));
}

impl<'a> Agent<'a> {
    /// Opens the records in `out` and seeds the buffer: for a fresh start the
    /// rendered system and task messages; for a continuation the parent's
    /// transcript (re-recorded as this worker's own prefix) plus the `resume`
    /// message. `file` is written to the workspace file when none is there
    /// yet: the frozen statement at the root, the parent's file below it.
    /// `frozen` is the frozen file itself, for the statement guard only. A
    /// parent transcript that cannot be loaded is an error here — the
    /// harness's bookkeeping is wrong, and a fresh start would silently
    /// pretend otherwise. `tools` is the kernel's listing; without a `lean
    /// verify` in it the run could never end on a solve, and is refused.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        kernel: &'a mut Kernel,
        options: &'a Options,
        model: ModelAccess,
        model_resource: ResourcePath,
        shell: HandleId,
        workspace: ResourcePath,
        tools: Vec<ToolInfo>,
        problem_id: &str,
        task: Option<&str>,
        out: &Path,
        frozen: &str,
        file: &str,
        start: Start,
        prompts: AgentPrompts,
    ) -> Result<Self, String> {
        if !has_verify(&tools, &options.lean_endpoint) {
            return Err(format!(
                "the tool loop needs a tool-callable `{} verify` among this worker's capabilities; the kernel lists {}",
                options.lean_endpoint,
                tools.iter().map(|t| tool_name(&t.endpoint, &t.action)).collect::<Vec<_>>().join(", ")
            ));
        }
        let steps_log = std::fs::File::create(out.join("attempts.jsonl")).map_err(|e| e.to_string())?;
        let file_name = format!("{problem_id}.lean");
        let (start_kind, messages) = match start {
            Start::Fresh { system, task, resume } => {
                let mut buffer = vec![Message::text(Role::System, system), Message::text(Role::User, task)];
                buffer.push(Message::text(Role::User, resume));
                ("fresh", buffer)
            }
            Start::Continued { system, parent_transcript, resume } => {
                let mut buffer = load_transcript(&parent_transcript)?;
                refresh_system(&mut buffer, &system);
                buffer.push(Message::text(Role::User, resume));
                ("continued", buffer)
            }
        };
        let header = Header {
            v: TRANSCRIPT_VERSION,
            problem: problem_id.to_owned(),
            task: task.map(str::to_owned),
            start: start_kind.to_owned(),
            model: options.model.clone(),
        };
        let mut transcript = Transcript::create(&out.join(TRANSCRIPT_FILE), &header)?;
        for message in &messages {
            transcript.message(message)?;
        }
        let planning = Planning::new(prompts.planning.is_some());
        let mut agent = Self {
            prompts,
            kernel,
            options,
            model,
            model_resource,
            shell,
            workspace,
            specs: tool_specs(&tools),
            tools,
            lean_endpoint: options.lean_endpoint.clone(),
            problem: problem_id.to_owned(),
            file_name,
            transcript,
            out: out.to_path_buf(),
            steps_log,
            messages,
            seed: file.to_owned(),
            frozen: frozen.to_owned(),
            context_tokens: 0,
            cost_nano_usd: 0,
            steps: 0,
            calls: 0,
            single_call_turns: 0,
            batch_hints: 0,
            seed_hints: 0,
            submitted: None,
            collecting: false,
            submit_only: false,
            planning,
            request_tools: Vec::new(),
            board_changed: false,
            nudges: 0,
            compactions: 0,
            input_tokens: 0,
            output_tokens: 0,
        };
        // The workspace file: the statement for a fresh start, the parent's
        // (cloned by the harness) otherwise — written only when there is no
        // file to read, so a clone is never overwritten. Whatever else a
        // parent left at that name (a directory, a file that cannot be read)
        // is replaced rather than ending this worker.
        if !matches!(agent.read_file(&agent.file_name.clone()), Ok(Some(_))) {
            agent.write_file(file)?;
        }
        Ok(agent)
    }

    fn record(&mut self, step: &Step) -> Result<(), String> {
        let line = serde_json::to_string(step).map_err(|e| e.to_string())?;
        writeln!(self.steps_log, "{line}").map_err(|e| e.to_string())
    }

    fn push(&mut self, message: Message) -> Result<(), String> {
        self.transcript.message(&message)?;
        self.messages.push(message);
        Ok(())
    }

    fn shell_call<T: serde::de::DeserializeOwned>(
        &mut self,
        action: &str,
        payload: &impl Serialize,
        deadline_ms: u64,
    ) -> Result<Result<T, Error>, ChannelError> {
        let reply = self.kernel.call(self.shell, action, &self.workspace, payload, Some(deadline_ms))?;
        Ok(reply.and_then(|p| p.decode::<T>()))
    }

    /// True for this worker's own `shell exec` tool.
    fn is_exec_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| {
            t.endpoint.as_str() == self.options.shell_endpoint
                && t.action.as_str() == shell::ACTION_EXEC
                && tool_name(&t.endpoint, &t.action) == name
        })
    }

    /// The kernel's name for this worker's `explore submit`, which a
    /// continuation worker always holds.
    fn submit_tool(&self) -> Option<String> {
        self.tools
            .iter()
            .find(|t| {
                t.endpoint.as_str() == self.options.explore_endpoint
                    && t.action.as_str() == explore::ACTION_SUBMIT
            })
            .map(|t| tool_name(&t.endpoint, &t.action))
    }

    /// Whether `name` is one of this worker's plan tools.
    fn is_plan_tool(&self, name: &str) -> bool {
        self.prompts.planning.as_ref().is_some_and(|p| name == p.publish_tool || name == p.read_tool)
    }

    /// Runs one of the model's tool calls through the kernel: the SDK resolves
    /// the name and arguments into an endpoint, action, resource and payload
    /// (no tool is named here); the handle is this worker's own; the reply, or
    /// the refusal, goes back to the model as JSON. A `lean verify` reply with
    /// `accepted: true` ends the run, and the accepted candidate is written to
    /// the workspace file so that the file and the verdict agree.
    fn dispatch(&mut self, call: &ToolCall, step: &mut Step) -> Result<(String, bool), String> {
        let refused = |e: Error| json!({"error": {"code": e.code, "message": e.message}}).to_string();
        // The graded file by reference: a `verify` without a `candidate`
        // means "the file in my workspace". Sending the whole source costs the
        // model output tokens each time it asks where it stands; by
        // reference, verifying the graded file is the cheapest call it can
        // make.
        let submitting = self.submit_tool().is_some_and(|t| t == call.name);
        if self.submit_only && !submitting {
            let e = Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "your budget is nearly spent, so this worker takes no further `{}`: the only call \
                     left is the submit tool, with the file you want graded or with no arguments at all",
                    call.name
                ),
            );
            return Ok((refused(e), false));
        }
        if self.board_changed {
            let e = Error::new(
                ErrorCode::InvalidInput,
                "not run: a plan call earlier in this reply comes first; read its result, then make this \
                 call again in your next reply if it still applies",
            );
            return Ok((refused(e), false));
        }
        // A tool this worker holds but this request did not offer (a name it
        // does not hold at all is answered below, with the list).
        if !self.request_tools.contains(&call.name) && self.specs.iter().any(|spec| spec.name == call.name) {
            let e = Error::new(
                ErrorCode::InvalidInput,
                format!("`{}` was not offered in this request", call.name),
            );
            return Ok((refused(e), false));
        }
        // `read_plans` takes no arguments. A model may add one anyway, and
        // while this worker must read the board each such call would be
        // refused and forced again, so what the model wrote there is dropped.
        let emptied;
        let call = match &self.prompts.planning {
            Some(p) if p.read_tool == call.name && call.arguments != json!({}) => {
                emptied = ToolCall { arguments: json!({}), ..call.clone() };
                &emptied
            }
            _ => call,
        };
        let coordination = self.is_plan_tool(&call.name);
        if coordination {
            // A plan call ends this reply's dispatch even when it fails: a
            // publication whose outcome is unclear is settled by reading the
            // board (below, for any reply that is not a board), never by
            // going on with work or by publishing again.
            self.board_changed = true;
            if self.planning.phase != Phase::Publish {
                self.planning.phase = Phase::Read;
            }
        }
        let filled = if submitting { self.with_submission(call)? } else { self.with_lean_arguments(call)? };
        let call = &match filled {
            Ok(filled) => self.with_exec_timeout(filled),
            Err(e) => return Ok((refused(e), false)),
        };
        let resolved = match resolve_tool_call(&self.tools, &call.name, &call.arguments) {
            Some(Ok(resolved)) => resolved,
            Some(Err(e)) => return Ok((refused(e), false)),
            None => {
                let known = self.specs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ");
                let e = Error::new(
                    ErrorCode::InvalidInput,
                    format!("no such tool `{}`; the tools are {known}", call.name),
                );
                return Ok((refused(e), false));
            }
        };
        let Some(handle) = self.kernel.find_handle_for(
            resolved.endpoint.as_str(),
            resolved.action.as_str(),
            &resolved.resource,
        ) else {
            return Ok((
                refused(Error::new(ErrorCode::PermissionDenied, "this worker holds no such capability")),
                false,
            ));
        };
        // Long enough for a command at `--bash-timeout-ms`, or a cold verify.
        let deadline =
            self.options.bash_timeout_ms.max(self.options.verify_deadline_ms).saturating_add(30_000);
        let reply = self
            .kernel
            .call(handle, resolved.action.as_str(), &resolved.resource, &resolved.payload, Some(deadline))
            .map_err(|e| e.to_string())?;
        let value = match reply.and_then(|p| p.decode::<Value>()) {
            Ok(value) => value,
            Err(e) => {
                if coordination {
                    self.planning.unsettled();
                }
                return Ok((refused(e), false));
            }
        };
        if coordination {
            match serde_json::from_value::<explore::PlanBoard>(value.clone()) {
                Ok(board) => self.planning.board(board.own.is_some()),
                Err(why) => {
                    self.planning.unsettled();
                    let e =
                        Error::new(ErrorCode::InvalidInput, format!("the plan reply is not a board: {why}"));
                    return Ok((refused(e), false));
                }
            }
        } else if value.get("error").is_none() {
            // Only an ordinary call that was dispatched and answered moves
            // this worker out of planning: a refused one is not work.
            self.planning.work();
        }
        // The harness accepted it: this node's artifact is now what the model
        // chose. The caller ends the loop immediately after recording the result.
        if submitting {
            self.submitted = call.arguments.get("file").and_then(Value::as_str).map(str::to_owned);
        }
        step.exit_code = value.get("exit_code").and_then(Value::as_i64).and_then(|c| i32::try_from(c).ok());
        step.timed_out = value.get("timed_out").and_then(Value::as_bool);
        let is_verify = resolved.endpoint.as_str() == self.lean_endpoint
            && resolved.action.as_str() == lean::ACTION_VERIFY;
        let accepted = is_verify && value.get("accepted").and_then(Value::as_bool) == Some(true);
        if is_verify {
            step.accepted = Some(accepted);
            step.rejection = value.get("rejection").and_then(Value::as_str).map(str::to_owned);
        }
        if accepted && let Some(candidate) = call.arguments.get("candidate").and_then(Value::as_str) {
            self.write_file(candidate)?;
        }
        Ok((bounded_json(value), accepted))
    }

    /// The planning message for the phase this worker is in, if any.
    fn planning_notice(&self) -> Option<String> {
        if self.collecting {
            return None;
        }
        let prompts = self.prompts.planning.as_ref()?;
        match self.planning.phase {
            Phase::Publish => Some(prompts.publish.clone()),
            Phase::Read => Some(prompts.read.clone()),
            Phase::Review => Some(prompts.review.clone()),
            Phase::Disabled | Phase::Work => None,
        }
    }

    fn push_planning_notice(&mut self) -> Result<(), String> {
        if let Some(notice) = self.planning_notice() {
            self.push(Message::text(Role::User, notice))?;
        }
        Ok(())
    }

    /// Clears the conversation at a completed tool turn: the buffer drops to
    /// the system message and the task, followed by the template's notice. It
    /// costs no model call and runs no tool. What survives is what the model
    /// saved in files: the shipped template's notice points it to its
    /// `STATE.md` index and `BRANCHES.json`, and what enters its context
    /// afterwards is what it reads with its own tools; work it did not save is
    /// lost to its context. The boundary and the notice are one transcript
    /// line, written before the buffer changes, so a failed write stops the
    /// loop before another request.
    fn reset_context(&mut self) -> Result<(), String> {
        let notice = self.prompts.context_reset.clone();
        self.transcript
            .write(&Line::ContextReset { before_tokens: self.context_tokens, notice: notice.clone() })?;
        self.messages = kept_prefix(&self.messages);
        self.messages.push(Message::text(Role::User, notice));
        self.compactions += 1;
        self.context_tokens = 0;
        self.single_call_turns = 0;
        self.steps += 1;
        self.record(&Step::new(self.steps, "context_reset"))?;
        self.planning.reset();
        self.push_planning_notice()
    }

    /// Whether to reset before the next call. The token count is the
    /// provider's own (`usage.input_tokens + output_tokens` of the last reply), so it needs
    /// no tokenizer or estimate, and no independent message-count ceiling.
    ///
    /// There is deliberately no byte bound: the kernel is the authority on
    /// what it will carry and says so in an error, while a guess at the
    /// encoded size of the buffer resets either far too early or wrongly at
    /// the margin.
    fn needs_reset(&self) -> bool {
        self.messages.len() >= MIN_MESSAGES_TO_COMPACT
            && self.context_tokens >= self.options.compact_at_tokens
    }

    /// This process is a consumer, not a provider; only termination needs
    /// action while idle. Do not sleep through a supervisor shutdown.
    fn wait_model_retry(&mut self) -> Result<bool, String> {
        let until = Instant::now() + std::time::Duration::from_millis(self.options.model_retry_backoff_ms);
        loop {
            if self.kernel.terminate_requested() {
                return Ok(false);
            }
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(true);
            }
            match self.kernel.next_event_within(remaining).map_err(|e| e.to_string())? {
                Some(nucleus_sdk::Event::Terminate) => return Ok(false),
                None => return Ok(true),
                Some(_) => {}
            }
        }
    }

    /// Freeze the inherited transcript before adding any collection instruction.
    /// The separate log retains every collection input/output for audit and replay.
    fn begin_collection(&mut self) -> Result<(), String> {
        let instruction = self
            .prompts
            .collection
            .as_ref()
            .ok_or("collection prompts missing for the submit capability")?
            .instruction
            .clone();
        self.transcript.file.sync_all().map_err(|e| e.to_string())?;
        let mut transcript = Transcript::create(
            &self.out.join(COLLECTION_TRANSCRIPT_FILE),
            &Header {
                v: TRANSCRIPT_VERSION,
                problem: self.problem.clone(),
                task: None,
                start: "submission".to_owned(),
                model: self.options.model.clone(),
            },
        )?;
        for message in &self.messages {
            transcript.message(message)?;
        }
        self.transcript = transcript;
        self.collecting = true;
        self.steps += 1;
        self.record(&Step::new(self.steps, "collection_started"))?;
        self.push(Message::text(Role::System, instruction))
    }

    /// `budget` proof-search calls, then at most five collection calls if unsolved.
    pub fn run(&mut self, budget: u64) -> Result<LoopEnd, String> {
        let mut end = LoopEnd {
            stopped: "max_attempts",
            error: None,
            calls: 0,
            steps: 0,
            nudges: 0,
            compactions: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_nano_usd: 0,
            complete: false,
            file: None,
            submitted: None,
        };
        if budget > 0 {
            self.push_planning_notice()?;
        }
        'turns: loop {
            if self.kernel.terminate_requested() {
                end.stopped = "terminated";
                break;
            }
            let submit_tool = self.submit_tool();
            let total =
                budget.saturating_add(if submit_tool.is_some() { explore::COLLECTION_CALLS } else { 0 });
            if u64::from(self.calls) >= budget && !self.collecting {
                // A worker that only planned has nothing to collect.
                if self.planning.incomplete() {
                    end.stopped = "plan_incomplete";
                    break;
                }
                if submit_tool.is_some() {
                    self.begin_collection()?;
                } else {
                    end.stopped = "max_attempts";
                    break;
                }
            }
            if u64::from(self.calls) >= total {
                end.stopped = "max_attempts";
                break;
            }
            if !self.collecting && self.needs_reset() {
                self.reset_context()?;
            }
            // Only complete responses enter the transcript or dispatch tools.
            // A fresh model request can therefore retry this step without
            // replaying earlier tools or mixing partial output into the reply.
            let mut retries = 0;
            let (response, mut step) = loop {
                self.submit_only = self.collecting && total.saturating_sub(u64::from(self.calls)) <= 1;
                let mut specs = self.specs.clone();
                if self.collecting {
                    let collection = self
                        .prompts
                        .collection
                        .as_ref()
                        .ok_or("collection prompts missing for the submit capability")?;
                    for spec in &mut specs {
                        if submit_tool.as_ref() == Some(&spec.name) {
                            spec.description = collection.tool_description.clone();
                        }
                    }
                }
                // While this worker plans, the one plan tool its phase needs
                // is the only tool offered, and it is required; collection
                // offers no plan tool at all.
                let forced_plan = self.prompts.planning.as_ref().and_then(|p| {
                    if self.collecting {
                        specs.retain(|spec| spec.name != p.publish_tool && spec.name != p.read_tool);
                        return None;
                    }
                    match self.planning.phase {
                        Phase::Publish => Some(p.publish_tool.clone()),
                        Phase::Read => Some(p.read_tool.clone()),
                        Phase::Disabled | Phase::Review | Phase::Work => None,
                    }
                });
                let tool_choice = if let Some(name) = forced_plan {
                    specs.retain(|spec| spec.name == name);
                    Some(ToolChoice::Named(name))
                } else if self.submit_only {
                    specs.retain(|spec| submit_tool.as_ref() == Some(&spec.name));
                    submit_tool.clone().map(ToolChoice::Named)
                } else {
                    None
                };
                self.request_tools = specs.iter().map(|spec| spec.name.clone()).collect();
                self.board_changed = false;
                let asked = Instant::now();
                let mut seen = crate::Progress::default();
                let response = {
                    let inflight = self.out.join(INFLIGHT_FILE);
                    let step_no = self.steps + 1;
                    let mut watch = |p: crate::Progress| {
                        seen = p;
                        let line = json!({
                            "step": step_no,
                            "waited_ms": p.waited_ms,
                            "produced": p.produced,
                            "first_byte_ms": p.first_byte_ms,
                            "queue": p.queue,
                        });
                        let _ = std::fs::write(&inflight, line.to_string());
                    };
                    complete(
                        self.kernel,
                        self.options,
                        self.model,
                        &self.model_resource,
                        self.messages.clone(),
                        specs.clone(),
                        tool_choice.clone(),
                        &mut watch,
                    )
                    .map_err(|e| e.to_string())?
                };
                let _ = std::fs::remove_file(self.out.join(INFLIGHT_FILE));
                self.calls += 1;
                self.steps += 1;
                let mut step = Step::new(self.steps, "model");
                step.elapsed_ms = elapsed_ms(asked);
                step.first_byte_ms = seen.first_byte_ms;
                step.produced = (seen.produced > 0).then_some(seen.produced);
                match response {
                    Ok(r) => break (r, step),
                    Err(e) => {
                        step.detail = Some(format!("{:?}: {}", e.code, e.message));
                        self.record(&step)?;
                        if self.kernel.terminate_requested() {
                            end.stopped = "terminated";
                            break 'turns;
                        }
                        if retryable_model_error(e.code)
                            && retries < self.options.model_retries
                            && u64::from(self.calls) < if self.collecting { total } else { budget }
                        {
                            retries += 1;
                            self.steps += 1;
                            let mut retry = Step::new(self.steps, "model_retry");
                            retry.detail = Some(format!(
                                "retry {retries} of {} after {} ms",
                                self.options.model_retries, self.options.model_retry_backoff_ms
                            ));
                            self.record(&retry)?;
                            if !self.wait_model_retry()? {
                                end.stopped = "terminated";
                                break 'turns;
                            }
                            continue;
                        }
                        // Exhausting normal calls on a transient model error
                        // still enters collection. No failed fragment or tools
                        // are replayed, and the failed call remains charged.
                        // A worker still planning has nothing to collect.
                        if !self.collecting
                            && submit_tool.is_some()
                            && u64::from(self.calls) >= budget
                            && retryable_model_error(e.code)
                            && !self.planning.incomplete()
                        {
                            continue 'turns;
                        }
                        end.stopped = model_stop(e.code);
                        end.error = Some(format!("{:?}: {}", e.code, e.message));
                        break 'turns;
                    }
                };
            };
            self.input_tokens += response.usage.input_tokens;
            self.output_tokens += response.usage.output_tokens;
            self.cost_nano_usd = self.cost_nano_usd.saturating_add(response.usage.cost_nano_usd);
            self.context_tokens = response.usage.input_tokens.saturating_add(response.usage.output_tokens);
            step.input_tokens = response.usage.input_tokens;
            step.output_tokens = response.usage.output_tokens;
            step.cost_nano_usd = response.usage.cost_nano_usd;
            step.finish_reason = Some(format!("{:?}", response.finish_reason).to_lowercase());
            step.detail = Some(excerpt(&response.message.content, STEP_DETAIL_CHARS));
            self.record(&step)?;
            let mut calls = response.message.tool_calls.clone();
            let finish = response.finish_reason;
            let mut assistant = response.message;
            assistant.role = Role::Assistant;
            let cut_at_cap = finish == FinishReason::Length;
            let dropped = drop_calls_of_a_cut_reply(finish, &mut calls);
            if dropped > 0 {
                // The assistant message loses them too, or the transcript
                // would carry a tool call with no result and the provider
                // would refuse the next request.
                assistant.tool_calls.clear();
            }
            // An assistant message with neither text nor calls is refused by
            // providers on the way back; give it a body.
            if assistant.content.trim().is_empty() && calls.is_empty() {
                assistant.content = "(empty reply)".to_owned();
            }
            self.push(assistant)?;

            if calls.is_empty() {
                if !self.collecting && self.planning.is_planning() {
                    // A planning reply without its plan call gets the
                    // planning message again, not the proof-search nudge.
                    self.nudges += 1;
                    self.steps += 1;
                    self.record(&Step::new(self.steps, "plan_nudge"))?;
                    self.push_planning_notice()?;
                } else if cut_at_cap {
                    self.steps += 1;
                    let mut step = Step::new(self.steps, "continue");
                    if dropped > 0 {
                        step.detail =
                            Some(format!("{dropped} tool call(s) dropped: the reply was cut writing them"));
                    }
                    self.record(&step)?;
                    self.push(Message::text(Role::User, self.prompts.continue_after_length.clone()))?;
                } else {
                    self.nudges += 1;
                    self.steps += 1;
                    self.record(&Step::new(self.steps, "nudge"))?;
                    let nudge = if self.collecting {
                        self.prompts
                            .collection
                            .as_ref()
                            .ok_or("collection prompts missing for the submit capability")?
                            .nudge
                            .clone()
                    } else {
                        self.prompts.nudge.clone()
                    };
                    self.push(Message::text(Role::User, nudge))?;
                }
                continue;
            }
            if calls.len() == 1 {
                self.single_call_turns += 1;
            } else {
                self.single_call_turns = 0;
            }
            let planning = self.planning.is_planning();
            let hint =
                !planning && self.single_call_turns >= BATCH_HINT_AFTER && self.batch_hints < BATCH_HINTS_MAX;
            if hint {
                self.single_call_turns = 0;
                self.batch_hints += 1;
            }
            let seed_hint = !planning
                && self.calls >= SEED_HINT_AFTER
                && self.seed_hints < SEED_HINTS_MAX
                && self.calls.is_multiple_of(SEED_HINT_AFTER)
                && self.read_file(&self.file_name.clone())?.is_some_and(|f| f.trim() == self.seed.trim());
            if seed_hint {
                self.seed_hints += 1;
            }
            let last = calls.len().saturating_sub(1);
            let mut solved = false;
            let mut ran_exec = false;
            for (index, call) in calls.into_iter().enumerate() {
                let started = Instant::now();
                self.steps += 1;
                let mut step = Step::new(self.steps, "tool");
                step.tool = Some(call.name.clone());
                ran_exec |= self.is_exec_tool(&call.name) && !self.board_changed;
                let (result, passed) = self.dispatch(&call, &mut step)?;
                solved |= passed;
                step.elapsed_ms = elapsed_ms(started);
                step.detail = Some(excerpt(&result, STEP_DETAIL_CHARS));
                self.record(&step)?;
                let mut content = result;
                if index == last && !self.board_changed {
                    if hint {
                        content.push_str("\n\n");
                        content.push_str(&self.prompts.batch_hint);
                    }
                    if seed_hint {
                        content.push_str("\n\n");
                        content.push_str(&self.prompts.seed_hint);
                    }
                }
                // Only a command can have changed the file; after the
                // turn's last call, every command of the turn has run.
                if index == last
                    && ran_exec
                    && let Some(notice) = self.statement_notice()
                {
                    content.push_str("\n\n");
                    content.push_str(&notice);
                }
                self.push(Message {
                    role: Role::Tool,
                    content,
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call.id),
                    reasoning_details: None,
                    reasoning: None,
                    parts: Vec::new(),
                })?;
                if passed || self.submitted.is_some() {
                    break;
                }
            }
            if self.board_changed {
                self.push_planning_notice()?;
            }
            if solved {
                end.stopped = "complete";
                end.complete = true;
                break;
            }
            if self.submitted.is_some() {
                end.stopped = "submitted";
                break;
            }
        }
        // A worker that never got past planning did no work and yields no
        // child. A model error, an unknown model outcome and a terminate keep
        // their own names: discovery backs off from a round the provider
        // lost only when its workers stopped on `model_error`.
        if self.planning.incomplete()
            && !self.collecting
            && !matches!(end.stopped, "model_error" | "model_outcome_unknown" | "terminated")
        {
            end.stopped = "plan_incomplete";
        }
        end.calls = self.calls;
        end.steps = self.steps;
        end.nudges = self.nudges;
        end.compactions = self.compactions;
        end.input_tokens = self.input_tokens;
        end.output_tokens = self.output_tokens;
        end.cost_nano_usd = self.cost_nano_usd;
        end.submitted = self.submitted.clone();
        // The workspace file is only what the caller sends when the model did
        // not hand anything over itself. Reading it after a submission costs
        // a shell round trip in the one moment there is none to spare: the
        // harness records the node, the round may be the run's last, and this
        // process is its child.
        if end.submitted.is_none() {
            let name = self.file_name.clone();
            end.file = self.read_file(&name)?;
        }
        Ok(end)
    }
}

/// The `stopped` of a loop that a model error ended. An unknown outcome (a
/// stream whose release the provider did not confirm) is its own reason: the
/// call may still have run, so nothing is retried and no child is made.
fn model_stop(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::PermissionDenied | ErrorCode::BudgetExhausted => "model_budget",
        ErrorCode::UnknownOutcome => "model_outcome_unknown",
        _ => "model_error",
    }
}

/// A reply as the model reads it: compact JSON, every top-level string field
/// cut at [`TOOL_RESULT_CHARS`] (a command's output, Lean's diagnostics).
fn bounded_json(mut value: Value) -> String {
    if let Value::Object(map) = &mut value {
        for field in map.values_mut() {
            if let Value::String(text) = field
                && text.chars().count() > TOOL_RESULT_CHARS
            {
                *text = excerpt(text, TOOL_RESULT_CHARS);
            }
        }
    }
    value.to_string()
}

pub(crate) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Retry model observations only. Denials, stale authority, invalid input,
/// cancellation, and unknown tool outcomes never authorize another attempt.
fn retryable_model_error(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::ServiceError | ErrorCode::Timeout | ErrorCode::Unavailable | ErrorCode::QueueFull
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn continued_context_refreshes_policy_without_retaining_collection_instructions() {
        let mut messages = vec![
            Message::text(Role::System, "old policy"),
            Message::text(Role::User, "proof context"),
            Message::text(Role::System, "old temporary instruction"),
        ];
        refresh_system(&mut messages, "current policy");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], Message::text(Role::System, "current policy"));
        assert_eq!(messages[1], Message::text(Role::User, "proof context"));
    }

    #[test]
    fn model_recovery_never_retries_denials_cancellation_or_unknown_outcomes() {
        for code in [
            ErrorCode::PermissionDenied,
            ErrorCode::BudgetExhausted,
            ErrorCode::Revoked,
            ErrorCode::UnknownHandle,
            ErrorCode::StaleHandle,
            ErrorCode::InvalidInput,
            ErrorCode::Cancelled,
            ErrorCode::ShuttingDown,
            ErrorCode::UnknownOutcome,
        ] {
            assert!(!retryable_model_error(code), "{code:?}");
        }
        assert_eq!(model_stop(ErrorCode::UnknownOutcome), "model_outcome_unknown");
    }

    /// A reply cut at the output cap may be cut *inside* a call, whose
    /// arguments then come back as a truncated fragment (a JSON string, not an
    /// object), so none of a cut reply's calls may run.
    #[test]
    fn a_reply_cut_at_the_output_cap_has_its_tool_calls_dropped_and_nothing_else_does() {
        let call = |args: Value| ToolCall { id: "c1".into(), name: "shell_exec".into(), arguments: args };
        // What a truncated call looks like: the fragment, kept as a string.
        let truncated = call(Value::String("{\"command\": \"cat > f.lean <<'EOF'\\nimport Math".into()));
        let whole = call(serde_json::json!({"command": "ls"}));

        let mut calls = vec![whole.clone(), truncated];
        assert_eq!(drop_calls_of_a_cut_reply(FinishReason::Length, &mut calls), 2);
        assert!(calls.is_empty(), "a cut reply hands over nothing to run");

        for finish in [FinishReason::ToolCalls, FinishReason::Stop, FinishReason::ContentFilter] {
            let mut calls = vec![whole.clone()];
            assert_eq!(drop_calls_of_a_cut_reply(finish, &mut calls), 0, "{finish:?}");
            assert_eq!(calls, vec![whole.clone()], "only a cut reply loses its calls: {finish:?}");
        }
    }
}
