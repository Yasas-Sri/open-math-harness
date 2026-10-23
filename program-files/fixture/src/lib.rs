//! Fixture-driven agent loop.
//!
//! A fixture is a recording of model output: a list of turns, each with the
//! tool calls the model asked for. The loop executes every call through the
//! kernel with whatever handles this process was given and appends the result
//! to a JSON-lines transcript. No model is contacted and no key is needed.
//!
//! The fixture is untrusted input in the same sense model output is: it can
//! ask for anything, and only the process's capabilities decide what happens.
//!
//! Three tools exist for demonstrations and tests rather than for real agents:
//! `await_operator` (block until a file appears in the signal directory),
//! `host.read` (open a path directly, bypassing every service, to show what
//! the sandbox allows), and `protocol.oversized_frame` (break the channel).
//!
//! `examples/slice` runs it under the launcher, which sandboxes with `bwrap`
//! (`cargo run -p nucleus-cli -- run examples/slice/config.json`), with the `fs`
//! service. Holding `fs` read, list and create on `work/`, the agent reads inside
//! it, is refused `secret.txt`, spawns a worker delegated read on `work/sub` alone,
//! is refused a worker with read on everything, and creates `work/report.txt`; the
//! worker is refused `top.txt`, a planted file and a handle it does not hold. Each
//! writes its calls and results to `out/agent/transcript.jsonl` and
//! `out/agent/worker/transcript.jsonl`, while `log/access.jsonl` holds only what
//! the `fs` service received, so no refused call appears there. `create` never
//! replaces a file, so a second run records a `service_error` for the report
//! unless `data/work/report.txt` is removed first.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use nucleus_protocol::{
    ACTION_SPAWN, ACTION_SPAWN_EXECUTABLE, ActionName, ChildBind, ChildProgram, Error, ErrorCode, Grant,
    HandleId, KERNEL_ENDPOINT, MAX_FRAME_BYTES, Payload, ProgramName, ResourcePath,
};
use nucleus_sdk::{ChannelError, Connection, Event};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Startup options, from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub fixture: PathBuf,
    pub transcript: PathBuf,
    pub signals: Option<PathBuf>,
    /// Endpoint the `fs.*` tools talk to. Operator configuration, not model input.
    pub fs_endpoint: String,
}

impl Options {
    /// Parses `--fixture <file> --transcript <file> [--signals <dir>] [--fs-endpoint <name>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut fixture, mut transcript, mut signals) = (None, None, None);
        let mut fs_endpoint = "fs".to_owned();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let text = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let value = PathBuf::from(&text);
            match flag.as_str() {
                "--fixture" => fixture = Some(value),
                "--transcript" => transcript = Some(value),
                "--signals" => signals = Some(value),
                "--fs-endpoint" => fs_endpoint = text,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            fixture: fixture.ok_or("--fixture is required")?,
            transcript: transcript.ok_or("--transcript is required")?,
            signals,
            fs_endpoint,
        })
    }
}

/// Recorded model output.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub turns: Vec<Turn>,
}

/// One model response.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    #[serde(default)]
    pub note: String,
    pub tool_calls: Vec<ToolCall>,
}

/// A bind requested by a `spawn_executable` tool call, in the spawner's
/// own guest namespace.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureChildBind {
    pub path: String,
    #[serde(default)]
    pub writable: bool,
}

/// A delegation requested by a `spawn` tool call.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureGrant {
    pub endpoint: String,
    pub actions: BTreeSet<ActionName>,
    pub path: ResourcePath,
    #[serde(default)]
    pub delegable: bool,
}

