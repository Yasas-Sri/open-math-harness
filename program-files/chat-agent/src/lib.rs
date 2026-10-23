//! Chat agent: waits for a user message, lets a model answer it, runs the
//! tools the model asks for, and posts everything to the conversation.
//!
//! It holds no model credentials. Its ability to act comes from its granted
//! handles and filesystem namespace, including endpoint creation and
//! executable spawning when configured. In the VM a session's agent runs as
//! its project, an ordinary process of the projects' user in no sandbox: a
//! compromised agent can run any code as that user, in every project's
//! folder, and spend or delegate its service budgets. It and the plugins it
//! starts have the VM's network; the plugins take no binds, which the kernel
//! refuses for a project's process.
//!
//! The model's output is untrusted: a tool call is only a request, its
//! arguments are validated here, and the kernel decides whether the resulting
//! call is allowed. A denial is returned to the model as the tool's result,
//! so the model can explain or adapt, and shown to the user as what it is.
//!
//! Bounds: truncated tool results. A turn ends when the
//! model replies without calling a tool, and that reply's text is the answer;
//! or when the person stops it. One exception: a reply with no text at all
//! after the turn's tool work is answered once with `NUDGE`, and the turn
//! ends with the next reply whatever it holds. `--max-steps` optionally caps
//! the model calls per message (no cap by default). A model call that failed
//! transiently is made again within those caps (module `retry`).
//!
//! The conversation: an agent that waits for messages starts from its
//! session's log (module `restore`). Messages marked Send now reach it between
//! steps once the tool calls return; ordinary queued messages wait for the
//! turn's end (conversation interface, "Between steps").
//!
//! Files: with `--workdir`, the session's working directory, the agent reads
//! the files a person attached to a message (module `attachments`) and offers
//! `view_image` (module `images`); images reach the model only when its model
//! is one of `--image-model`. A reply shows the person the images of the
//! working directory it references, whatever the model (module
//! `reply_images`). Every file is opened beneath the working directory with no
//! symbolic link followed (module `workdir`).
//!
//! Irreversible tools (`fs_remove`) are offered only when the agent could use them:
//! directly, which an operator should not arrange, or through the approvals guard,
//! where a person decides each use. The agent cannot tell the difference in
//! advance and does not need to; it says what it wants and gets a result or a refusal.
//!
//! Streaming is a matter of which capabilities the agent holds. Given `start`
//! on its model handle and a second handle for `next` and `cancel`, it pulls
//! the answer in pieces and posts them as provisional entries; otherwise it
//! calls `complete` and posts the answer whole. A call budget on the first
//! handle, where a composition sets one, counts its `start`s and `complete`s;
//! pulls are not charged against it. A session's agent has none.

use std::collections::BTreeMap;

use nucleus_approvals_interface as approvals;
mod attachments;
mod images;
pub mod jobs;
mod plugins;
pub mod reminders;
mod reply_images;
mod restore;
mod retry;
pub mod sessions;
mod skills;
mod stream;
pub mod swarm;
mod workdir;
pub use plugins::ExecutionContext;

use nucleus_conversation_interface as conversation;
use nucleus_conversation_interface::{Entry, ReceiveRequest, ReceiveResponse};
use nucleus_memory_interface as memory;
use nucleus_model_interface as model;
use nucleus_model_interface::{CompletionRequest, CompletionResponse, Message, Role, ToolCall, ToolSpec};
use nucleus_protocol::{
    ActionName, EffectClass, EndpointName, Error, ErrorCode, HandleId, ResourceFrom, ResourcePath, ToolInfo,
};
use nucleus_sdk::{ChannelError, Connection, ToolBinding, ToolCatalog, validate_bindings};
use serde_json::{Value, json};

/// Tool results longer than this are cut before they go back to the model:
/// about 16k tokens, so a long file or log reaches the model whole while one
/// result cannot crowd out the conversation.
const MAX_TOOL_RESULT_CHARS: usize = 64_000;
/// Context tokens — the provider's own count of what the last call cost, input
/// plus output — past which the conversation is summarised and replaced. Only
/// tokens trigger it: a count of messages would summarise a tool-heavy turn
/// (two messages per call) long before its context is full.
const DEFAULT_COMPACT_AT_TOKENS: u64 = 800_000;
/// Messages below which compacting is not worth a model call: there is nothing
/// to summarise that is shorter than what is there.
const MIN_MESSAGES_TO_COMPACT: usize = 4;
/// How long one `receive` stays pending before the agent asks again.
const RECEIVE_DEADLINE_MS: u64 = 25_000;
/// How long one pull waits for text. Also how coarse the streaming looks: a pull
/// returns everything that arrived in the meantime, so text appears in batches.
const PULL_WAIT_MS: u64 = 400;
/// Absolute budget per model request, including streaming startup and pulls.
const DEFAULT_MODEL_DEADLINE_MS: u64 = 1_800_000;
const MAX_MODEL_DEADLINE_MS: u64 = 3_600_000;
/// Further attempts at a failed model call, and the wait before each: the
/// prover's defaults and bound (module `retry`).
const DEFAULT_MODEL_RETRIES: u32 = 3;
const DEFAULT_MODEL_RETRY_BACKOFF_MS: u64 = 2_000;
const MAX_MODEL_RETRY_BACKOFF_MS: u64 = 60_000;
/// Cleanup is bounded independently once the request's budget has expired.
const CANCEL_DEADLINE_MS: u64 = 1_000;
/// How long a call to the memory service may take. Reading what is remembered
/// is on the path to the first answer, so it is short: a memory service that
/// is slow must not be what makes a conversation feel slow.
const MEMORY_DEADLINE_MS: u64 = 10_000;
/// How long a request to the approvals guard may wait for a person. When it runs
/// out the request is void; the model is told, and may ask again.
const APPROVAL_WAIT_MS: u64 = 240_000;

/// What this agent's instructions say about its own authority.
///
/// Every field is derived from what the kernel lists for the process when the
/// prompt is rendered, never from free configuration: the text must not tell
/// the model it can do something it cannot, or that it cannot do something it
/// can.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// Tools the kernel listed for this process (before the agent's own operations).
    pub tools: usize,
    /// A held filesystem or shell tool that writes: `create`, `append`, `edit`
    /// or `remove` on a file service, or shell `exec`. Selects `workspace-write`
    /// rather than `read-only`.
    pub writes_files: bool,
    /// `fs_remove` is offered through the approvals guard. Selects
    /// `on-request` rather than `never`.
    pub guarded: bool,
    /// A held `web` tool, or network declared for this process by the
    /// operator's composition.
    pub network: bool,
    /// `swarm start` is held, so the swarm guidance applies.
    pub swarms: bool,
    /// `shell_exec` offers `background`, so the guidance on background jobs
    /// applies.
    pub jobs: bool,
}

impl Held {
    #[must_use]
    pub fn derive(tools: &[ToolInfo], guarded: bool, network_declared: bool) -> Self {
        let writes_files = tools.iter().any(|tool| {
            tool.effect != EffectClass::Observation
                && (matches!(tool.action.as_str(), "create" | "append" | "edit" | "remove")
                    || (tool.endpoint.as_str() == "shell" && tool.action.as_str() == "exec"))
        });
        let network = network_declared || tools.iter().any(|tool| tool.endpoint.as_str() == "web");
        let jobs = tools.iter().any(|tool| {
            tool.endpoint.as_str() == nucleus_shell_interface::ENDPOINT
                && tool.action.as_str() == nucleus_shell_interface::ACTION_EXEC
                && tool.params.pointer("/properties/background").is_some()
        });
        Self { tools: tools.len(), writes_files, guarded, network, swarms: swarm::coordinates(tools), jobs }
    }
}

/// The system prompt assembled from `nucleus-prompts` for what this agent holds:
/// the model's instructions (personality, tool count, permissions), the
/// capability policy, and the guidance for the swarm and background jobs it holds.
#[must_use]
pub fn instructions(personality: nucleus_prompts::Personality, held: Held) -> String {
    use nucleus_prompts::{ApprovalPolicy, Assembly, NetworkAccess, SandboxMode};
    let assembly = Assembly {
        personality,
        sandbox_mode: if held.writes_files { SandboxMode::WorkspaceWrite } else { SandboxMode::ReadOnly },
        approval_policy: if held.guarded { ApprovalPolicy::OnRequest } else { ApprovalPolicy::Never },
        network: if held.network { NetworkAccess::Enabled } else { NetworkAccess::Disabled },
        tools: held.tools,
    };
    let mut text = nucleus_prompts::render_model_instructions(&assembly);
    text.push_str("\n\n");
    text.push_str(nucleus_prompts::guardian::POLICY.trim());
    if held.swarms {
        text.push_str("\n\n");
        text.push_str(nucleus_prompts::swarm::COORDINATOR.trim());
    }
    if held.jobs {
        text.push_str("\n\n");
        text.push_str(nucleus_prompts::jobs::GUIDANCE.trim());
    }
    text
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Provider model id, e.g. `deepseek/deepseek-v4-flash`.
    pub model: String,
    /// Replaces the assembled instructions entirely (`--system-prompt`).
    pub system_prompt: Option<String>,
    /// Appended to the assembled instructions: a swarm member's role brief.
    pub developer_instructions: Option<String>,
    pub personality: nucleus_prompts::Personality,
    pub max_steps: u32,
    pub max_output_tokens: u32,
    /// Additional generation attempts after a provider output limit, within max_steps.
    /// Defaults to no extra continuation cap; callers may choose a finite one.
    pub max_length_continuations: u32,
    pub model_deadline_ms: u64,
    /// Further attempts at a model call that failed transiently, per model
    /// step; each is a step and a model call (module `retry`).
    pub model_retries: u32,
    /// The wait before each of them; termination interrupts it.
    pub model_retry_backoff_ms: u64,
    /// Context tokens past which the conversation is summarised. The provider's
    /// own count, never an estimate.
    pub compact_at_tokens: u64,
    /// Passed to the provider on every call; `None` leaves its default.
    pub reasoning_effort: Option<nucleus_model_interface::ReasoningEffort>,
    /// Where this agent's transcript is kept, as an `fs` resource path.
    ///
    /// A swarm member writes here so that it can be continued: a continuation
    /// is this transcript plus one more message.
    pub transcript: Option<Vec<String>>,
    /// Start from a transcript written earlier, then take the task as the next
    /// message: how a swarm member goes on after the judge's answer, or when a
    /// restart interrupted its swarm and the swarm is resumed. A call the
    /// transcript leaves without a result gets one saying it did not return,
    /// and a long one is bounded as a session's history is
    /// (`restore::transcript`).
    pub resume: Option<Vec<String>>,
    /// One-shot mode: answer this and exit, instead of waiting for messages.
    ///
    /// A one-shot agent holds `post` on a conversation, if any, and not
    /// `receive`: nothing can be sent to it afterwards, so the task has to be
    /// complete on its own.
    pub task: Option<String>,
    /// The sessions service (module `sessions`): instead of serving a session,
    /// start every session's agent from this executable, this program, as the
    /// session's project. Options `child_*`, `projects` and the scopes are
    /// this mode's.
    pub child_executable: Option<String>,
    /// Arguments every session's agent starts with, before its session's own.
    pub child_args: Vec<String>,
    /// Paths of this process's namespace every agent sees at the same path,
    /// read-only, or writable (`--child-bind-writable`): a directory every
    /// session shares, for a composition without projects.
    pub child_binds: Vec<(String, bool)>,
    /// Where the projects' folders are: their real path, which this
    /// service's sandbox has them at too, so an agent is told the same path
    /// its shell prints.
    pub projects: Option<String>,
    /// Endpoints whose delegable capabilities an agent gets narrowed to its
    /// project's repository (`--repo-scope`), or to its project.
    pub repo_scoped: Vec<String>,
    pub project_scoped: Vec<String>,
    /// Endpoints whose delegable capabilities an agent gets narrowed to its
    /// own session, `[project, session]` (`--session-scope`).
    pub session_scoped: Vec<String>,
    /// Operator-configured environment/workspace names for already held tools.
    /// These select ordinary capabilities; they never grant new ones.
    pub tool_bindings: Vec<ToolBinding>,
    /// Guest path metadata for ordinary plugin creation, supplied by the launcher.
    pub execution_context: Option<ExecutionContext>,
    /// The VM is an organisation's, shared by its members (`--organisation`):
    /// the instructions say so, and how a message names its author.
    pub organisation: bool,
    /// The session's working directory in this process's namespace
    /// (`--workdir`, set by the sessions service): where the files attached
    /// to a message and `view_image` paths are read.
    pub workdir: Option<String>,
    /// Models that take image input (`--image-model`, repeated): images reach
    /// the model only when it is one of them.
    pub image_models: Vec<String>,
    /// A swarm member (`--swarm-member free|holding`): whether
    /// it starts holding a direction. Its loop refuses other tools until it
    /// holds one (`swarm::member`).
    pub swarm_member: Option<bool>,
    /// The project an agent without a conversation works for (`--project`,
    /// given by a swarm's service and the memory service), which its model
    /// calls name; an agent with a conversation names its session's
    /// ([`project_of`]). Metadata, never authority.
    pub project: Option<String>,
}

