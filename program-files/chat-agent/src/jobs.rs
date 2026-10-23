//! Background shell jobs of the agent's own session.
//!
//! In the VM the projects service keeps them: a command `shell_exec` starts
//! with `background: true`, or one a person moves to the background from the
//! window, keeps running after its call has answered, and the session is
//! woken when it ends. Before each shell command the agent tells the
//! service it is its session's agent (`jobs register`, which only that
//! agent's capability reaches; the service forgets the oldest
//! registrations, so one made once could be gone), and it names the command
//! by its tool call, by which the person moves it. The model is offered one
//! tool, `shell_jobs`, bound to the agent's own session as the reminder
//! tools are: `list` and `output` of the project's jobs, and `stop` of the
//! session's own, which is all its capability reaches.

use nucleus_model_interface::ToolSpec;
use nucleus_protocol::{Error, ErrorCode, Payload, ResourcePath, ToolInfo};
use nucleus_sdk::{ChannelError, ToolCall};
use nucleus_shell_interface::jobs as wire;
use serde_json::{Value, json};

/// The one tool the model is offered for its session's jobs.
pub const NAME: &str = "shell_jobs";

/// `shell_jobs`, when this agent holds `jobs list` on its own session.
pub(crate) fn offer(kernel: &crate::Kernel, session: &ResourcePath) -> Option<ToolSpec> {
    kernel.find_handle_for(wire::ENDPOINT, wire::ACTION_LIST, session)?;
    Some(ToolSpec {
        name: NAME.to_owned(),
        description: "Background jobs: shell commands that keep running after their call (shell_exec with \
                      background: true, or moved to the background by a person). list: a page of the jobs of \
                      this project (scope session: of this session only), running ones first, newest first, \
                      with the session that started each, who started it, its state, exit status and last \
                      lines, how many run now (`running`) and `next`, to pass as `page` for the following \
                      page; it shows what is using the VM. At most 16 jobs run in a project at once. Every \
                      job and its output file are kept: remove output files you no longer need. output: a \
                      job's output, its last 16 KiB, or 64 KiB from byte `after`. stop: stops a running job \
                      of this session; its command and everything it started are killed. Another session's \
                      job is someone else's work: never stop or kill it; tell the person. You are woken with a job's result when it ends, so do not wait for one in \
                      a loop."
            .to_owned(),
        parameters: json!({"type": "object", "properties": {
            "action": {"type": "string", "enum": ["list", "output", "stop"]},
            "job": {"type": "string", "description": "The job's id, from shell_exec's answer or list: for output and stop."},
            "after": {"type": "integer", "minimum": 0, "description": "For output: the byte offset to read from; without it, the end of the output."},
            "scope": {"type": "string", "enum": ["project", "session"], "description": "For list: every job of the project (the default), or this session's only."},
            "page": {"type": "string", "description": "For list: the `next` of the previous list, for the following page."}
        }, "required": ["action"]}),
    })
}

/// Takes `background` out of the shell tools of an agent without jobs of
/// its own (a swarm member): the projects service refuses a
/// background command from anyone but a session's registered agent.
pub(crate) fn withhold_background(tools: &mut [ToolInfo]) {
    let exec = |tool: &ToolInfo| {
        tool.endpoint.as_str() == nucleus_shell_interface::ENDPOINT
            && tool.action.as_str() == nucleus_shell_interface::ACTION_EXEC
    };
    for tool in tools.iter_mut().filter(|tool| exec(tool)) {
        if let Some(properties) = tool.params.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove("background");
        }
    }
}

/// Calls the `shell_jobs` action the model asked for on the agent's own
/// session.
pub(crate) fn call(
    kernel: &mut crate::Kernel,
    session: &ResourcePath,
    arguments: &Value,
) -> Result<Result<String, Error>, ChannelError> {
    let invalid = |why: &str| Ok(Err(Error::new(ErrorCode::InvalidInput, why.to_owned())));
    let job = arguments.get("job").and_then(Value::as_str);
    let (action, payload) = match (arguments.get("action").and_then(Value::as_str), job) {
        (Some("list"), _) => (
            wire::ACTION_LIST,
            json!({"scope": arguments.get("scope"), "after": arguments.get("page").and_then(Value::as_str)}),
        ),
        (Some("output"), Some(job)) => (
            wire::ACTION_OUTPUT,
            json!({"job": job, "after": arguments.get("after").and_then(Value::as_u64)}),
        ),
        (Some("stop"), Some(job)) => (wire::ACTION_STOP, json!({"job": job})),
        (Some("output" | "stop"), None) => return invalid("output and stop name a job"),
        _ => return invalid("action is list, output or stop"),
    };
    let Some(handle) = kernel.find_handle_for(wire::ENDPOINT, action, session) else {
        return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent holds no such capability")));
    };
    let reply = kernel.call(handle, action, session, &payload, None)?;
    Ok(reply.and_then(|payload| payload.decode::<Value>()).map(|value| crate::truncate(&value.to_string())))
}

/// Before a `shell exec`: says that this is its session's agent, and names
/// the command by its tool call, so a person can move it to the background.
/// Nothing changes for an agent without `jobs`.
pub(crate) fn name_command(
    kernel: &mut crate::Kernel,
    session: &ResourcePath,
    call_id: &str,
    resolved: &mut ToolCall,
) -> Result<(), ChannelError> {
    let is_exec = resolved.endpoint.as_str() == nucleus_shell_interface::ENDPOINT
        && resolved.action.as_str() == nucleus_shell_interface::ACTION_EXEC;
    let Some(register) = kernel.find_handle_for(wire::ENDPOINT, wire::ACTION_REGISTER, session) else {
        return Ok(());
    };
    if !is_exec {
        return Ok(());
    }
    if let Err(e) = kernel.call(register, wire::ACTION_REGISTER, session, &json!({}), None)? {
        eprintln!("nucleus-chat-agent: this command cannot go to the background: {e}");
        return Ok(());
    }
    if let Ok(Value::Object(mut fields)) = resolved.payload.decode::<Value>()
        && wire::valid_call(call_id)
    {
        fields.insert("call".into(), Value::String(call_id.to_owned()));
        if let Ok(named) = Payload::encode(&Value::Object(fields)) {
            resolved.payload = named;
        }
    }
    Ok(())
}