/// A tool call as the model wrote it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "tool", deny_unknown_fields)]
pub enum ToolCall {
    #[serde(rename = "fs.read")]
    FsRead {
        path: ResourcePath,
        #[serde(default)]
        handle: Option<u32>,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    #[serde(rename = "fs.list")]
    FsList {
        path: ResourcePath,
        #[serde(default)]
        handle: Option<u32>,
    },
    #[serde(rename = "fs.create")]
    FsCreate {
        path: ResourcePath,
        content: String,
        #[serde(default)]
        handle: Option<u32>,
    },
    /// One completion through the `model` interface. The model id becomes
    /// the resource path, so the kernel checks it against the capability.
    #[serde(rename = "model.complete")]
    ModelComplete {
        model: String,
        prompt: String,
        max_output_tokens: u32,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    /// A streamed completion: `start`, then `next` until done. Reports the text,
    /// how many pulls it took, and the pieces as they arrived.
    #[serde(rename = "model.stream")]
    ModelStream { model: String, prompt: String, max_output_tokens: u32 },
    /// One raw `next` on a stream id, to show that an id is not authority.
    #[serde(rename = "model.next")]
    ModelNext { model: String, stream: String },
    #[serde(rename = "spawn")]
    Spawn { program: ProgramName, grants: Vec<FixtureGrant> },
    /// Start a child from an executable in this process's own namespace.
    #[serde(rename = "spawn_executable")]
    SpawnExecutable {
        executable: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        binds: Vec<FixtureChildBind>,
        #[serde(default)]
        grants: Vec<FixtureGrant>,
    },
    /// A generic call on any endpoint the process holds a handle for: the
    /// action, the resource path and a JSON payload, exactly as a real agent
    /// would issue it. Lets a fixture drive a new service without a bespoke
    /// verb per interface.
    #[serde(rename = "call")]
    Call {
        endpoint: String,
        action: String,
        #[serde(default)]
        path: ResourcePath,
        #[serde(default)]
        payload: Value,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    #[serde(rename = "wait_child")]
    WaitChild,
    #[serde(rename = "rebind")]
    Rebind { endpoint: String },
    #[serde(rename = "await_operator")]
    AwaitOperator { signal: String },
    #[serde(rename = "host.read")]
    HostRead { path: String },
    #[serde(rename = "protocol.oversized_frame")]
    OversizedFrame,
}

/// Why the loop stopped early.
#[derive(Debug)]
pub enum Stop {
    /// The supervisor asked this process to exit.
    Terminated,
    Failed(String),
}

impl From<ChannelError> for Stop {
    fn from(e: ChannelError) -> Self {
        Self::Failed(e.to_string())
    }
}

type Kernel = Connection<std::io::Stdin, std::io::Stdout>;

fn refused(code: ErrorCode, message: &str) -> Error {
    Error::new(code, message)
}

/// Picks a handle the way a tool layer would: the one that allows the action,
/// or failing that any handle for the endpoint so the kernel gives the verdict.
fn handle_for(
    kernel: &Kernel,
    endpoint: &str,
    action: &str,
    explicit: Option<u32>,
) -> Result<HandleId, Error> {
    if let Some(number) = explicit {
        return Ok(HandleId(number));
    }
    kernel
        .find_handle(endpoint, action)
        .or_else(|| kernel.handles().iter().find(|h| h.endpoint.as_str() == endpoint).map(|h| h.handle))
        .ok_or_else(|| refused(ErrorCode::UnknownHandle, "this process holds no handle for that endpoint"))
}

fn fs_call(
    kernel: &mut Kernel,
    options: &Options,
    action: &str,
    path: &ResourcePath,
    payload: &Value,
    handle: Option<u32>,
    deadline_ms: Option<u64>,
) -> Result<Result<Value, Error>, Stop> {
    let handle = match handle_for(kernel, &options.fs_endpoint, action, handle) {
        Ok(handle) => handle,
        Err(e) => return Ok(Err(e)),
    };
    let reply = kernel.call(handle, action, path, payload, deadline_ms)?;
    Ok(reply.and_then(|payload: Payload| payload.decode::<Value>()))
}

fn execute(kernel: &mut Kernel, options: &Options, call: &ToolCall) -> Result<Result<Value, Error>, Stop> {
    match call {
        ToolCall::FsRead { path, handle, deadline_ms } => {
            fs_call(kernel, options, "read", path, &json!({}), *handle, *deadline_ms)
        }
        ToolCall::FsList { path, handle } => {
            fs_call(kernel, options, "list", path, &json!({}), *handle, None)
        }
        ToolCall::FsCreate { path, content, handle } => {
            fs_call(kernel, options, "create", path, &json!({ "content": content }), *handle, None)
        }
        ToolCall::Call { endpoint, action, path, payload, deadline_ms } => {
            let handle = match handle_for(kernel, endpoint, action, None) {
                Ok(handle) => handle,
                Err(e) => return Ok(Err(e)),
            };
            let reply = kernel.call(handle, action, path, payload, *deadline_ms)?;
            Ok(reply.and_then(|payload: Payload| payload.decode::<Value>()))
        }
        ToolCall::ModelComplete { model, prompt, max_output_tokens, deadline_ms } => {
            use nucleus_model_interface::{ACTION_COMPLETE, CompletionRequest, ENDPOINT, Message, Role};
            let resource = match nucleus_model_interface::resource_for(model) {
                Ok(resource) => resource,
                Err(e) => return Ok(Err(e)),
            };
            let handle = match handle_for(kernel, ENDPOINT, ACTION_COMPLETE, None) {
                Ok(handle) => handle,
                Err(e) => return Ok(Err(e)),
            };
            let request = CompletionRequest {
                messages: vec![Message::text(Role::User, prompt.clone())],
                tools: Vec::new(),
                max_output_tokens: *max_output_tokens,
                temperature: None,
                reasoning_effort: None,
                tool_choice: None,
                project: None,
            };
            let reply = kernel.call(handle, ACTION_COMPLETE, &resource, &request, *deadline_ms)?;
            Ok(reply.and_then(|payload: Payload| payload.decode::<Value>()))
        }
        ToolCall::ModelStream { model, prompt, max_output_tokens } => {
            use nucleus_model_interface as m;
            let resource = match m::resource_for(model) {
                Ok(resource) => resource,
                Err(e) => return Ok(Err(e)),
            };
            let (starts, pulls) = match (
                handle_for(kernel, m::ENDPOINT, m::ACTION_START, None),
                handle_for(kernel, m::ENDPOINT, m::ACTION_NEXT, None),
            ) {
                (Ok(starts), Ok(pulls)) => (starts, pulls),
                (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
            };
            let request = m::CompletionRequest {
                messages: vec![m::Message::text(m::Role::User, prompt.clone())],
                tools: Vec::new(),
                max_output_tokens: *max_output_tokens,
                temperature: None,
                reasoning_effort: None,
                tool_choice: None,
                project: None,
            };
            let started = kernel.call(starts, m::ACTION_START, &resource, &request, None)?;
            let stream = match started.and_then(|p: Payload| p.decode::<m::StartResponse>()) {
                Ok(started) => started.stream,
                Err(e) => return Ok(Err(e)),
            };
            let (mut pieces, mut pull_count) = (Vec::new(), 0u32);
            loop {
                let pull = m::NextRequest { stream: stream.clone(), wait_ms: 300 };
                let reply = kernel.call(pulls, m::ACTION_NEXT, &resource, &pull, None)?;
                pull_count += 1;
                match reply.and_then(|p: Payload| p.decode::<m::NextResponse>()) {
                    Err(e) => return Ok(Err(e)),
                    Ok(next) => {
                        if !next.delta.is_empty() {
                            pieces.push(next.delta);
                        }
                        if let Some(done) = next.done {
                            return Ok(Ok(json!({
                                "stream": stream, "pieces": pieces, "pulls": pull_count,
                                "text": done.message.content, "usage": done.usage,
                            })));
                        }
                    }
                }
            }
        }
        ToolCall::ModelNext { model, stream } => {
            use nucleus_model_interface as m;
            let resource = match m::resource_for(model) {
                Ok(resource) => resource,
                Err(e) => return Ok(Err(e)),
            };
            let pulls = match handle_for(kernel, m::ENDPOINT, m::ACTION_NEXT, None) {
                Ok(pulls) => pulls,
                Err(e) => return Ok(Err(e)),
            };
            let pull = m::NextRequest { stream: stream.clone(), wait_ms: 0 };
            let reply = kernel.call(pulls, m::ACTION_NEXT, &resource, &pull, None)?;
            Ok(reply.and_then(|p: Payload| p.decode::<Value>()))
        }
        ToolCall::Spawn { program, grants } => {
            let authority = match handle_for(kernel, KERNEL_ENDPOINT, ACTION_SPAWN, None) {
                Ok(handle) => handle,
                Err(e) => return Ok(Err(e)),
            };
            let mut delegations = Vec::new();
            for grant in grants {
                let from = match handle_for(kernel, &grant.endpoint, "", None) {
                    Ok(handle) => handle,
                    Err(e) => return Ok(Err(e)),
                };
                delegations.push(Grant {
                    from,
                    actions: grant.actions.clone(),
                    resource: grant.path.clone(),
                    ttl_ms: None,
                    delegable: grant.delegable,
                    max_calls: None,
                });
            }
            let reply = kernel.spawn(authority, program.clone(), delegations)?;
            Ok(reply.map(|child| json!({ "child": child })))
        }
        ToolCall::SpawnExecutable { executable, args, binds, grants } => {
            let authority = match handle_for(kernel, KERNEL_ENDPOINT, ACTION_SPAWN_EXECUTABLE, None) {
                Ok(handle) => handle,
                Err(e) => return Ok(Err(e)),
            };
            let mut delegations = Vec::new();
            for grant in grants {
                let from = match handle_for(kernel, &grant.endpoint, "", None) {
                    Ok(handle) => handle,
                    Err(e) => return Ok(Err(e)),
                };
                delegations.push(Grant {
                    from,
                    actions: grant.actions.clone(),
                    resource: grant.path.clone(),
                    ttl_ms: None,
                    delegable: grant.delegable,
                    max_calls: None,
                });
            }
            let program = ChildProgram {
                executable: executable.clone(),
                args: args.clone(),
                binds: binds
                    .iter()
                    .map(|b| ChildBind { path: b.path.clone(), mount: None, writable: b.writable })
                    .collect(),
                network: false,
            };
            let reply = kernel.spawn_executable(authority, program, delegations)?;
            Ok(reply.map(|child| json!({ "child": child })))
        }
        ToolCall::WaitChild => loop {
            match kernel.next_event()? {
                Event::ChildReaped { .. } => {}
                Event::ChildExited(child) => return Ok(Ok(json!({ "exited": child }))),
                Event::Terminate => return Err(Stop::Terminated),
                Event::Dispatch(_) | Event::CancelDispatch(_) => {}
            }
        },
        ToolCall::Rebind { endpoint } => {
            let handle = match handle_for(kernel, endpoint, "", None) {
                Ok(handle) => handle,
                Err(e) => return Ok(Err(e)),
            };
            Ok(kernel.rebind(handle)?.map(|generation| json!({ "generation": generation })))
        }
        ToolCall::AwaitOperator { signal } => {
            let Some(dir) = &options.signals else {
                return Ok(Err(refused(ErrorCode::InvalidInput, "no signal directory configured")));
            };
            if signal.contains('/') || signal.starts_with('.') {
                return Ok(Err(refused(ErrorCode::InvalidInput, "signal must be a plain file name")));
            }
            let path = dir.join(signal);
            while !path.exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Ok(json!({})))
        }
        ToolCall::HostRead { path } => Ok(match std::fs::read(path) {
            Ok(bytes) => Ok(json!({ "bytes": bytes.len() })),
            Err(e) => Err(refused(ErrorCode::ServiceError, &format!("{:?}", e.kind()))),
        }),
        ToolCall::OversizedFrame => {
            let too_big = u32::try_from(MAX_FRAME_BYTES + 1).unwrap_or(u32::MAX);
            let mut stdout = std::io::stdout();
            let written = stdout.write_all(&too_big.to_be_bytes()).and_then(|()| stdout.flush());
            written.map_err(|e| Stop::Failed(e.to_string()))?;
            // The supervisor drops the channel; the next read reports it.
            match kernel.next_event() {
                Ok(Event::Terminate) | Err(_) => Err(Stop::Terminated),
                Ok(_) => Ok(Ok(json!({ "unexpected": "channel survived" }))),
            }
        }
    }
}

#[derive(Serialize)]
struct Line<'a> {
    turn: usize,
    call: &'a ToolCall,
    result: Value,
}

fn record(transcript: &mut std::fs::File, line: &impl Serialize) -> Result<(), Stop> {
    let mut text =
        serde_json::to_string(line).map_err(|e| Stop::Failed(format!("encode transcript: {e}")))?;
    text.push('\n');
    transcript.write_all(text.as_bytes()).map_err(|e| Stop::Failed(format!("write transcript: {e}")))
}

/// Replays the fixture to the end, or until told to terminate.
pub fn run(options: &Options) -> Result<(), Stop> {
    let failed = |what: &str, e: &dyn std::fmt::Display| Stop::Failed(format!("{what}: {e}"));
    let text = std::fs::read_to_string(&options.fixture).map_err(|e| failed("read fixture", &e))?;
    let fixture: Fixture = serde_json::from_str(&text).map_err(|e| failed("parse fixture", &e))?;
    let mut transcript =
        std::fs::File::create(&options.transcript).map_err(|e| failed("create transcript", &e))?;
    let mut kernel = Connection::from_stdio()?;

    for (turn, recorded) in fixture.turns.iter().enumerate() {
        for call in &recorded.tool_calls {
            let result = match execute(&mut kernel, options, call)? {
                Ok(value) => json!({ "ok": value }),
                Err(error) => json!({ "error": error }),
            };
            record(&mut transcript, &Line { turn, call, result })?;
        }
    }
    record(&mut transcript, &json!({ "done": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_parse_and_reject_unknown_tools() {
        let ok = r#"{"turns":[{"note":"look around","tool_calls":[
            {"tool":"fs.read","path":["work","a.txt"]},
            {"tool":"spawn","program":"worker","grants":[{"endpoint":"fs","actions":["read"],"path":["work"]}]},
            {"tool":"wait_child"}]}]}"#;
        assert!(serde_json::from_str::<Fixture>(ok).is_ok());
        let unknown = r#"{"turns":[{"tool_calls":[{"tool":"shell.exec","cmd":"id"}]}]}"#;
        assert!(serde_json::from_str::<Fixture>(unknown).is_err());
        let traversal = r#"{"turns":[{"tool_calls":[{"tool":"fs.read","path":["work","..","secret"]}]}]}"#;
        assert!(serde_json::from_str::<Fixture>(traversal).is_err());
    }
}