impl Options {
    /// Parses `--model <id>`, then `[--personality <p>] [--developer-instructions <text>]
    /// [--max-steps <n>] [--max-output-tokens <n>] [--model-deadline-ms <n>]
    /// [--model-retries <n>] [--model-retry-backoff-ms <n>]
    /// [--compact-at-tokens <n>] [--system-prompt <text>] [--task <text>]
    /// [--transcript <fs path>] [--resume <fs path>] [--reasoning-effort <e>]
    /// [--max-length-continuations <n>]
    /// [--tool-bindings <JSON array>] [--execution-context <JSON object>]
    /// [--workdir <dir>] [--image-model <id>]... [--project <name>]
    /// [--organisation] [--swarm-member free|holding]`, or, for the sessions
    /// service, `--child-executable <path> [--child-arg <arg>]...
    /// [--child-bind <path>]... [--child-bind-writable <path>]... [--projects <dir>]
    /// [--repo-scope <endpoint>]... [--project-scope <endpoint>]...
    /// [--session-scope <endpoint>]... [--tool-bindings <JSON array>]
    /// [--execution-context <JSON object>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            model: String::new(),
            system_prompt: None,
            developer_instructions: None,
            personality: nucleus_prompts::Personality::default(),
            max_steps: u32::MAX,
            max_output_tokens: 0,
            max_length_continuations: u32::MAX,
            model_deadline_ms: DEFAULT_MODEL_DEADLINE_MS,
            model_retries: DEFAULT_MODEL_RETRIES,
            model_retry_backoff_ms: DEFAULT_MODEL_RETRY_BACKOFF_MS,
            compact_at_tokens: DEFAULT_COMPACT_AT_TOKENS,
            reasoning_effort: None,
            transcript: None,
            resume: None,
            task: None,
            child_executable: None,
            child_args: Vec::new(),
            child_binds: Vec::new(),
            projects: None,
            repo_scoped: Vec::new(),
            project_scoped: Vec::new(),
            session_scoped: Vec::new(),
            tool_bindings: Vec::new(),
            execution_context: None,
            organisation: false,
            workdir: None,
            image_models: Vec::new(),
            swarm_member: None,
            project: None,
        };
        let number =
            |flag: &str, value: &str| value.parse::<u64>().map_err(|_| format!("{flag} must be a number"));
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if flag == "--organisation" {
                options.organisation = true;
                continue;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--model" => options.model = value,
                "--max-steps" => {
                    options.max_steps = u32::try_from(number(&flag, &value)?).unwrap_or(u32::MAX)
                }
                "--max-output-tokens" => {
                    options.max_output_tokens = u32::try_from(number(&flag, &value)?).unwrap_or(u32::MAX);
                }
                "--max-length-continuations" => {
                    options.max_length_continuations =
                        u32::try_from(number(&flag, &value)?).unwrap_or(u32::MAX);
                }
                "--model-deadline-ms" => options.model_deadline_ms = number(&flag, &value)?,
                "--model-retries" => {
                    options.model_retries = u32::try_from(number(&flag, &value)?).unwrap_or(u32::MAX)
                }
                "--model-retry-backoff-ms" => options.model_retry_backoff_ms = number(&flag, &value)?,
                "--compact-at-tokens" => options.compact_at_tokens = number(&flag, &value)?,
                "--task" => options.task = Some(value),
                "--child-executable" => options.child_executable = Some(value),
                "--child-arg" => options.child_args.push(value),
                "--child-bind" => options.child_binds.push((value, false)),
                "--child-bind-writable" => options.child_binds.push((value, true)),
                "--projects" => options.projects = Some(value),
                "--repo-scope" => options.repo_scoped.push(value),
                "--project-scope" => options.project_scoped.push(value),
                "--session-scope" => options.session_scoped.push(value),
                "--workdir" => options.workdir = Some(value),
                "--image-model" => {
                    if options.image_models.contains(&value) || options.image_models.len() >= 16 {
                        return Err("--image-model: duplicate, or more than 16".into());
                    }
                    options.image_models.push(value);
                }
                "--transcript" => options.transcript = Some(segments(&value)),
                "--resume" => options.resume = Some(segments(&value)),
                sessions::EFFORT_FLAG => {
                    options.reasoning_effort = Some(
                        nucleus_model_interface::ReasoningEffort::parse(&value)
                            .ok_or_else(|| format!("--reasoning-effort: no such effort: {value}"))?,
                    );
                }
                "--system-prompt" => options.system_prompt = Some(value),
                "--developer-instructions" => options.developer_instructions = Some(value),
                swarm::member::FLAG => options.swarm_member = Some(swarm::member::parse(&value)?),
                "--project" => {
                    if !nucleus_protocol::valid_project(&value) {
                        return Err(format!("--project {value:?} is not a project's name"));
                    }
                    options.project = Some(value);
                }
                "--personality" => {
                    options.personality = nucleus_prompts::Personality::parse(&value)
                        .ok_or_else(|| format!("--personality: no such personality: {value}"))?;
                }
                "--tool-bindings" => {
                    if value.len() > 128 * 1024 {
                        return Err("--tool-bindings exceeds 128 KiB".into());
                    }
                    options.tool_bindings =
                        serde_json::from_str(&value).map_err(|e| format!("tool bindings: {e}"))?;
                    validate_bindings(&options.tool_bindings).map_err(|e| format!("tool bindings: {e}"))?;
                }
                "--execution-context" => {
                    if value.len() > 16 * 1024 {
                        return Err("--execution-context exceeds 16 KiB".into());
                    }
                    let context: ExecutionContext =
                        serde_json::from_str(&value).map_err(|e| format!("execution context: {e}"))?;
                    context.validate()?;
                    options.execution_context = Some(context);
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let service = options.child_executable.is_some();
        if service {
            // The sessions service thinks with no model of its own.
            if !options.model.is_empty() || options.project.is_some() {
                return Err("the sessions service (--child-executable) runs no model of its own".into());
            }
            if options.task.is_some() || options.resume.is_some() || options.transcript.is_some() {
                return Err("the sessions service serves no task, transcript or continuation".into());
            }
            if options.organisation || !options.image_models.is_empty() {
                return Err(
                    "--organisation and --image-model are an agent's: pass them with --child-arg".into()
                );
            }
            if options.workdir.is_some() {
                return Err("--workdir is an agent's: the service sets it for each session".into());
            }
            // Not the operator's to fix: the service starts each agent at the
            // effort its `sessions create` names.
            if options.child_args.iter().any(|arg| arg == sessions::EFFORT_FLAG) {
                return Err("--child-arg --reasoning-effort is refused: \
                            `sessions create` names each agent's reasoning effort"
                    .into());
            }
            // The repository is a directory of a project's folder; a project
            // scope is only the session's project's name.
            if options.projects.is_none() && !options.repo_scoped.is_empty() {
                return Err("--repo-scope needs --projects".into());
            }
        } else {
            if options.model.is_empty() {
                return Err("--model is required".to_owned());
            }
            let mode_only = !options.child_args.is_empty()
                || !options.child_binds.is_empty()
                || options.projects.is_some()
                || !options.repo_scoped.is_empty()
                || !options.project_scoped.is_empty()
                || !options.session_scoped.is_empty();
            if mode_only {
                return Err("--child-*, --projects and the scopes are the sessions service's \
                            (--child-executable)"
                    .into());
            }
        }
        if options.max_steps == 0 {
            return Err("--max-steps must be at least 1".to_owned());
        }
        if options.swarm_member.is_some() && options.task.is_none() {
            return Err("a swarm member (--swarm-member) runs one task (--task)".into());
        }
        if options.resume.is_some() && options.task.is_none() {
            return Err(
                "--resume continues a task (--task); an agent that waits for messages continues its \
                        session's conversation"
                    .into(),
            );
        }
        if !(1..=MAX_MODEL_DEADLINE_MS).contains(&options.model_deadline_ms) {
            return Err(format!("--model-deadline-ms must be between 1 and {MAX_MODEL_DEADLINE_MS}"));
        }
        if options.model_retry_backoff_ms > MAX_MODEL_RETRY_BACKOFF_MS {
            return Err(format!("--model-retry-backoff-ms must be at most {MAX_MODEL_RETRY_BACKOFF_MS}"));
        }
        // Use the same budget validation as the provider interface. Fail before
        // starting IPC, rather than spend a call discovering invalid settings.
        CompletionRequest {
            messages: vec![Message::text(Role::User, "validate generation settings")],
            tools: Vec::new(),
            max_output_tokens: options.max_output_tokens,
            temperature: None,
            reasoning_effort: options.reasoning_effort,
            tool_choice: None,
            project: None,
        }
        .validate()
        .map_err(|e| format!("generation settings: {}", e.message))?;
        Ok(options)
    }
}

/// A hand-written tool: deleting a file through the approvals guard. The
/// agent never holds `fs remove`; it offers this so the model can ask, and the
/// guard (with a person) decides.
const FS_REMOVE_TOOL: &str = "fs_remove";

fn guard_remove_spec() -> ToolSpec {
    ToolSpec {
        name: FS_REMOVE_TOOL.into(),
        description: "Permanently delete one file. This cannot be undone, and a person may be asked to approve it first, which can take a while.".into(),
        parameters: json!({"type": "object",
            "properties": {"path": {"type": "string", "description": "Path relative to the workspace root"},
                           "reason": {"type": "string", "description": "Why the file should be deleted, for the person who approves"}},
            "required": ["path", "reason"]}),
    }
}

/// The provider says the context is nearly full, and there is more than a
/// summary would replace. Never a length in bytes or messages: the thing being
/// bounded is tokens, and the provider is the one who counts them.
fn compaction_due(messages: usize, context_tokens: u64, threshold: u64) -> bool {
    messages >= MIN_MESSAGES_TO_COMPACT && context_tokens >= threshold
}

/// How full the context is after a call: the provider's count, or the last
/// one when a reply carried none, so a missing count never reads as empty.
fn context_after(previous: u64, usage: &model::Usage) -> u64 {
    match usage.input_tokens {
        0 => previous,
        input => input.saturating_add(usage.output_tokens),
    }
}

/// A tool's reply as the model reads it: within [`MAX_TOOL_RESULT_CHARS`],
/// cutting the longest string field (a command's output) inside the reply
/// first, so the fields beside it (its exit status, whether it timed out,
/// what it left running) still reach the model; then as [`truncate`].
fn bounded_result(value: Value) -> String {
    let text = value.to_string();
    let excess = text.chars().count().saturating_sub(MAX_TOOL_RESULT_CHARS);
    let Value::Object(mut map) = value else { return truncate(&text) };
    let longest =
        map.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.chars().count()))).max_by_key(|f| f.1);
    let Some((field, length)) = longest.filter(|_| excess > 0) else { return truncate(&text) };
    // Every character cut removes at least one from the escaped text.
    let kept = length.saturating_sub(excess + 200);
    if let Some(Value::String(long)) = map.get_mut(&field) {
        *long = format!(
            "{}\n[cut: the first {kept} of {length} characters]",
            long.chars().take(kept).collect::<String>()
        );
    }
    truncate(&Value::Object(map).to_string())
}

fn truncate(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_TOOL_RESULT_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    format!("{kept}\n[truncated: the first {MAX_TOOL_RESULT_CHARS} of {total} characters]")
}

/// A compaction summary within what the conversation keeps of one
/// ([`conversation::MAX_SUMMARY_BYTES`]), cut on a character boundary.
fn bounded_summary(summary: String) -> String {
    if summary.len() <= conversation::MAX_SUMMARY_BYTES {
        return summary;
    }
    let marker = format!("\n[the summary was cut here: it had {} bytes]", summary.len());
    let mut end = conversation::MAX_SUMMARY_BYTES - marker.len();
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &summary[..end])
}

/// A conversation entry is a display record, not the tool's invocation. Large
/// arguments remain intact in the model message and actual dispatch payload.
fn tool_arguments_preview(arguments: &Value) -> Value {
    let bytes = arguments.to_string().len();
    if bytes <= conversation::MAX_TEXT_BYTES {
        arguments.clone()
    } else {
        Value::String(format!(
            "[Arguments omitted from conversation preview: {bytes} bytes; execution uses the complete arguments]"
        ))
    }
}

/// How a result ends that the conversation keeps less of than the model got
/// (`tool_result_preview`): a restarted agent replays what was kept and says
/// so (module `restore`).
const RESULT_SHORTENED: &str = "\n[Result shortened for conversation preview: ";

fn tool_result_preview(summary: &str) -> String {
    if summary.len() <= conversation::MAX_TEXT_BYTES {
        return summary.to_owned();
    }
    let marker = format!("{RESULT_SHORTENED}{} original bytes; model result is unchanged]", summary.len());
    let mut end = conversation::MAX_TEXT_BYTES - marker.len();
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &summary[..end])
}

fn tool_message(call: &ToolCall, outcome: &Result<String, Error>) -> Message {
    let content = match outcome {
        Ok(result) => result.clone(),
        Err(e) => json!({"error": {"code": e.code, "message": e.message}}).to_string(),
    };
    Message {
        role: Role::Tool,
        content,
        tool_calls: Vec::new(),
        tool_call_id: Some(call.id.clone()),
        reasoning_details: None,
        reasoning: None,
        parts: Vec::new(),
    }
}

/// An `fs` resource path written as `a/b/c` on the command line.
fn segments(text: &str) -> Vec<String> {
    text.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

type Kernel = Connection<std::io::Stdin, std::io::Stdout>;

struct Agent<'a> {
    kernel: Kernel,
    options: &'a Options,
    session: ResourcePath,
    /// The project its model calls are made for ([`project_of`]).
    project: Option<String>,
    /// The model, reasoning effort and harness mode the current turn runs with:
    /// its opening message's settings, else this agent's own ([`Agent::settle`]).
    model_name: String,
    model_resource: ResourcePath,
    effort: Option<model::ReasoningEffort>,
    mode: model::ReasoningMode,
    /// The conversation this agent serves, when it has one. A one-shot agent
    /// whose whole output is the files it edits has none, and what it would
    /// have said goes to stderr.
    conversation: Option<HandleId>,
    model: HandleId,
    /// The memory service, when this agent was given it: what to inject at the
    /// start, and where to report what the model actually used.
    memory: Option<HandleId>,
    /// What this session is told it remembers, read once at the start. Data
    /// about what has happened before, never authority: it is in front of the
    /// model because the model should know it, and the kernel decides what may
    /// be done about it.
    remembered: Option<String>,
    /// The list of skills, read once at the start (module `skills`), a message of its own.
    skills: Option<String>,
    /// The list could not be read at the start: it is read again before each turn until it
    /// is, so that one slow load does not leave this agent without skills for its life.
    skills_unread: bool,
    /// The `/skill` commands of the messages being answered, which this agent
    /// answers with its own calls before the model's step (module `skills`).
    skill_commands: Vec<Option<String>>,
    /// The callable tools, from the kernel, refreshed each turn. Every service
    /// the agent can reach appears here; it needs no per-service code.
    tools: Vec<ToolInfo>,
    /// Exactly the tool routing table presented to the model for this turn.
    catalog: Option<ToolCatalog>,
    /// The guard, if held: lets the agent offer `fs_remove` and route it there.
    approvals: Option<HandleId>,
    /// What the instructions say this agent holds, from the last tool refresh.
    held: Held,
    /// The handle for `next` and `cancel`, when this agent was given one. With it,
    /// answers are streamed; without it, each arrives whole.
    streaming: Option<HandleId>,
    message_namespace: String,
    answers: u64,
    history: images::History,
    /// The session's working directory, opened, when this agent has one.
    workdir: Option<workdir::Workdir>,
    /// Whether this agent's model takes images (`--image-model`).
    image_input: bool,
    /// Images of the person's message about to be answered: the next user
    /// message remembered carries them after its text.
    prompt_images: Vec<images::Held>,
    /// Images `view_image` read in the current batch of tool calls: a user
    /// message after the batch's last result carries them.
    tool_images: Vec<images::Held>,
    /// The provider's own count for the last call: input plus output. The only
    /// honest measure of how full the context is — bytes are not tokens and an
    /// estimate drifts from the thing it estimates.
    context_tokens: u64,
    compactions: u32,
    /// Where each message is appended, when this agent keeps a transcript.
    transcript: Option<(HandleId, ResourcePath)>,
    plugins: plugins::Plugins,
    /// What a swarm member's own loop knows of its claim.
    swarm_member: Option<swarm::member::Member>,
    /// Who has written and their last selected mode, earlier agents' turns included
    /// (`reminders::queued_prompt`).
    authors: BTreeMap<Option<String>, model::ReasoningMode>,
}

/// A model outcome and the provisional display records it may replace.
struct StreamedCompletion {
    response: Result<CompletionResponse, Error>,
    message_id: Option<String>,
    reasoning_shown: bool,
}

/// Conversation entries have a byte limit, independently of the model's token
/// budget. Preserve every byte while keeping each UTF-8 string independently valid.
fn text_chunks(mut text: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        let mut end = text.len().min(conversation::MAX_TEXT_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, rest) = text.split_at(end);
        text = rest;
        Some(chunk)
    })
}

/// Display correlation, not authority. Kernel process ids distinguish siblings;
/// a fresh randomized namespace also distinguishes restarts of the entire kernel
/// against the same durable conversation (where its process ids start over).
fn message_namespace(process: nucleus_protocol::ProcessId) -> String {
    use std::hash::BuildHasher;
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let nonce = std::collections::hash_map::RandomState::new().hash_one((process.0, now, std::process::id()));
    format!("p{}-{now:x}-{nonce:016x}", process.0)
}

/// Each final fragment has its own replacement identity, while retaining its
/// association with the first fragment across posts from other agents.
fn final_chunk_id(base: &str, index: usize) -> String {
    if index == 0 { base.to_owned() } else { format!("{base}:chunk:{index}") }
}

/// The model's next input after a response that stopped at its output limit
/// (finish reason `length`): the response's text is kept, its tool calls are
/// dropped unexecuted, and this message asks for the rest. It goes into the
/// model's history and the transcript only, never into the conversation as
/// something the person said. A caller may bound continuations with
/// `--max-length-continuations`, or all model calls with `--max-steps`; neither
/// imposes a finite policy limit by default. Each continuation uses the same
/// per-request generation settings.
const LENGTH_CONTINUATION: &str = "The previous response was truncated and is incomplete. \
No tool calls attached to that incomplete response were executed; they were discarded. \
Continue and complete the response without repeating the preserved partial answer. If a tool is still needed, \
generate its complete tool call again, including all arguments. Do not repeat tools that already \
completed earlier in this conversation. If only reasoning was produced, provide the answer \
or complete tool calls.";

/// What the model is told, once in a turn, when its reply after the turn's
/// tool work holds neither text nor a tool call: the turn would end with
/// nothing said to the person.
const NUDGE: &str = "Your last reply was empty: no text and no tool call. Give the person your answer now, \
as text: what you did and found, what you could not do, or the question you need answered.";

impl Agent<'_> {
    fn next_message_id(&mut self) -> String {
        self.answers += 1;
        format!("{}-a{}", self.message_namespace, self.answers)
    }

    /// Posts a response's text as the answer of record, with the images it
    /// references (module `reply_images`) on its first piece, and returns the
    /// note the model reads on those it could not show.
    fn post_assistant(
        &mut self,
        text: &str,
        mut message_id: Option<String>,
    ) -> Result<Option<String>, ChannelError> {
        if !text.is_empty() && message_id.is_none() {
            message_id = Some(self.next_message_id());
        }
        // One identified final entry replaces ALL deltas. Subsequent fragments
        // have distinct, associated ids: anonymous chunks could be mistaken for
        // another agent's answer when its posts interleave with ours.
        if text.is_empty() && message_id.is_some() {
            self.post(&Entry::Assistant {
                text: String::new(),
                message_id,
                images: Vec::new(),
                images_refused: Vec::new(),
            })?;
            return Ok(None);
        }
        let (mut images, mut images_refused) = match (&self.workdir, self.conversation) {
            (Some(workdir), Some(_)) => reply_images::read(workdir, text),
            _ => (Vec::new(), Vec::new()),
        };
        let note = reply_images::note(&images_refused);
        if let Some(base) = message_id {
            for (index, chunk) in text_chunks(text).enumerate() {
                self.post(&Entry::Assistant {
                    text: chunk.to_owned(),
                    message_id: Some(final_chunk_id(&base, index)),
                    images: std::mem::take(&mut images),
                    images_refused: std::mem::take(&mut images_refused),
                })?;
            }
        }
        Ok(note)
    }

    /// Tells the model, once, which images its last response referenced and
    /// the person was not shown (module `reply_images`).
    fn remember_refused(&mut self, note: Option<String>) {
        if let Some(note) = note {
            self.remember(Message::text(Role::User, note));
        }
    }

    /// Provider-supplied readable reasoning is a separate display record, not
    /// an answer or a tool instruction. Final chunks replace only its deltas.
    fn post_reasoning(&mut self, text: &str, message_id: &str) -> Result<(), ChannelError> {
        if text.is_empty() {
            return self.post(&Entry::Reasoning { text: String::new(), message_id: message_id.into() });
        }
        for (index, chunk) in text_chunks(text).enumerate() {
            self.post(&Entry::Reasoning {
                text: chunk.into(),
                message_id: final_chunk_id(message_id, index),
            })?;
        }
        Ok(())
    }

    fn model_failure(&self, error: &Error, elapsed: std::time::Duration) -> String {
        format!(
            "the model call failed ({:?}): {}; model={}; elapsed={} ms; configured limit={} ms",
            error.code,
            error.message,
            self.model_name,
            elapsed.as_millis(),
            self.options.model_deadline_ms
        )
    }

    /// The reasoning effort a session's own agent runs at, as its
    /// `model_call` records say it (conversation interface, "The agent's
    /// reasoning effort"): the log is what keeps it once this agent is gone.
    /// A one-shot agent says none: its effort is not its session's setting.
    fn session_effort(&self) -> Option<model::EffortSetting> {
        match self.options.task {
            Some(_) => None,
            None => model::EffortSetting::of(self.effort),
        }
    }

    /// Settles what the turn about to start runs with ([`conversation::MessageSettings`]):
    /// the opening message's model, reasoning effort and mode, each absent one
    /// this agent's own. A model it holds no `complete` capability for is not
    /// one it can switch to: the turn runs on this agent's own. Ultramath reasons
    /// at Max.
    fn settle(&mut self, settings: Option<&conversation::MessageSettings>) {
        let settings = settings.cloned().unwrap_or_default();
        let wanted = settings.model.unwrap_or_else(|| self.options.model.clone());
        let callable = |kernel: &Kernel, name: &str| {
            let resource = model::resource_for(name).ok()?;
            let handle = kernel.find_handle_for(model::ENDPOINT, model::ACTION_COMPLETE, &resource)?;
            Some((resource, handle))
        };
        let (name, (resource, handle)) = match callable(&self.kernel, &wanted) {
            Some(found) => (wanted, found),
            None => match callable(&self.kernel, &self.options.model) {
                Some(found) => (self.options.model.clone(), found),
                None => return,
            },
        };
        if name != self.model_name {
            let can_start =
                self.kernel.handles().iter().any(|h| {
                    h.handle == handle && h.actions.iter().any(|a| a.as_str() == model::ACTION_START)
                });
            self.streaming = self
                .kernel
                .find_handle_for(model::ENDPOINT, model::ACTION_NEXT, &resource)
                .filter(|_| can_start);
            self.image_input = self.options.image_models.contains(&name);
            (self.model_name, self.model_resource, self.model) = (name, resource, handle);
        }
        self.mode = settings.reasoning_mode.unwrap_or_default();
        self.effort = match (self.mode, settings.reasoning_effort) {
            (model::ReasoningMode::Ultramath, _) => Some(model::ReasoningEffort::Max),
            (_, Some(setting)) => setting.effort(),
            (_, None) => self.options.reasoning_effort,
        };
    }

    fn fail_turn(&mut self, why: impl Into<String>) -> Result<String, Box<dyn std::error::Error>> {
        let why = why.into();
        self.post(&Entry::Error { text: why.clone() })?;
        self.post(&Entry::TurnComplete)?;
        if self.options.task.is_some() {
            return Err(why.into());
        }
        Ok(String::new())
    }

    fn post(&mut self, entry: &Entry) -> Result<(), ChannelError> {
        if let Err(e) = self.post_checked(entry)? {
            eprintln!("nucleus-chat-agent: could not post to the conversation: {e}");
        }
        Ok(())
    }

    fn post_checked(&mut self, entry: &Entry) -> Result<Result<(), Error>, ChannelError> {
        let Some(handle) = self.conversation else {
            // Nothing to post into. Saying it anyway, where an operator can
            // read it, beats an agent that fails in silence. A swarm member's
            // transcript is its record; only its errors go to stderr.
            if self.swarm_member.is_none() || matches!(entry, Entry::Error { .. }) {
                eprintln!("nucleus-chat-agent: {}", serde_json::to_string(entry).unwrap_or_default());
            }
            if let (Some(member), Entry::ModelCall { step, .. }) = (self.swarm_member, entry) {
                member.report(&mut self.kernel, u64::from(*step))?;
            }
            return Ok(Ok(()));
        };
        Ok(self.kernel.call(handle, conversation::ACTION_POST, &self.session, entry, None)?.map(|_| ()))
    }

    fn stop_unobserved_tools(
        &mut self,
        pending: &[ToolCall],
        why: String,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // The accepted assistant message contains the whole batch. Pair every
        // call we did not dispatch before ending the turn, including in a saved
        // transcript, so subsequent input cannot replay an unresolved batch.
        let outcome = Err(Error::new(ErrorCode::ServiceError, "not executed: conversation recording failed"));
        for call in pending {
            self.remember(tool_message(call, &outcome));
        }
        eprintln!("nucleus-chat-agent: {why}");
        self.fail_turn(why)
    }

    /// Tells the memory service what this turn used and that the session has
    /// moved on.
    ///
    /// The citations are the model's, so they are counted the way the read
    /// path counts them: one session's use of one memory, however many times
    /// it named it. What is reported decides which memories survive the next
    /// consolidation, which is why the model is not the one making the call.
    fn report_memory(&mut self, answer: &str) -> Result<(), ChannelError> {
        let Some(handle) = self.memory else { return Ok(()) };
        let cited = nucleus_memories_read::citations::parse(answer);
        let used = nucleus_memories_read::usage::from_citations(&cited);
        if !used.is_empty() {
            let request = memory::UsedRequest { paths: used.paths.into_iter().collect() };
            let reply = self.kernel.call(
                handle,
                memory::ACTION_USED,
                &self.session,
                &request,
                Some(MEMORY_DEADLINE_MS),
            )?;
            if let Err(e) = reply {
                eprintln!("nucleus-chat-agent: what was used could not be reported: {e}");
            }
        }
        let reply = self.kernel.call(
            handle,
            memory::ACTION_REMEMBER,
            &self.session,
            &memory::RememberRequest {},
            Some(MEMORY_DEADLINE_MS),
        )?;
        if let Err(e) = reply {
            eprintln!("nucleus-chat-agent: the session could not be handed back: {e}");
        }
        Ok(())
    }

    /// How this agent is to behave: an explicit override, or the assembled
    /// instructions for what it holds, followed by a role's brief.
    ///
    /// In Ultramath mode, which a turn takes from its opening message's
    /// settings, an agent that is not a swarm member also gets the proactive
    /// swarm guidance (`nucleus_prompts::swarm::PROACTIVE`) when it holds
    /// `swarm start`, or a note that the swarm tools are unavailable; this is
    /// appended after an explicit `--system-prompt` too. Every model call and
    /// every compaction starts from this text (`opening`, `compact`), so
    /// compaction never drops it.
    fn system_prompt(&self) -> String {
        let mut text = self
            .options
            .system_prompt
            .clone()
            .unwrap_or_else(|| instructions(self.options.personality, self.held));
        if self.mode == model::ReasoningMode::Ultramath && self.options.swarm_member.is_none() {
            text.push_str("\n\n");
            text.push_str(if self.held.swarms {
                nucleus_prompts::swarm::PROACTIVE.trim()
            } else {
                "Ultramath mode is selected, but Ultramath tools are unavailable. Solve the task directly with available tools and explain this limitation when relevant. Do not claim to have started parallel agents."
            });
        }
        if self.options.system_prompt.is_some() {
            return text;
        }
        if self.options.organisation {
            text.push_str("\n\n");
            text.push_str(ORGANISATION);
        }
        // Its replies show images when it has files to read them from and a
        // conversation to show them in.
        if self.workdir.is_some() && self.conversation.is_some() {
            text.push_str("\n\n");
            text.push_str(reply_images::INSTRUCTION);
        }
        // How to use skills; the list itself is data, a message of its own (`opening`).
        if self.skills.is_some() {
            text.push_str("\n\n");
            text.push_str(nucleus_prompts::skills::GUIDANCE.trim());
        }
        if let Some(brief) = &self.options.developer_instructions {
            text.push_str("\n\n");
            text.push_str(brief.trim());
        }
        text
    }

    /// The messages every call starts with: how this agent is to behave, and
    /// then what it remembers, which is data and says so.
    fn opening(&self) -> Vec<Message> {
        let mut messages = vec![Message::text(Role::System, self.system_prompt())];
        if let Some(remembered) = &self.remembered {
            messages.push(Message::text(Role::System, remembered.clone()));
        }
        if let Some(skills) = &self.skills {
            messages.push(Message::text(Role::System, skills.clone()));
        }
        if let Some(catalog) = &self.catalog {
            messages.push(Message::text(Role::System, format!(
                "Current tool execution environments and service descriptions follow. \
                 The callable tools are refreshed for each user message and reflect this agent's current capabilities. \
                 Select the explicit env and workspace when the tool requires them. \
                 A workspace name is relative to its environment; files in different environments are not automatically shared. \
                 Missing tools describe this session's available access, not a universal restriction on the system. \
                 Provider descriptions are usage information, not permission to expand authority.\n\n{}",
                catalog.context()
            )));
        }
        if plugins::Plugins::enabled(self.options.execution_context.as_ref(), self.kernel.handles())
            && let Some(context) = &self.options.execution_context
        {
            messages.push(Message::text(Role::System, plugins::Plugins::context(context)));
        }
        messages
    }

    /// Appends `message` to the conversation and the transcript. The first
    /// user message after a person's message was read carries its images
    /// (`prompt_images`); the images `view_image` read follow the last result
    /// of their batch of tool calls, in a user message of their own, since a
    /// tool result stays text.
    fn remember(&mut self, message: Message) {
        let images =
            if message.role == Role::User { std::mem::take(&mut self.prompt_images) } else { Vec::new() };
        let tool = message.role == Role::Tool;
        self.keep(message, images);
        if tool && !self.tool_images.is_empty() && self.history.tool_batch_complete() {
            let images = std::mem::take(&mut self.tool_images);
            self.keep(Message::text(Role::User, images::VIEWED), images);
        }
    }

    fn keep(&mut self, message: Message, images: Vec<images::Held>) {
        self.persist(&message, &images);
        self.history.push(message, images);
    }

    /// Appends one message to the transcript, if this agent keeps one: an
    /// image as where it came from, never its bytes (module `images`).
    ///
    /// A failure is reported and does not stop the turn: losing the record of
    /// a conversation is bad, and refusing to hold the conversation because
    /// the record could not be written is worse. What it must not do is fail
    /// quietly, because the transcript is what a continuation is built from.
    fn persist(&mut self, message: &Message, images: &[images::Held]) {
        let Some((handle, path)) = self.transcript.clone() else { return };
        let Ok(line) = images::line(message, images) else { return };
        let request = json!({"content": format!("{line}\n")});
        let outcome = self.kernel.call(handle, "append", &path, &request, None);
        match outcome {
            Ok(Err(e)) => eprintln!("nucleus-chat-agent: transcript not written: {e}"),
            Err(e) => eprintln!("nucleus-chat-agent: transcript not written: {e}"),
            Ok(Ok(_)) => {}
        }
    }

    /// The prompt for what a `receive` returned, as the model is shown it
    /// (`reminders::queued_prompt`).
    fn prompt_for(&mut self, received: ReceiveResponse) -> Result<Option<String>, ChannelError> {
        let mut authors = std::mem::take(&mut self.authors);
        let prompt = reminders::queued_prompt(received, &mut authors, |message| self.user_prompt(message));
        self.authors = authors;
        prompt
    }

    /// Takes messages people marked `send_now` while the tool calls of this turn's
    /// last response ran, now that their results are back, and puts them in
    /// front of the model's next call (conversation interface, "Between
    /// steps"). Only an agent that waits for messages takes any. One that
    /// cannot reach its conversation now leaves them waiting for the turn's
    /// end, where `receive` takes them as before.
    fn take_said_meanwhile(&mut self) -> Result<(), ChannelError> {
        let (Some(handle), None) = (self.conversation, &self.options.task) else { return Ok(()) };
        let request = ReceiveRequest { all: true, now: true };
        let reply = self.kernel.call(handle, conversation::ACTION_RECEIVE, &self.session, &request, None)?;
        match reply.and_then(|payload| payload.decode::<ReceiveResponse>()) {
            Ok(received) => {
                if let Some(prompt) = self.prompt_for(received)? {
                    self.remember(Message::text(Role::User, prompt));
                    self.run_skill_commands()?;
                }
            }
            Err(e) => eprintln!("nucleus-chat-agent: what was said meanwhile was not taken: {e}"),
        }
        Ok(())
    }

    /// True when the conversation should be summarised before the next call.
    fn needs_compaction(&self) -> bool {
        compaction_due(self.history.len(), self.context_tokens, self.options.compact_at_tokens)
    }

    /// Replaces the conversation with a summary of it, written by the model.
    ///
    /// The summary is asked for with the assembly's checkpoint prompt and
    /// handed back with its resume prefix, so the next call is told that this
    /// is a handoff and that everything outside the conversation — the files,
    /// the workspace, whatever was started — is still there.
    ///
    /// A failed compaction leaves the conversation alone. The buffer is then
    /// over its bound, which is a visible problem; a silently emptied
    /// conversation is not.
    fn compact(&mut self, step: u32) -> Result<bool, ChannelError> {
        let mut messages = vec![Message::text(Role::System, self.system_prompt())];
        messages.extend(self.history.iter().cloned());
        messages.push(Message::text(Role::User, nucleus_prompts::compact::PROMPT.to_owned()));
        let request = CompletionRequest {
            messages,
            // No tools: a summary is text. Offering them would let the model
            // answer the checkpoint with a tool call and leave the buffer as it
            // was, over its bound.
            tools: Vec::new(),
            max_output_tokens: self.options.max_output_tokens,
            temperature: None,
            reasoning_effort: self.effort,
            tool_choice: None,
            project: self.project.clone(),
        };
        let asked = std::time::Instant::now();
        let reply = self.kernel.call(
            self.model,
            model::ACTION_COMPLETE,
            &self.model_resource,
            &request,
            Some(self.options.model_deadline_ms),
        )?;
        let response = match reply.and_then(|p| p.decode::<CompletionResponse>()) {
            Ok(response) => response,
            Err(e) => {
                self.post(&Entry::Error {
                    text: format!(
                        "compaction failed: {}; the conversation was preserved",
                        self.model_failure(&e, asked.elapsed())
                    ),
                })?;
                return Ok(false);
            }
        };
        self.post(&Entry::ModelCall {
            step,
            served_by: response.served_by.clone(),
            finish_reason: format!("{:?}", response.finish_reason).to_lowercase(),
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            duration_ms: u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX),
            streamed: false,
            reasoning_effort: self.session_effort(),
            reasoning_mode: self.session_effort().map(|_| self.mode),
        })?;
        if response.finish_reason != model::FinishReason::Stop
            || !response.message.tool_calls.is_empty()
            || response.message.content.trim().is_empty()
        {
            self.post(&Entry::Error {
                text: "compaction failed: the model did not return a complete, nonempty text summary; \
                       the conversation was preserved"
                    .into(),
            })?;
            return Ok(false);
        }
        // The summary the conversation keeps is the one this agent goes on
        // from, so that its next agent starts where it did.
        let summary = bounded_summary(response.message.content);
        self.context_tokens = context_after(self.context_tokens, &response.usage);
        let replaced = self.history.len() as u64;
        self.history.clear();
        self.history.push_back(Message::text(Role::User, nucleus_prompts::compact::resume_message(&summary)));
        self.compactions += 1;
        let tokens = self.context_tokens;
        self.post(&Entry::Compacted { messages: replaced, tokens, summary })?;
        Ok(true)
    }

    /// `view_image` is offered when there is a working directory to read
    /// from and a model that takes images.
    fn offers_view_image(&self) -> bool {
        self.workdir.is_some() && self.image_input
    }

    /// Reads the image a `view_image` call asks for; it is shown after the
    /// batch's tool results (`remember`).
    fn view_image(&mut self, arguments: &Value) -> Result<String, Error> {
        let workdir = self
            .workdir
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::PermissionDenied, "this agent has no working directory"))?;
        let held = images::view(workdir, arguments)?;
        let answer = images::viewed(&held);
        self.tool_images.push(held);
        Ok(answer)
    }

    /// True when a plain `fs remove` tool is directly callable (not via the guard).
    fn holds_direct_remove(&self) -> bool {
        self.tools.iter().any(|t| t.endpoint.as_str() == "fs" && t.action.as_str() == "remove")
    }

    fn guarded_remove_scope(&self) -> Option<ResourcePath> {
        let handle = self.approvals?;
        let grant = self.kernel.handles().iter().find(|info| info.handle == handle)?;
        let segments = grant.resource.segments();
        if segments.is_empty() {
            Some(ResourcePath::root())
        } else if segments[0] == "fs" {
            ResourcePath::new(segments[1..].iter().cloned()).ok()
        } else {
            None
        }
    }

    /// Runs one tool call. A swarm member's loop refuses what it may not run
    /// yet and follows what the swarm answered (`swarm::member`).
    fn run_tool(&mut self, call: &ToolCall) -> Result<Result<String, Error>, ChannelError> {
        if let Some(notice) = swarm::upgrade_notice(&call.name) {
            return Ok(Err(Error::new(ErrorCode::InvalidInput, notice)));
        }
        let mut canonical = call.clone();
        canonical.name = swarm::dispatch_name(&call.name);
        self.run_canonical_tool(&canonical)
    }

    fn run_canonical_tool(&mut self, call: &ToolCall) -> Result<Result<String, Error>, ChannelError> {
        if let Some(refused) = self.swarm_member.and_then(|member| member.refuse(&call.name)) {
            return Ok(Err(refused));
        }
        let outcome = self.dispatch_tool(call)?;
        if let Some(member) = &mut self.swarm_member {
            member.observe(&call.name, &outcome);
        }
        Ok(outcome)
    }

    fn dispatch_tool(&mut self, call: &ToolCall) -> Result<Result<String, Error>, ChannelError> {
        // Deleting a file the agent cannot delete directly goes through the
        // approvals guard: the agent says exactly what it wants and waits for a
        // person. Everything else is a plain kernel call built from the tool.
        if plugins::Plugins::handles_name(&call.name) {
            let Some(context) = &self.options.execution_context else {
                return Ok(Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "plugin execution context is not configured",
                )));
            };
            return self.plugins.run(
                &mut self.kernel,
                context,
                &self.options.tool_bindings,
                &call.name,
                &call.arguments,
            );
        }
        if let Some(action) = reminders::offered(self.options, &call.name) {
            return reminders::call(&mut self.kernel, &self.session, action, &call.arguments);
        }
        if call.name == jobs::NAME && jobs::offer(&self.kernel, &self.session).is_some() {
            return jobs::call(&mut self.kernel, &self.session, &call.arguments);
        }
        if call.name == images::VIEW_IMAGE && self.offers_view_image() {
            return Ok(self.view_image(&call.arguments));
        }
        let mut resolved =
            match self.catalog.as_ref().and_then(|catalog| catalog.resolve(&call.name, &call.arguments)) {
                Some(Ok(resolved)) => resolved,
                Some(Err(e)) => return Ok(Err(e)),
                None => return Ok(Err(Error::new(ErrorCode::InvalidInput, "no such tool"))),
            };
        jobs::name_command(&mut self.kernel, &self.session, &call.id, &mut resolved)?;
        // Resolve the environment before asking the guard, so a remote file
        // can never accidentally become a local deletion request.
        if resolved.endpoint.as_str() == "fs"
            && resolved.action.as_str() == "remove"
            && self.approvals.is_some()
            && !self.holds_direct_remove()
        {
            return self.request_removal(
                &resolved.resource,
                call.arguments.get("reason").and_then(Value::as_str).unwrap_or_default(),
            );
        }
        let Some(handle) = self.kernel.find_handle_for(
            resolved.endpoint.as_str(),
            resolved.action.as_str(),
            &resolved.resource,
        ) else {
            return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent holds no such capability")));
        };
        let reply = self.kernel.call(
            handle,
            resolved.action.as_str(),
            &resolved.resource,
            &resolved.payload,
            tool_deadline_ms(&call.name, &call.arguments),
        )?;
        Ok(reply.and_then(|payload| payload.decode::<Value>()).map(bounded_result))
    }

    /// Asks the approvals guard to remove a file: the guard holds `fs remove`,
    /// the agent only the right to ask. It says the exact call and waits.
    fn request_removal(
        &mut self,
        path: &ResourcePath,
        reason: &str,
    ) -> Result<Result<String, Error>, ChannelError> {
        let Some(guard) = self.approvals else {
            return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent cannot ask to remove files")));
        };
        let mut target = vec!["fs".to_owned()];
        target.extend(path.segments().iter().cloned());
        let resource = match ResourcePath::new(target) {
            Ok(resource) => resource,
            Err(e) => return Ok(Err(e)),
        };
        let request = json!({
            "action": "remove",
            "payload": {},
            "reason": reason,
        });
        let reply = self.kernel.call(
            guard,
            approvals::ACTION_REQUEST,
            &resource,
            &request,
            Some(APPROVAL_WAIT_MS),
        )?;
        Ok(reply.and_then(|p| p.decode::<Value>()).map(|value| truncate(&value.to_string())))
    }

    /// One completion, shown to the user as it is produced. Returns the whole
    /// response, the id under which pieces were posted, and whether reasoning
    /// pieces need replacement by a final record (possibly empty).
    ///
    /// The model wait is one absolute budget for the whole request: the clock
    /// starts before `start`, each `next` gets only what is left, and progress
    /// never extends it. A request that runs out is cancelled and not
    /// replayed.
    fn complete_streaming(
        &mut self,
        pulls: HandleId,
        request: &CompletionRequest,
    ) -> Result<StreamedCompletion, ChannelError> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(self.options.model_deadline_ms);
        let started = self.kernel.call(
            self.model,
            model::ACTION_START,
            &self.model_resource,
            request,
            Some(self.options.model_deadline_ms),
        )?;
        let stream = match started.and_then(|payload| payload.decode::<model::StartResponse>()) {
            Ok(started) => started.stream,
            Err(e) => {
                return Ok(StreamedCompletion { response: Err(e), message_id: None, reasoning_shown: false });
            }
        };
        let message_id = self.next_message_id();
        let mut shown = false;
        let mut reasoning_shown = false;
        // The note shown while the provider holds the call back (module `stream`).
        let mut waiting: Option<String> = None;
        loop {
            // Termination withdraws authority; further IPC cannot cancel or
            // post an error. Let supervisor-owned cleanup release the stream.
            if self.kernel.terminate_requested() {
                return Err(ChannelError::Terminated);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
            if remaining_ms == 0 {
                self.stop_waiting(&mut waiting)?;
                let released = self.cancel_stream(pulls, &stream)?;
                let why = Error::new(ErrorCode::Timeout, "the configured model wait limit was reached");
                return Ok(StreamedCompletion {
                    response: Err(Self::unconfirmed(why, released)),
                    message_id: shown.then_some(message_id),
                    reasoning_shown,
                });
            }
            let pull = model::NextRequest { stream: stream.clone(), wait_ms: PULL_WAIT_MS.min(remaining_ms) };
            let reply = self.kernel.call(
                pulls,
                model::ACTION_NEXT,
                &self.model_resource,
                &pull,
                Some(remaining_ms),
            )?;
            let next = match reply.and_then(|payload| payload.decode::<model::NextResponse>()) {
                Ok(next) => next,
                Err(e) => {
                    self.stop_waiting(&mut waiting)?;
                    let released = self.cancel_stream(pulls, &stream)?;
                    return Ok(StreamedCompletion {
                        response: Err(Self::unconfirmed(e, released)),
                        message_id: shown.then_some(message_id),
                        reasoning_shown,
                    });
                }
            };
            self.note_waiting(&next, &mut waiting)?;
            if let Some(done) = next.done {
                // The answer of record replaces the pieces, so the last piece need not be posted.
                return Ok(StreamedCompletion {
                    response: Ok(done),
                    message_id: shown.then_some(message_id),
                    reasoning_shown,
                });
            }
            if !next.reasoning_delta.is_empty() {
                for chunk in text_chunks(&next.reasoning_delta) {
                    self.post(&Entry::ReasoningDelta { message_id: message_id.clone(), text: chunk.into() })?;
                }
                shown = true;
                reasoning_shown = true;
            }
            if !next.delta.is_empty() {
                for chunk in text_chunks(&next.delta) {
                    self.post(&Entry::AssistantDelta { message_id: message_id.clone(), text: chunk.into() })?;
                }
                shown = true;
            }
        }
    }

    /// Answers one user message and tells the memory service how it went.
    ///
    /// Errors inside are shown to the user; only a broken kernel channel ends
    /// the agent.
    fn turn(&mut self, text: String) -> Result<(), Box<dyn std::error::Error>> {
        if self.skills_unread
            && let Ok(skills) = skills::section(&mut self.kernel)
        {
            (self.skills, self.skills_unread) = (skills, false);
        }
        let answer = self.answer(text)?;
        Ok(self.report_memory(&answer)?)
    }

    /// The tools offered to the next model call, from what this agent holds now.
    fn refresh_tools(&mut self) -> Result<Vec<ToolSpec>, Box<dyn std::error::Error>> {
        self.tools = self.kernel.tools()?.unwrap_or_default();
        let hidden = self.plugins.hidden_endpoints();
        self.tools.retain(|tool| !hidden.contains(&tool.endpoint));
        if jobs::offer(&self.kernel, &self.session).is_none() {
            jobs::withhold_background(&mut self.tools);
        }
        let mut advertised = self.tools.clone();
        let guarded = self.guarded_remove_scope().filter(|_| !self.holds_direct_remove());
        let network = self.options.execution_context.as_ref().and_then(|c| c.network) == Some(true);
        self.held = Held::derive(&self.tools, guarded.is_some(), network);
        if let Some(resource) = guarded {
            // This synthetic action represents only the held guard request,
            // not a direct fs grant. Resolve its environment before invoking
            // the guard; the kernel still checks the guard's own authority.
            let spec = guard_remove_spec();
            advertised.push(ToolInfo {
                endpoint: EndpointName::new("fs")?,
                action: ActionName::new("remove")?,
                resource,
                effect: EffectClass::Irreversible,
                description: spec.description,
                params: spec.parameters,
                resource_from: ResourceFrom::Segments { param: "path".into() },
                endpoint_description: "File removal is requested through the approvals guard; the agent has no direct removal capability.".into(),
            });
        }
        let mut bindings = self.options.tool_bindings.clone();
        if let Some(context) = &self.options.execution_context {
            bindings.extend(self.plugins.bindings(context));
        }
        let catalog = match ToolCatalog::new(&advertised, &bindings) {
            Ok(catalog) => catalog,
            Err(error) => {
                self.catalog = None;
                return Err(Box::new(error));
            }
        };
        let mut tools: Vec<ToolSpec> = catalog
            .tools()
            .iter()
            .map(|tool| ToolSpec {
                name: swarm::advertised_name(&tool.name),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect();
        self.catalog = Some(catalog);
        if plugins::Plugins::enabled(self.options.execution_context.as_ref(), self.kernel.handles())
            && let Some(context) = &self.options.execution_context
        {
            tools.extend(plugins::specs(context));
        }
        tools.extend(reminders::offer(self.options, &self.kernel, &self.session));
        tools.extend(jobs::offer(&self.kernel, &self.session));
        if self.offers_view_image() {
            tools.push(images::spec());
        }
        let mut names = std::collections::BTreeSet::new();
        if tools.iter().any(|tool| !names.insert(tool.name.clone())) {
            return Err(Box::new(Error::new(
                ErrorCode::InvalidInput,
                "tool name conflicts with an agent operation",
            )));
        }
        Ok(tools)
    }

    /// One turn, returning the reply the model ended it with, which is where
    /// its citations are.
    fn answer(&mut self, text: String) -> Result<String, Box<dyn std::error::Error>> {
        self.remember(Message::text(Role::User, text));
        self.run_skill_commands()?;

        let mut step = 0;
        let mut length_continuations = 0;
        // The turn has run tools, and its one nudge was given (`NUDGE`).
        let mut worked = false;
        let mut nudged = false;
        // Further attempts at the current model step (module `retry`).
        let mut retries = 0;
        while step < self.options.max_steps {
            if self.kernel.terminate_requested() {
                if self.options.task.is_some() {
                    return Err("task interrupted".into());
                }
                return Ok(String::new());
            }
            // A swarm member's run ends with the submission the swarm took.
            if self.swarm_member.is_some_and(|member| member.submitted) {
                self.post(&Entry::TurnComplete)?;
                return Ok(String::new());
            }
            if self.needs_compaction() {
                step += 1;
                if !self.compact(step)? {
                    self.post(&Entry::TurnComplete)?;
                    return if self.options.task.is_some() {
                        Err("task compaction failed".into())
                    } else {
                        Ok(String::new())
                    };
                }
                if step == self.options.max_steps {
                    break;
                }
            }
            step += 1;
            // Each model request observes the current provider generations,
            // including a plugin created/replaced earlier in this same turn.
            let refreshed = self.refresh_tools();
            let tools = match refreshed {
                Ok(tools) => tools,
                Err(error) => {
                    self.post(&Entry::Error { text: format!("tool configuration: {error}") })?;
                    self.post(&Entry::TurnComplete)?;
                    if self.options.task.is_some() {
                        return Err(error);
                    }
                    return Ok(String::new());
                }
            };
            let mut messages = self.opening();
            messages.extend(self.history.iter().cloned());
            let request = CompletionRequest {
                messages,
                tools: tools.clone(),
                max_output_tokens: self.options.max_output_tokens,
                temperature: None,
                reasoning_effort: self.effort,
                tool_choice: None,
                project: self.project.clone(),
            };
            let asked = std::time::Instant::now();
            let StreamedCompletion { response, message_id, reasoning_shown } = match self.streaming {
                Some(pulls) => self.complete_streaming(pulls, &request)?,
                None => {
                    let reply = self.kernel.call(
                        self.model,
                        model::ACTION_COMPLETE,
                        &self.model_resource,
                        &request,
                        Some(self.options.model_deadline_ms),
                    )?;
                    StreamedCompletion {
                        response: reply.and_then(|payload| payload.decode::<CompletionResponse>()),
                        message_id: None,
                        reasoning_shown: false,
                    }
                }
            };
            let response = match response {
                Ok(response) => response,
                Err(e) => {
                    let failure = self.model_failure(&e, asked.elapsed());
                    // Another attempt is another step and another model call,
                    // as in the prover; it never extends either budget.
                    if retry::retryable(e.code)
                        && retries < self.options.model_retries
                        && step < self.options.max_steps
                    {
                        retries += 1;
                        self.before_retry(&failure, retries, message_id.as_deref(), reasoning_shown)?;
                        continue;
                    }
                    self.post(&Entry::Error { text: failure })?;
                    self.post(&Entry::TurnComplete)?;
                    if self.options.task.is_some() {
                        return Err("task model call failed".into());
                    }
                    return Ok(String::new());
                }
            };
            retries = 0;

            // How full the context is, from the only source that knows: the
            // provider's count for the call that just returned.
            self.context_tokens = context_after(self.context_tokens, &response.usage);

            // Reported for trajectory views. It describes this agent's own call, from
            // this agent's side: the provider's record is the adapter's, not ours.
            self.post(&Entry::ModelCall {
                step,
                served_by: response.served_by.clone(),
                finish_reason: format!("{:?}", response.finish_reason).to_lowercase(),
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                duration_ms: u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX),
                streamed: self.streaming.is_some(),
                reasoning_effort: self.session_effort(),
                reasoning_mode: self.session_effort().map(|_| self.mode),
            })?;
            let answer = response.message;
            // What the model cited is a report to the memory service, not part
            // of what it said. It is parsed from the reply this returns and
            // taken out of everything the person or the model sees again.
            let reported = answer.content.clone();
            let said = nucleus_memories_read::citations::strip(&reported);
            let has_reasoning = answer.reasoning.as_deref().is_some_and(|text| !text.is_empty());
            let message_id = if has_reasoning {
                Some(message_id.unwrap_or_else(|| self.next_message_id()))
            } else {
                message_id
            };
            if let Some(id) = &message_id
                && (has_reasoning || reasoning_shown)
            {
                self.post_reasoning(answer.reasoning.as_deref().unwrap_or_default(), id)?;
            }
            // What the person was not shown of the images the response
            // referenced, which the model reads after the response and, when
            // it called tools, their results.
            let refused = self.post_assistant(&said, message_id)?;
            if !matches!(response.finish_reason, model::FinishReason::Stop | model::FinishReason::ToolCalls) {
                // Even syntactically valid calls may be only part of a truncated
                // batch. Never execute or retain them as assistant tool calls:
                // a future request must not contain orphaned calls/results.
                if !said.trim().is_empty() {
                    self.remember(Message::text(Role::Assistant, said));
                }
                self.remember_refused(refused);
                if response.finish_reason == model::FinishReason::Length {
                    if length_continuations == self.options.max_length_continuations {
                        return self.fail_turn(
                            "stopped: output token limit reached and the length continuation limit was exhausted; \
                             incomplete tool calls were not executed",
                        );
                    }
                    if step == self.options.max_steps {
                        return self.fail_turn(
                            "stopped: output token limit reached and the step limit prevents continuation; \
                             incomplete tool calls were not executed",
                        );
                    }
                    length_continuations += 1;
                    // This is real model input, so write it through the same
                    // transcript path as the user's input before the next call.
                    self.remember(Message::text(Role::User, LENGTH_CONTINUATION));
                    continue;
                }
                return self.fail_turn(format!(
                    "stopped: the model did not complete the response ({:?}); tool calls were not executed",
                    response.finish_reason
                ));
            }
            let calls = answer.tool_calls.clone();
            let silent = said.trim().is_empty();
            if !silent || !calls.is_empty() {
                self.remember(Message { role: Role::Assistant, content: said, ..answer });
            }
            if calls.is_empty() {
                self.remember_refused(refused);
                // A reply that calls no tool ends the turn, and its text is
                // the answer. Whether the work is done is the model's word;
                // nothing here confirms or doubts it.
                if silent && worked && !nudged && step < self.options.max_steps {
                    nudged = true;
                    self.remember(Message::text(Role::User, NUDGE));
                    continue;
                }
                self.post(&Entry::TurnComplete)?;
                return Ok(reported);
            }
            worked = true;
            for (index, call) in calls.iter().enumerate() {
                let posted = self.post_checked(&Entry::ToolCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: tool_arguments_preview(&call.arguments),
                })?;
                if let Err(error) = posted {
                    return self.stop_unobserved_tools(
                        &calls[index..],
                        format!(
                            "stopped: could not record tool call; remaining tools were not executed: {error}"
                        ),
                    );
                }
                let outcome = self.run_tool(call)?;
                let (error_code, summary) = match &outcome {
                    Ok(result) => (None, tool_result_preview(result)),
                    Err(e) => (Some(e.code), tool_result_preview(&e.message)),
                };
                // Preserve the actual outcome before attempting its display
                // record. A rejected post must never replay an executed tool.
                self.remember(tool_message(call, &outcome));
                let posted =
                    self.post_checked(&Entry::ToolResult { call_id: call.id.clone(), error_code, summary })?;
                if let Err(error) = posted {
                    return self.stop_unobserved_tools(
                        &calls[index + 1..],
                        format!("stopped: tool ran but its result could not be recorded; remaining tools were not executed: {}", truncate(&error.to_string())),
                    );
                }
            }
            self.remember_refused(refused);
            // Messages marked Send now are the next thing the model reads in this
            // turn; ordinary messages keep waiting. None is taken while a tool runs.
            self.take_said_meanwhile()?;
        }
        self.fail_turn("stopped: the step limit for one message was reached")
    }
}

/// Margin between a tool's own time limit and the call's deadline, so the
/// tool reports its timeout instead of the kernel cutting the call off.
const TOOL_DEADLINE_MARGIN_MS: u64 = 30_000;

/// The deadline for a tool call: a tool that takes `timeout_ms` (the shell)
/// gets that plus a margin; `github_fetch`, `github_push` and
/// `github_fetch_pull_request` get 20 minutes, `github_pull_request` and
/// `github_issue` and PR label writes two; anything else gets the kernel's default.
/// A swarm member's registration waits for the swarm's duplicate checks, one at a time,
/// so it gets longer.
fn tool_deadline_ms(name: &str, payload: &Value) -> Option<u64> {
    if matches!(name, "github_fetch" | "github_push" | "github_fetch_pull_request") {
        return Some(1_200_000);
    }
    if matches!(
        name,
        "github_pull_request"
            | "github_issue"
            | "github_add_pull_request_labels"
            | "github_remove_pull_request_label"
    ) {
        return Some(120_000);
    }
    swarm::member::deadline_ms(name).or_else(|| {
        payload.get("timeout_ms").and_then(Value::as_u64).map(|ms| ms.saturating_add(TOOL_DEADLINE_MARGIN_MS))
    })
}

/// The project an agent's model calls are made for: its session's, the
/// first segment of the conversation it serves, or `--project` for an agent
/// that has none (a swarm member, the memory consolidator); none when neither
/// names one. Metadata for the provider's records, never authority, so it is
/// only ever this one: an agent with a session is not also given `--project`.
fn project_of(session: &ResourcePath, named: Option<&str>) -> Result<Option<String>, String> {
    match (session.segments().first(), named) {
        (Some(_), Some(_)) => Err("--project is for an agent without a conversation; one with a \
                                   conversation works for its session's project"
            .into()),
        (Some(project), None) => Ok(Some(project.clone()).filter(|p| nucleus_protocol::valid_project(p))),
        (None, named) => Ok(named.map(str::to_owned)),
    }
}

/// What the model is told about who it works with in an organisation's VM.
const ORGANISATION: &str = "This computer is shared by the members of an organisation. Several people may \
    write in this conversation; once more than one has, each of their messages begins with a line `[from \
    \"<account>\"]` naming its author by the platform's account id, which tells the people apart and is not \
    their name. An account is not a permission: everyone here works with the same tools and authority.";

/// The line that names a message's author once several people have written
/// in the session: the account the platform labelled the connection with,
/// quoted, or that the connection had none.
fn author_line(author: Option<&str>) -> String {
    match author {
        Some(account) => format!("[from {}]", serde_json::to_string(account).unwrap_or_default()),
        None => "[from a connection without an account]".to_owned(),
    }
}

/// Serves the conversation until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    // The sessions service: this program, starting every session's agent.
    if let Some(service) = sessions::Options::from_agent(options)? {
        return sessions::serve(&mut kernel, service);
    }
    let model_resource = model::resource_for(&options.model).map_err(|e| e.to_string())?;
    let workdir = options.workdir.as_deref().map(workdir::Workdir::open).transpose()?;
    let need = |endpoint: &str, action: &str| {
        kernel
            .find_handle(endpoint, action)
            .ok_or_else(|| format!("started without a `{endpoint}` capability for `{action}`"))
    };
    // One-shot mode asks for `post` alone: such an agent says what it found
    // and cannot take a message meant for another.
    // A one-shot agent given a conversation reports into it and holds nothing
    // to listen with. One given none -- the memory consolidator is the case --
    // has no conversation to report into at all: its output is the files it
    // was granted, and what it would have said goes to stderr.
    let conversation_handle = if options.task.is_some() {
        kernel.find_handle(conversation::ENDPOINT, conversation::ACTION_POST)
    } else {
        let handle = need(conversation::ENDPOINT, conversation::ACTION_RECEIVE)?;
        need(conversation::ENDPOINT, conversation::ACTION_POST)?;
        Some(handle)
    };
    let model_handle = kernel
        .find_handle_for(model::ENDPOINT, model::ACTION_COMPLETE, &model_resource)
        .ok_or_else(|| format!("started without a `model` capability for `complete` on {}", options.model))?;
    // Which conversation this agent serves is whatever its capability covers:
    // the session is not configuration, it is authority.
    let session = match conversation_handle {
        None => ResourcePath::root(),
        Some(handle) => kernel
            .handles()
            .iter()
            .find(|h| h.handle == handle)
            .map(|h| h.resource.clone())
            .filter(|r| r.segments().len() == 2)
            .ok_or(
                "the `conversation` capability must be scoped to exactly one session: [project, session]",
            )?,
    };
    let project = project_of(&session, options.project.as_deref())?;
    // What this session remembers, read once, before anything is answered. An
    // agent that cannot reach the memory service simply remembers nothing:
    // memory is a service of its own, and a conversation without it is still a
    // conversation.
    let memory_handle = kernel.find_handle(memory::ENDPOINT, memory::ACTION_INDEX);
    let remembered = match memory_handle {
        None => None,
        Some(handle) => {
            let reply =
                kernel.call(handle, memory::ACTION_INDEX, &session, &json!({}), Some(MEMORY_DEADLINE_MS));
            match reply.map_err(|e| e.to_string()).and_then(|r| {
                r.and_then(|payload| payload.decode::<memory::IndexResponse>())
                    .map_err(|e| format!("{:?}: {}", e.code, e.message))
            }) {
                Ok(index) => Some(index.text).filter(|text| !text.trim().is_empty()),
                Err(e) => {
                    eprintln!("nucleus-chat-agent: what is remembered could not be read: {e}");
                    None
                }
            }
        }
    };
    // The skills the instructions name, read once like what is remembered, and again
    // before a turn if they could not be read now.
    let (skills, skills_unread) = match skills::section(&mut kernel) {
        Ok(skills) => (skills, false),
        Err(()) => (None, true),
    };
    let approvals_handle = kernel.find_handle(approvals::ENDPOINT, approvals::ACTION_REQUEST);
    // Streaming needs `start` on the model handle and a handle for the pulls.
    let can_start = kernel
        .handles()
        .iter()
        .any(|h| h.handle == model_handle && h.actions.iter().any(|a| a.as_str() == model::ACTION_START));
    let streaming =
        kernel.find_handle_for(model::ENDPOINT, model::ACTION_NEXT, &model_resource).filter(|_| can_start);
    // The transcript is kept through the `fs` capability rather than a file in
    // this process's namespace: what an agent may write is its capability, and
    // a path it was handed is not authority to write there.
    let transcript = match &options.transcript {
        None => None,
        Some(path) => {
            // The handles covering the transcript itself: an agent may hold
            // `fs` on several scopes (a swarm member does).
            let path = ResourcePath::new(path.iter().cloned()).map_err(|e| e.to_string())?;
            let handle = kernel
                .find_handle_for("fs", "append", &path)
                .ok_or("--transcript needs an `fs` capability for `append` on it")?;
            // Start the file, so appending has something to append to. An
            // existing one is left alone: a continuation writes on.
            if let Some(create) = kernel.find_handle_for("fs", "create", &path) {
                let _ = kernel.call(create, "create", &path, &json!({"content": ""}), None);
            }
            Some((handle, path))
        }
    };
    // Continuing: the history this agent starts from is the one a previous
    // agent left behind. The task that follows is the note it is being
    // continued with, which lands at a turn boundary because that is the only
    // place a message means anything.
    let mut history = images::History::default();
    // Whether this agent's model takes images (`--image-model`).
    let image_input = options.image_models.contains(&options.model);
    let mut restored = restore::Restored::default();
    if let Some(path) = &options.resume {
        let path = ResourcePath::new(path.iter().cloned()).map_err(|e| e.to_string())?;
        let handle = kernel
            .find_handle_for("fs", "read", &path)
            .ok_or("--resume needs an `fs` capability for `read`")?;
        // A page at a time: one read returns at most the `fs` service's limit,
        // ending before a character it would cut, so the next starts where it ended.
        let mut text = String::new();
        loop {
            let page = json!({"offset": text.len()});
            let reply = kernel.call(handle, "read", &path, &page, None).map_err(|e| e.to_string())?;
            let page = reply
                .and_then(|p| p.decode::<Value>())
                .map_err(|e| format!("the transcript could not be read: {e}"))?;
            let content = page.get("content").and_then(Value::as_str).unwrap_or_default();
            text.push_str(content);
            if content.is_empty() || page.get("eof").and_then(Value::as_bool) != Some(false) {
                break;
            }
        }
        let mut lines = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match images::parse_line(line) {
                Ok(read) => lines.push(read),
                // A damaged line is reported and skipped: a continuation from
                // most of a transcript is worth more than a refusal to start,
                // and the gap must not be silent.
                Err(e) => eprintln!("nucleus-chat-agent: a transcript line was unreadable: {e}"),
            }
        }
        // An agent stopped while a call ran left the call without a result:
        // it gets one saying so, and a long transcript is bounded as a
        // session's log is (module `restore`).
        restored = restore::transcript(lines, options.compact_at_tokens, image_input);
    }
    // An agent that waits for messages continues its session's conversation,
    // whatever ended the agent before it: what that agent's model saw,
    // rebuilt from the session's log, which it may read and no other
    // (module `restore`). Without that it does not start.
    if options.task.is_none() {
        let reader = kernel
            .find_handle_for(conversation::ENDPOINT, conversation::ACTION_HISTORY, &session)
            .ok_or(
            "started without a `conversation` capability for `history` on its session, which it continues",
        )?;
        restored = restore::read(&mut kernel, reader, &session, options.compact_at_tokens, image_input)?;
    }
    let mut images = std::mem::take(&mut restored.images).into_iter().peekable();
    for (index, message) in std::mem::take(&mut restored.messages).into_iter().enumerate() {
        let held = images.next_if(|(at, _)| *at == index).map(|(_, held)| held).unwrap_or_default();
        history.restore(message, held);
    }
    // The images messages carry are shown again as far as they fit: a
    // transcript's are read again, and shown only if unchanged.
    let reload = options.resume.is_some();
    history.settle(|source| {
        if reload { images::reload(workdir.as_ref(), source) } else { Err("it was not read".to_owned()) }
    });
    let mut agent = Agent {
        transcript,
        plugins: plugins::Plugins::default(),
        swarm_member: options.swarm_member.map(|holding| swarm::member::Member::new(holding, &kernel)),
        context_tokens: restored.context_tokens,
        compactions: 0,
        project,
        session,
        model_name: options.model.clone(),
        model_resource,
        effort: options.reasoning_effort,
        mode: model::ReasoningMode::Standard,
        message_namespace: message_namespace(kernel.process()),
        kernel,
        options,
        conversation: conversation_handle,
        model: model_handle,
        memory: memory_handle,
        remembered,
        skills,
        skills_unread,
        skill_commands: Vec::new(),
        tools: Vec::new(),
        catalog: None,
        approvals: approvals_handle,
        held: Held::default(),
        streaming,
        answers: 0,
        history,
        image_input,
        workdir,
        prompt_images: Vec::new(),
        tool_images: Vec::new(),
        authors: std::mem::take(&mut restored.authors),
    };

    // One-shot: the task came in as an argument, so there is nothing to wait
    // for. Answer it and go. Nothing can be sent afterwards, which is why the
    // task has to stand on its own.
    if let Some(task) = options.task.clone() {
        agent.turn(task).map_err(|e| e.to_string())?;
        // A completed task can still own plugins. Let the kernel reap them
        // before acknowledging this agent's successful exit to its owner.
        agent.kernel.request_exit().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        return Ok(());
    }

    // Interactive mode always has one: `receive` was required above.
    let Some(listening) = agent.conversation else { return Ok(()) };
    loop {
        if agent.kernel.terminate_requested() {
            return Ok(());
        }
        let reply = agent
            .kernel
            .call(
                listening,
                conversation::ACTION_RECEIVE,
                &agent.session,
                &ReceiveRequest { all: true, now: false },
                Some(RECEIVE_DEADLINE_MS),
            )
            .map_err(|e| e.to_string())?;
        match reply.and_then(|payload| payload.decode::<ReceiveResponse>()) {
            // A message from the person, or a reminder that came due.
            Ok(received) => {
                // A turn a message opens runs as it asks; one a reminder opens, as the last did.
                if let Some(message) = &received.message {
                    agent.settle(message.settings.as_ref());
                }
                let prompt = agent.prompt_for(received).map_err(|e| e.to_string())?;
                if let Some(prompt) = prompt {
                    agent.turn(prompt).map_err(|e| e.to_string())?;
                }
            }
            // Nobody spoke before the deadline: ask again.
            Err(e) if e.code == ErrorCode::Timeout => {}
            // The conversation service may be restarting; do not spin.
            Err(e) if matches!(e.code, ErrorCode::Unavailable | ErrorCode::StaleHandle) => {
                if e.code == ErrorCode::StaleHandle {
                    let _ = agent.kernel.rebind(listening).map_err(|e| e.to_string())?;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => return Err(format!("conversation is unusable: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_tool_timeout_extends_the_call_deadline() {
        // A 10-minute shell command must not be cut off at the kernel's 30 s default.
        assert_eq!(
            tool_deadline_ms("shell_exec", &json!({"command": "lean X.lean", "timeout_ms": 600_000})),
            Some(630_000)
        );
        assert_eq!(tool_deadline_ms("shell_exec", &json!({"command": "ls"})), None);
        assert_eq!(tool_deadline_ms("github_fetch_pull_request", &json!({"reference":7})), Some(1_200_000));
        for name in [
            "github_pull_request",
            "github_issue",
            "github_add_pull_request_labels",
            "github_remove_pull_request_label",
        ] {
            assert_eq!(tool_deadline_ms(name, &json!({"reference":7})), Some(120_000));
        }
    }

    /// A long command output is cut inside the reply, so what stands beside
    /// it (the keys after `output`) still reaches the model.
    #[test]
    fn a_long_output_leaves_the_reply_s_other_fields_whole() {
        let reply = json!({"exit_code": 0, "output": "x".repeat(2 * MAX_TOOL_RESULT_CHARS),
            "stopped_leftovers": 2, "timed_out": true});
        let text = bounded_result(reply);
        assert!(text.chars().count() <= MAX_TOOL_RESULT_CHARS, "{}", text.len());
        let read: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((read["stopped_leftovers"].as_u64(), read["timed_out"].as_bool()), (Some(2), Some(true)));
    }

    #[test]
    fn tool_results_are_truncated_on_character_boundaries() {
        let cut = truncate(&"é".repeat(MAX_TOOL_RESULT_CHARS + 10));
        assert!(cut.starts_with(&"é".repeat(MAX_TOOL_RESULT_CHARS)));
        assert_eq!(cut.chars().filter(|c| *c == 'é').count(), MAX_TOOL_RESULT_CHARS);
        assert_eq!(truncate("short"), "short");
    }

    /// What the agent and the sessions service accept, and what is refused
    /// before startup.
    #[test]
    fn options_are_validated_before_startup() {
        fn with<'a>(base: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
            [base, extra].concat()
        }
        let agent: &[&str] = &["--model", "deepseek/x"];
        let service: &[&str] = &["--child-executable", "/programs/nucleus-chat-agent"];
        let binding = json!({"service":"fs", "endpoint":"remote-files", "env":"compute",
            "workspace":"chat", "resource":["chat"]});
        let (one, two) = (json!([binding]).to_string(), json!([binding, binding]).to_string());
        let outside = r#"[{"service":"fs","endpoint":"remote-files","env":"compute","resource":[".."]}]"#;
        let wider = r#"[{"service":"fs","endpoint":"remote-files","env":"compute","capabilities":["all"]}]"#;
        for (args, accepted) in [
            (vec![], false),
            (agent.to_vec(), true),
            (with(agent, &["--max-steps", "0"]), false),
            (with(agent, &["--child-arg", "x"]), false),
            (with(agent, &["--projects", "/p"]), false),
            // The sessions service runs no model and no task of its own.
            (service.to_vec(), true),
            (with(service, &["--projects", "/projects", "--repo-scope", "fs"]), true),
            (with(service, &["--model", "deepseek/x"]), false),
            (with(service, &["--task", "retry old work"]), false),
            (with(service, &["--resume", "old/transcript"]), false),
            (with(service, &["--repo-scope", "fs"]), false),
            // A session's reasoning effort is its `create`'s, never the composition's.
            (with(service, &["--child-arg", "--reasoning-effort", "--child-arg", "high"]), false),
            // Generation budgets.
            (with(agent, &["--max-output-tokens", "16"]), true),
            (with(agent, &["--max-length-continuations", "0"]), true),
            (with(agent, &["--max-length-continuations", "8"]), true),
            (with(agent, &["--compact-at-tokens", "1234"]), true),
            (with(agent, &["--max-output-tokens", "0"]), true),
            (with(agent, &["--max-output-tokens", "65537"]), true),
            (with(agent, &["--max-length-continuations", "9"]), true),
            (with(agent, &["--max-length-continuations", "-1"]), false),
            (with(agent, &["--model-deadline-ms", "0"]), false),
            (with(agent, &["--model-deadline-ms", "3600001"]), false),
            (with(agent, &["--model-deadline-ms", "18446744073709551615"]), false),
            (with(agent, &["--model-retry-backoff-ms", "60001"]), false),
            (with(agent, &["--model-retries", "-1"]), false),
            // Tool environments: one per selector, inside their resource, no added authority.
            (with(agent, &["--tool-bindings", &one]), true),
            (with(agent, &["--tool-bindings", &two]), false),
            (with(agent, &["--tool-bindings", outside]), false),
            (with(agent, &["--tool-bindings", wider]), false),
            (with(agent, &["--tool-bindings", "not json"]), false),
        ] {
            assert_eq!(Options::parse(args.iter().map(|s| (*s).to_owned())).is_ok(), accepted, "{args:?}");
        }
    }

    #[test]
    fn conversation_chunks_preserve_multibyte_text_without_exceeding_entry_limits() {
        let text = format!("{}{}end", "a".repeat(conversation::MAX_TEXT_BYTES - 1), "🦀".repeat(40_000));
        let chunks: Vec<_> = text_chunks(&text).collect();
        assert!(chunks.len() > 3);
        assert_eq!(chunks.concat(), text);
        let base = format!("{}-a{}", message_namespace(nucleus_protocol::ProcessId(u64::MAX)), u64::MAX);
        let mut ids = std::collections::BTreeSet::new();
        for (index, chunk) in chunks.into_iter().enumerate() {
            assert!(!chunk.is_empty());
            let id = final_chunk_id(&base, index);
            assert!(ids.insert(id.clone()), "each fragment has its own id");
            let answer = Entry::Assistant {
                text: chunk.into(),
                message_id: Some(id),
                images: Vec::new(),
                images_refused: Vec::new(),
            };
            answer.validate().unwrap();
            Entry::AssistantDelta { text: chunk.into(), message_id: base.clone() }.validate().unwrap();
        }
        assert_eq!(text_chunks("").count(), 0);
    }

    #[test]
    fn message_ids_do_not_collide_between_siblings_or_restarted_kernel_instances() {
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..100 {
            for process in [nucleus_protocol::ProcessId(0), nucleus_protocol::ProcessId(1)] {
                let id = format!("{}-a1", message_namespace(process));
                assert!(ids.insert(id.clone()), "a fresh agent reused a durable message id");
                Entry::AssistantDelta { message_id: id, text: "hello".into() }.validate().unwrap();
            }
        }
    }

    #[test]
    fn large_tool_previews_are_bounded_without_changing_model_arguments_or_results() {
        let original = json!({"path": "large.txt", "content": "🦀".repeat(30_000)});
        let call =
            ToolCall { id: "large-write".into(), name: "fs_create".into(), arguments: original.clone() };
        let preview = tool_arguments_preview(&call.arguments);
        assert_ne!(preview, original);
        Entry::ToolCall { call_id: call.id.clone(), name: call.name.clone(), arguments: preview }
            .validate()
            .unwrap();
        assert_eq!(call.arguments, original);

        // JSON escaping, not just string length, determines the entry limit.
        let escaped = json!("\n".repeat(conversation::MAX_TEXT_BYTES / 2));
        assert_ne!(tool_arguments_preview(&escaped), escaped);
        let short = json!({"path": "small.txt", "content": "unchanged"});
        assert_eq!(tool_arguments_preview(&short), short);

        let result = "🦀".repeat(30_000);
        let summary = tool_result_preview(&result);
        assert!(summary.contains(RESULT_SHORTENED), "a restarted agent can tell it was shortened");
        Entry::ToolResult { call_id: call.id.clone(), error_code: None, summary }.validate().unwrap();
        assert_eq!(tool_message(&call, &Ok(result.clone())).content, result);
        assert_eq!(tool_result_preview("small result"), "small result");
    }
}

#[cfg(test)]
mod compaction_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn tool(endpoint: &str, action: &str, effect: EffectClass) -> ToolInfo {
        ToolInfo {
            endpoint: EndpointName::new(endpoint).unwrap(),
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::root(),
            effect,
            description: String::new(),
            params: json!({}),
            resource_from: ResourceFrom::Handle,
            endpoint_description: String::new(),
        }
    }

    /// The permissions paragraph follows what the kernel lists, not a flag.
    #[test]
    fn the_instructions_describe_the_authority_actually_held() {
        use EffectClass::{Irreversible, Observation, ReversibleMutation};
        type Case<'a> = (&'a [ToolInfo], bool, bool, &'a str, &'a str, &'a str);
        let cases: [Case; 5] = [
            (&[], false, false, "`read-only`", "`never`", "Network access is not available"),
            (&[tool("fs", "read", Observation)], false, false, "`read-only`", "`never`", "not available"),
            (
                &[tool("fs", "create", ReversibleMutation)],
                true,
                false,
                "`workspace-write`",
                "`on-request`",
                "not available",
            ),
            (
                &[tool("shell", "exec", Irreversible)],
                false,
                true,
                "`workspace-write`",
                "`never`",
                "Network access is available",
            ),
            (
                &[tool("web", "fetch", Observation)],
                false,
                false,
                "`read-only`",
                "`never`",
                "Network access is available",
            ),
        ];
        for (tools, guarded, declared, sandbox, approval, network) in cases {
            let held = Held::derive(tools, guarded, declared);
            assert_eq!(held.tools, tools.len());
            let text = instructions(nucleus_prompts::Personality::None, held);
            assert!(nucleus_prompts::unfilled(&text).is_empty(), "{text}");
            for expected in [sandbox, approval, network] {
                assert!(text.contains(expected), "{tools:?}: expected {expected}");
            }
        }
        let pragmatic = instructions(nucleus_prompts::Personality::Pragmatic, Held::default());
        assert!(pragmatic.contains("# Personality"));
    }

    /// The bound is tokens the provider counted: a short conversation is never
    /// worth a model call to summarise, and many messages are no reason to.
    #[test]
    fn compaction_waits_for_the_providers_count_not_for_a_message_count() {
        assert!(!compaction_due(2, 10_000_000, 800_000), "two messages: nothing shorter replaces them");
        assert!(!compaction_due(10, 799_999, 800_000), "under the threshold the conversation stands whole");
        assert!(compaction_due(10, 800_000, 800_000));
        assert!(!compaction_due(5_000, 60_000, 800_000), "a tool-heavy conversation keeps its context");
        let usage =
            |input_tokens, output_tokens| model::Usage { input_tokens, output_tokens, ..Default::default() };
        assert_eq!(
            context_after(500_000, &usage(0, 0)),
            500_000,
            "a reply without a count keeps the last one"
        );
        assert_eq!(context_after(500_000, &usage(600_000, 1_000)), 601_000);
    }
}
