//! Ordinary child programs authored and managed by this working agent.
//! Code and dependencies are files in its namespace. Kernel capabilities,
//! not manifests or tool descriptions, authorize every allocation and spawn.
//!
//! `plugin_start` starts a named program from a complete request: `env`,
//! `program`, optional `actions` and explicit `grants`. Each grant must be
//! covered by a delegable handle this agent holds, and none may be on
//! `conversation`. Nonempty actions allocate a `dynamic.<n>` endpoint, whose
//! non-delegable `provide` the child receives and whose name replaces
//! `{endpoint}` in its arguments; the model sees its actions as
//! `plugin-<name>_<action>`. Without actions the program gets no endpoint,
//! `{endpoint}` in its arguments is refused, and the start only observes a
//! live child. `mcp_connect` starts the MCP adapter the same way.
//!
//! `plugin_replace` needs a running instance and stops it, confirming its
//! cleanup, before it starts the new one, so versions never overlap; a later
//! failure leaves the name stopped. A failed candidate whose cleanup is not
//! confirmed within five seconds stays listed as `pending:<pid>`, a name no
//! plugin can take, for a later `plugin_stop`. `plugin_list` shows only this
//! agent's instances and cleanups. Every start is a fresh process whose
//! grants are checked against the agent's current authority. These records
//! live only in this process: a plugin is this agent's child and ends with
//! it, and nothing restarts it or replays a call.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use nucleus_conversation_interface as conversation;
use nucleus_model_interface::ToolSpec;
use nucleus_protocol::{
    ACTION_CREATE_ENDPOINT, ACTION_PROVIDE, ACTION_SPAWN_EXECUTABLE, ActionName, ChildProgram,
    CreatedEndpoint, EndpointName, Error, ErrorCode, Grant, HandleInfo, KERNEL_ENDPOINT, ProcessId,
    ResourcePath, ToolInfo,
};
use nucleus_sdk::{ChannelError, Connection, Event, ToolBinding, ToolCatalog};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONNECT: &str = "mcp_connect";
pub const START: &str = "plugin_start";
pub const REPLACE: &str = "plugin_replace";
pub const STOP: &str = "plugin_stop";
pub const LIST: &str = "plugin_list";
pub const CAPABILITIES: &str = "plugin_capabilities";
const MAX_PLUGINS: usize = 32;
// What the agent adds to its kernel catalog must remain available after
// adding any plugin's tool surface, or a turn would offer the model more than
// it accepts: six plugin operations, the optional guarded remove tool, three
// reminder tools, `view_image` and the session's jobs tool.
const AGENT_TOOL_RESERVE: usize = 6 + 1 + 3 + 1 + 1;
const CLEANUP_WAIT: Duration = Duration::from_secs(5);

/// Launcher-supplied addresses in this agent's view of the filesystem.
/// Informational, never authority. In particular `env` cannot move a child
/// into a different execution domain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    pub env: String,
    pub workspace: String,
    pub workspace_dir: String,
    pub programs_dir: String,
    pub chat_executable: String,
    pub mcp_executable: String,
    /// Declared by the same operator composition that configures the process.
    /// Absent is unknown; never a grant or a connectivity probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Whether this agent, and every program it starts, runs in a sandbox
    /// that shows only its binds (`true`), or as an ordinary process of the
    /// projects' Linux user, which sees the VM's files at their real paths
    /// and takes no binds (`false`). The sessions service says which when it
    /// starts the agent; absent is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandboxed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_executable: Option<String>,
}

impl ExecutionContext {
    pub fn validate(&self) -> Result<(), String> {
        // Labels reach the model and, in tool calls, the person: neither may
        // read one that the other does not see.
        for label in [&self.env, &self.workspace] {
            if label.trim().is_empty() || nucleus_protocol::text::marked(label).any(|(_, hidden)| hidden) {
                return Err(
                    "execution context labels must be nonempty and contain no hidden characters".into()
                );
            }
        }
        for path in [&self.workspace_dir, &self.programs_dir, &self.chat_executable, &self.mcp_executable]
            .into_iter()
            .chain(self.python_executable.iter())
        {
            validate_guest_path(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

fn validate_guest_path(path: &str) -> Result<(), Error> {
    let relative = path.strip_prefix('/').ok_or_else(|| invalid("expected an absolute guest path"))?;
    ResourcePath::new(relative.split('/'))?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestedGrant {
    endpoint: EndpointName,
    actions: BTreeSet<ActionName>,
    #[serde(default)]
    resource: ResourcePath,
    #[serde(default)]
    delegable: bool,
    #[serde(default)]
    max_calls: Option<u64>,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

fn ready_timeout() -> u64 {
    5_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    name: String,
    env: String,
    program: ChildProgram,
    #[serde(default)]
    actions: BTreeSet<ActionName>,
    #[serde(default)]
    grants: Vec<RequestedGrant>,
    #[serde(default = "ready_timeout")]
    ready_timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectRequest {
    name: String,
    url: String,
    tools: Vec<String>,
}

/// The `plugin_start` request `mcp_connect` makes: the adapter's path, its
/// endpoint and the tool mapping from the execution context and the one tool
/// list it was given, and no grants, so the adapter gets none of this
/// agent's service authority. Reusing a connection by its name only confirms
/// that the local adapter process still runs; it is not a check of the
/// remote server.
fn connection_start(request: &ConnectRequest, context: &ExecutionContext) -> Result<Value, Error> {
    let url = url::Url::parse(&request.url).map_err(|_| invalid("invalid MCP URL"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if request.url.len() > 4096
        || request.url.contains(['{', '}', '\0'])
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "MCP URL requires HTTPS (HTTP only for a loopback IP), no userinfo, fragment or placeholders",
        ));
    }
    if request.tools.is_empty() || request.tools.len() > 64 {
        return Err(invalid("select between 1 and 64 MCP tools"));
    }
    let mut selected = BTreeSet::new();
    let mut args = vec![
        "--endpoint".to_owned(),
        "{endpoint}".into(),
        "--url".into(),
        url.to_string(),
        "--timeout-ms".into(),
        "150000".into(),
    ];
    for tool in &request.tools {
        ActionName::new(tool)?;
        if tool.len() > 128 || !selected.insert(tool.clone()) {
            return Err(invalid("MCP tool names must be unique and at most 128 bytes"));
        }
    }
    for tool in &selected {
        args.extend(["--tool".into(), format!("{tool}={tool}")]);
    }
    let mut program = json!({"executable": context.mcp_executable, "args": args});
    if context.sandboxed != Some(false) {
        program["network"] = json!(true);
    }
    Ok(json!({"name": request.name, "env": context.env, "program": program,
        "actions": selected, "grants": [], "ready_timeout_ms": 30000}))
}

#[derive(Clone)]
struct Record {
    name: String,
    service: EndpointName,
    child: ProcessId,
    endpoint: Option<CreatedEndpoint>,
    state: &'static str,
}

impl Record {
    fn binding(&self, context: &ExecutionContext) -> Option<ToolBinding> {
        let endpoint = self.endpoint.as_ref()?;
        Some(ToolBinding {
            service: self.service.clone(),
            endpoint: endpoint.endpoint.clone(),
            env: context.env.clone(),
            workspace: None,
            resource: ResourcePath::root(),
            description: format!(
                "Agent-created service {} in env={}. Its files and capabilities are exactly those explicitly delegated at start; no shared workspace is implied.",
                self.name, context.env
            ),
        })
    }

    fn value(&self, env: &str) -> Value {
        json!({"name":self.name,"env":env,"state":self.state,"process":self.child,
            "endpoint":self.endpoint.as_ref().map(|endpoint| &endpoint.endpoint),
            "service":self.endpoint.as_ref().map(|_| &self.service)})
    }
}

#[derive(Default)]
pub struct Plugins {
    records: BTreeMap<String, Record>,
    connections: BTreeMap<String, Value>,
    // A replacement can fail cleanup before it acquires the public name.
    // Keep the owned process and handles visible for a later explicit stop.
    pending: Vec<Record>,
    owned: BTreeSet<ProcessId>,
    reaped: BTreeMap<ProcessId, bool>,
}

fn authority(held: &[HandleInfo], action: &str, resource: &ResourcePath) -> Option<HandleInfo> {
    held.iter()
        .find(|h| {
            h.endpoint.as_str() == KERNEL_ENDPOINT
                && h.actions.iter().any(|a| a.as_str() == action)
                && resource.is_within(&h.resource)
                && (action != ACTION_CREATE_ENDPOINT || h.delegable)
        })
        .cloned()
}

fn delegated(held: &[HandleInfo], wanted: &[RequestedGrant]) -> Result<Vec<Grant>, Error> {
    wanted
        .iter()
        .map(|need| {
            // A plugin gets no `conversation` authority, whatever this agent
            // holds: on its own session, `receive` would take the person's
            // messages from the agent, and the reminder actions are the
            // agent's alone. What a plugin does reaches the conversation
            // through the agent, as tool results.
            if need.endpoint.as_str() == conversation::ENDPOINT {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "a plugin never gets `conversation` authority",
                ));
            }
            let source = held
                .iter()
                .find(|h| {
                    h.delegable
                        && h.endpoint == need.endpoint
                        && !need.actions.is_empty()
                        && need.actions.is_subset(&h.actions)
                        && need.resource.is_within(&h.resource)
                })
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::PermissionDenied,
                        format!(
                            "no delegable capability covers {} {:?} {:?}",
                            need.endpoint, need.actions, need.resource
                        ),
                    )
                })?;
            Ok(Grant {
                from: source.handle,
                actions: need.actions.clone(),
                resource: need.resource.clone(),
                delegable: need.delegable,
                ttl_ms: need.ttl_ms,
                max_calls: need.max_calls,
            })
        })
        .collect()
}

impl Plugins {
    pub fn handles_name(name: &str) -> bool {
        matches!(name, CONNECT | START | REPLACE | STOP | LIST | CAPABILITIES)
    }

    pub fn enabled(context: Option<&ExecutionContext>, held: &[HandleInfo]) -> bool {
        context.is_some()
            && held.iter().any(|h| {
                h.endpoint.as_str() == KERNEL_ENDPOINT
                    && h.actions.iter().any(|a| a.as_str() == ACTION_SPAWN_EXECUTABLE)
            })
    }

    pub fn bindings(&self, context: &ExecutionContext) -> Vec<ToolBinding> {
        self.records
            .values()
            .chain(&self.pending)
            .filter(|r| r.state == "running")
            .filter_map(|r| r.binding(context))
            .collect()
    }

    pub fn hidden_endpoints(&self) -> BTreeSet<EndpointName> {
        self.records
            .values()
            .chain(&self.pending)
            .filter(|r| r.state != "running")
            .filter_map(|r| r.endpoint.as_ref().map(|endpoint| endpoint.endpoint.clone()))
            .collect()
    }

    pub fn context(context: &ExecutionContext) -> String {
        format!(
            "Use mcp_connect to connect selected remote MCP tools in env={:?}; no capability inspection or plugin development guide is needed. Connections last for this agent. For authoring or debugging custom plugins, use the plugin development guide and plugin_capabilities. Existing capability checks apply to every startup.",
            context.env
        )
    }

    pub(super) fn event<R: Read, W: Write>(
        &mut self,
        kernel: &mut Connection<R, W>,
        event: Event,
    ) -> Result<(), ChannelError> {
        match event {
            Event::ChildReaped { child, cleanup_confirmed, .. } if self.owned.contains(&child) => {
                self.reaped.insert(child, cleanup_confirmed);
            }
            Event::Terminate => return Err(ChannelError::Terminated),
            Event::Dispatch(d) => {
                kernel.reply(d.id, Err(invalid("this agent does not serve that operation")))?
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_candidate<R: Read, W: Write>(
        &self,
        kernel: &mut Connection<R, W>,
        context: &ExecutionContext,
        configured: &[ToolBinding],
        fresh: &Record,
    ) -> Result<Result<(), Error>, ChannelError> {
        let mut tools = match kernel.tools()? {
            Ok(tools) => tools,
            Err(error) => return Ok(Err(error)),
        };
        let mut hidden = self.hidden_endpoints();
        if let Some(endpoint) = self.records.get(&fresh.name).and_then(|old| old.endpoint.as_ref()) {
            hidden.insert(endpoint.endpoint.clone());
        }
        tools.retain(|tool| !hidden.contains(&tool.endpoint));
        let mut bindings = configured.to_vec();
        bindings.extend(
            self.records
                .values()
                .filter(|r| r.name != fresh.name && r.state == "running")
                .filter_map(|r| r.binding(context)),
        );
        bindings.extend(fresh.binding(context));
        // Validate the actual published schemas and public aliases before the
        // new provider enters the conversation.
        Ok(ToolCatalog::new(&tools, &bindings)
            .and_then(|catalog| validate_model_capacity(catalog.tools().len())))
    }

    fn wait_ready<R: Read, W: Write>(
        &mut self,
        kernel: &mut Connection<R, W>,
        record: &Record,
        timeout: u64,
    ) -> Result<Result<(), Error>, ChannelError> {
        let deadline = Instant::now() + Duration::from_millis(timeout);
        loop {
            if kernel.terminate_requested() {
                return Err(ChannelError::Terminated);
            }
            let children = match kernel.children(1)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let Some(child) = children.iter().find(|c| c.process == record.child) else {
                return Ok(Err(Error::new(
                    ErrorCode::Unavailable,
                    "plugin exited before startup was observed",
                )));
            };
            let Some(endpoint) = &record.endpoint else {
                // Ordinary clients need not publish a service. This is an
                // observation of a live child, not an application-ready signal.
                return Ok(Ok(()));
            };
            if child.provides.iter().any(|(name, _)| name == &endpoint.endpoint) {
                return Ok(kernel.rebind(endpoint.invoke.handle)?.map(|_| ()));
            }
            if Instant::now() >= deadline {
                return Ok(Err(Error::new(
                    ErrorCode::Timeout,
                    "plugin did not publish its endpoint before the readiness deadline",
                )));
            }
            if let Some(event) = kernel.next_event_within(Duration::from_millis(25))? {
                self.event(kernel, event)?;
            }
        }
    }

    fn release<R: Read, W: Write>(
        kernel: &mut Connection<R, W>,
        endpoint: Option<&CreatedEndpoint>,
    ) -> Result<Result<(), Error>, ChannelError> {
        let Some(endpoint) = endpoint else {
            return Ok(Ok(()));
        };
        let first = if kernel.handles().iter().any(|h| h.handle == endpoint.provide.handle) {
            kernel.close(endpoint.provide.handle)?
        } else {
            Ok(())
        };
        let second = if kernel.handles().iter().any(|h| h.handle == endpoint.invoke.handle) {
            kernel.close(endpoint.invoke.handle)?
        } else {
            Ok(())
        };
        Ok(first.and(second))
    }

    fn stop_record<R: Read, W: Write>(
        &mut self,
        kernel: &mut Connection<R, W>,
        record: &mut Record,
    ) -> Result<Result<(), Error>, ChannelError> {
        self.stop_record_until(kernel, record, Instant::now() + CLEANUP_WAIT)
    }

    fn stop_record_until<R: Read, W: Write>(
        &mut self,
        kernel: &mut Connection<R, W>,
        record: &mut Record,
        deadline: Instant,
    ) -> Result<Result<(), Error>, ChannelError> {
        if record.state == "stopped" {
            return Ok(Ok(()));
        }
        let children = match kernel.children(1)? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        if children.iter().any(|c| c.process == record.child)
            && let Err(error) = kernel.kill(record.child)?
        {
            // Natural exit can race the preceding Children response. A
            // vanished owned child still needs its physical reap receipt.
            match kernel.children(1)? {
                Ok(children) if !children.iter().any(|c| c.process == record.child) => {}
                _ => return Ok(Err(error)),
            }
        }
        record.state = "cleanup_pending";
        loop {
            if let Some(cleanup_confirmed) = self.reaped.get(&record.child).copied() {
                if !cleanup_confirmed {
                    return Ok(Err(Error::new(
                        ErrorCode::Unavailable,
                        "plugin cleanup unconfirmed; process and handles retained for recovery",
                    )));
                }
                if let Err(e) = Self::release(kernel, record.endpoint.as_ref())? {
                    return Ok(Err(e));
                }
                self.reaped.remove(&record.child);
                self.owned.remove(&record.child);
                record.state = "stopped";
                return Ok(Ok(()));
            }
            if Instant::now() >= deadline {
                return Ok(Err(Error::new(
                    ErrorCode::Timeout,
                    "plugin stop timed out without a clean reap acknowledgment",
                )));
            }
            if let Some(event) = kernel.next_event_within(Duration::from_millis(25))? {
                self.event(kernel, event)?;
            }
        }
    }

    /// Runs one plugin operation (module doc). A start waits for that exact
    /// child to publish its endpoint, rebinds the invoke handle, and checks
    /// the published schemas and tool names against the rest of the catalog
    /// before the model is offered them.
    pub fn run<R: Read, W: Write>(
        &mut self,
        kernel: &mut Connection<R, W>,
        context: &ExecutionContext,
        configured: &[ToolBinding],
        name: &str,
        args: &Value,
    ) -> Result<Result<String, Error>, ChannelError> {
        if !Self::enabled(Some(context), kernel.handles()) {
            return Ok(Err(Error::new(
                ErrorCode::PermissionDenied,
                "this agent has no plugin lifecycle authority",
            )));
        }
        if name == CONNECT {
            let request: ConnectRequest = match serde_json::from_value(args.clone()) {
                Ok(request) => request,
                Err(error) => return Ok(Err(invalid(error.to_string()))),
            };
            let start = match connection_start(&request, context) {
                Ok(start) => start,
                Err(error) => return Ok(Err(error)),
            };
            if let Some(record) = self.records.get(&request.name).filter(|r| r.state != "stopped") {
                if self.connections.get(&request.name) != Some(&start) {
                    return Ok(Err(invalid(
                        "name already in use with different configuration; use plugin_stop with this name before reconnecting",
                    )));
                }
                let children = match kernel.children(1)? {
                    Ok(children) => children,
                    Err(error) => return Ok(Err(error)),
                };
                if record.state != "running" || !children.iter().any(|child| child.process == record.child) {
                    return Ok(Err(invalid(
                        "connection has ended; use plugin_stop with this name to clean up before reconnecting",
                    )));
                }
            } else {
                if !std::path::Path::new(&context.mcp_executable).is_file() {
                    return Ok(Err(Error::new(
                        ErrorCode::Unavailable,
                        "MCP adapter is not installed at the configured runtime path; update or repair the runtime",
                    )));
                }
                if let Err(error) = self.run(kernel, context, configured, START, &start)? {
                    return Ok(Err(error));
                }
                self.connections.insert(request.name.clone(), start);
            }
            let tools: Vec<_> =
                request.tools.iter().map(|tool| format!("plugin-{}_{tool}", request.name)).collect();
            return Ok(Ok(json!({"name": request.name, "state": "connected", "env": context.env,
                "tools": tools, "lifetime": "current agent; plugin_stop disconnects by name"})
            .to_string()));
        }
        if name == CAPABILITIES {
            let request: Capabilities = match serde_json::from_value(args.clone()) {
                Ok(v) => v,
                Err(e) => return Ok(Err(invalid(format!("invalid plugin_capabilities arguments: {e}")))),
            };
            if let Some(target) = request.describe {
                let endpoint = self
                    .records
                    .get(&target)
                    .and_then(|record| record.endpoint.as_ref())
                    .map_or(target.clone(), |endpoint| endpoint.endpoint.to_string());
                // The kernel lists only this agent's own held, tool-callable
                // actions, so an unheld endpoint is indistinguishable from none.
                return Ok(kernel
                    .tools()?
                    .and_then(|tools| describe(&tools, &endpoint))
                    .map(|v| v.to_string()));
            }
            let grants:Vec<_>=kernel.handles().iter().map(|h|json!({"endpoint":h.endpoint,"actions":h.actions,
                "resource":h.resource,"delegable":h.delegable,"expires_in_ms":h.expires_in_ms,"max_calls":h.max_calls})).collect();
            return Ok(Ok(json!({"execution_context":context,"grants":grants,
                "native_protocol":{"version":nucleus_protocol::PROTOCOL_VERSION},
                "guidance":"Call the plugin development guide tool if available in the current catalog. This response describes runtime facts, not a complete development guide."
            }).to_string()));
        }
        if name == LIST {
            if args != &json!({}) {
                return Ok(Err(invalid("plugin_list takes no arguments")));
            }
            let children = match kernel.children(1)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            for r in self.records.values_mut().chain(&mut self.pending) {
                if r.state == "running" && !children.iter().any(|c| c.process == r.child) {
                    r.state = "exited";
                }
            }
            return Ok(Ok(json!({"scope":"created_by_current_agent",
                "note":"This is not the system plugin inventory. Boot services and other agents' plugins are not listed here.",
                "plugins":self.records.values().chain(&self.pending).map(|r|r.value(&context.env)).collect::<Vec<_>>()} ).to_string()));
        }
        if name == STOP {
            let request: Named = match serde_json::from_value(args.clone()) {
                Ok(v) => v,
                Err(e) => return Ok(Err(invalid(e.to_string()))),
            };
            if let Some(mut record) = self.records.remove(&request.name) {
                let result = self.stop_record(kernel, &mut record)?;
                let value = record.value(&context.env).to_string();
                self.records.insert(record.name.clone(), record);
                return Ok(result.map(|_| value));
            }
            if let Some(index) = self.pending.iter().position(|r| r.name == request.name) {
                let mut record = self.pending.remove(index);
                let result = self.stop_record(kernel, &mut record)?;
                let value = record.value(&context.env).to_string();
                if result.is_err() {
                    self.pending.push(record);
                }
                return Ok(result.map(|_| value));
            }
            return Ok(Err(invalid("no plugin owned by this agent has that name")));
        }
        if !matches!(name, START | REPLACE) {
            return Ok(Err(invalid("unknown plugin operation")));
        }
        let mut request: StartRequest = match serde_json::from_value(args.clone()) {
            Ok(v) => v,
            Err(e) => return Ok(Err(invalid(e.to_string()))),
        };
        // A project's agent starts ordinary processes of its user: a plugin
        // sees the VM's files and has its network. `binds` and `network` are
        // settings of a sandbox, so a request that names either is refused,
        // not obeyed in part.
        if context.sandboxed == Some(false) {
            if ["binds", "network"].iter().any(|field| args["program"].get(field).is_some()) {
                return Ok(Err(invalid(
                    "this agent runs as the Linux user its project runs as, in no sandbox, and so does a plugin: it \
                     sees the files your shell sees, at the same paths, and has the VM's network. Give \
                     neither `binds` nor `network`, and name files by their real paths",
                )));
            }
            request.program.network = true;
        }
        let service = match validate_start(&request, context, configured) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let replacing = name == REPLACE;
        let running = self.records.get(&request.name).is_some_and(|r| r.state == "running");
        if replacing && !running {
            return Ok(Err(invalid("plugin_replace requires an owned running plugin")));
        }
        if !replacing && self.records.get(&request.name).is_some_and(|r| r.state != "stopped") {
            return Ok(Err(invalid(
                "plugin already exists; use plugin_replace or confirm its cleanup first",
            )));
        }
        if self.records.len() + self.pending.len() >= MAX_PLUGINS && !self.records.contains_key(&request.name)
        {
            return Ok(Err(Error::new(ErrorCode::LimitExceeded, "too many plugin records in this session")));
        }
        if self.owned.len() >= MAX_PLUGINS && !replacing {
            return Ok(Err(Error::new(
                ErrorCode::LimitExceeded,
                "too many active or unconfirmed plugin processes",
            )));
        }
        let mut grants = match delegated(kernel.handles(), &request.grants) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let exec_resource = match ResourcePath::new(
            std::iter::once("executable")
                .chain(request.program.executable.trim_start_matches('/').split('/')),
        ) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let Some(spawn) = authority(kernel.handles(), ACTION_SPAWN_EXECUTABLE, &exec_resource) else {
            return Ok(Err(Error::new(
                ErrorCode::PermissionDenied,
                "no spawn authority covers that executable",
            )));
        };
        if replacing && let Some(mut old) = self.records.remove(&request.name) {
            // Stop first so versions never overlap exclusive resources. Keep
            // the stopped record if allocation, spawn or publication fails;
            // a later explicit start must supply a complete fresh request.
            let stopped = self.stop_record(kernel, &mut old);
            self.records.insert(old.name.clone(), old);
            if let Err(error) = stopped? {
                return Ok(Err(error));
            }
        }
        let endpoint = if request.actions.is_empty() {
            None
        } else {
            let factory_resource = match ResourcePath::new(["endpoint"]) {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let Some(factory) = authority(kernel.handles(), ACTION_CREATE_ENDPOINT, &factory_resource) else {
                return Ok(Err(Error::new(ErrorCode::PermissionDenied, "no endpoint creation authority")));
            };
            let endpoint = match kernel.create_endpoint(factory.handle, request.actions)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            grants.push(Grant {
                from: endpoint.provide.handle,
                actions: BTreeSet::from([match ActionName::new(ACTION_PROVIDE) {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                }]),
                resource: endpoint.provide.resource.clone(),
                ttl_ms: None,
                delegable: false,
                max_calls: None,
            });
            Some(endpoint)
        };
        let mut program = request.program;
        if let Some(endpoint) = &endpoint {
            for arg in &mut program.args {
                *arg = arg.replace("{endpoint}", endpoint.endpoint.as_str());
            }
        }
        let child = match kernel.spawn_executable(spawn.handle, program, grants)? {
            Ok(v) => v,
            Err(e) => {
                let cleanup = Self::release(kernel, endpoint.as_ref())?;
                return Ok(Err(match cleanup {
                    Ok(()) => e,
                    Err(cleanup) => {
                        Error::new(e.code, format!("{}; endpoint cleanup: {}", e.message, cleanup.message))
                    }
                }));
            }
        };
        self.owned.insert(child);
        let mut fresh = Record { name: request.name.clone(), service, child, endpoint, state: "starting" };
        let ready = match self.wait_ready(kernel, &fresh, request.ready_timeout_ms)? {
            Ok(()) => self.validate_candidate(kernel, context, configured, &fresh)?,
            Err(error) => Err(error),
        };
        if let Err(error) = ready {
            let cleanup = self.stop_record(kernel, &mut fresh)?;
            if let Err(cleanup) = cleanup {
                // ':' is not a valid public plugin-name character, so an
                // ordinary record can never shadow this recovery address.
                fresh.name = format!("pending:{}", child.0);
                self.pending.push(fresh);
                return Ok(Err(Error::new(error.code, format!("{}; {}", error.message, cleanup.message))));
            }
            return Ok(Err(error));
        }
        fresh.state = "running";
        self.connections.remove(&request.name);
        let result = fresh.value(&context.env).to_string();
        self.records.insert(request.name, fresh);
        // Receipts for other child kinds are not lifecycle obligations here.
        let owned: BTreeSet<_> = self.records.values().chain(&self.pending).map(|r| r.child).collect();
        self.reaped.retain(|child, _| owned.contains(child));
        Ok(Ok(result))
    }
}

fn validate_start(
    request: &StartRequest,
    context: &ExecutionContext,
    configured: &[ToolBinding],
) -> Result<EndpointName, Error> {
    EndpointName::new(&request.name)?;
    let service = EndpointName::new(format!("plugin-{}", request.name))?;
    for action in &request.actions {
        let name = nucleus_sdk::tool_name(&service, action);
        if name.len() > 64 || name.bytes().any(|c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-')) {
            return Err(invalid(
                "plugin tool names must contain only letters, digits, hyphens or underscores and fit in 64 characters",
            ));
        }
    }
    if configured.iter().any(|b| b.service == service) {
        return Err(invalid("plugin name conflicts with a configured service"));
    }
    if request.env != context.env {
        return Err(Error::new(
            ErrorCode::PermissionDenied,
            "child plugins must run in this agent's execution environment",
        ));
    }
    if request.actions.len() > 64
        || request.grants.len() > 64
        || request.program.args.len() > 136
        || request.program.binds.len() > 64
        || !(1..=30_000).contains(&request.ready_timeout_ms)
    {
        return Err(invalid(
            "plugin actions, grants, arguments, binds or readiness timeout exceed their bounds",
        ));
    }
    validate_guest_path(&request.program.executable)?;
    for bind in &request.program.binds {
        validate_guest_path(&bind.path)?;
        if let Some(mount) = &bind.mount {
            validate_guest_path(mount)?;
        }
    }
    if request.program.args.iter().any(|a| a.len() > 64 * 1024 || a.contains('\0')) {
        return Err(invalid("invalid plugin program argument"));
    }
    if request.actions.is_empty() && request.program.args.iter().any(|arg| arg.contains("{endpoint}")) {
        return Err(invalid(
            "{endpoint} requires nonempty actions; ordinary programs have no allocated endpoint",
        ));
    }
    Ok(service)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    describe: Option<String>,
}

/// The payload shapes of one endpoint this agent already holds, for writing
/// `context.call` or native calls. Reads only the caller's own tool list.
fn describe(tools: &[ToolInfo], endpoint: &str) -> Result<Value, Error> {
    let actions: Vec<_> = tools
        .iter()
        .filter(|tool| tool.endpoint.as_str() == endpoint)
        .map(|tool| {
            json!({"action":tool.action,"resource":tool.resource,"effect":tool.effect,
                "description":tool.description,"params":tool.params,"resource_from":tool.resource_from})
        })
        .collect();
    if actions.is_empty() {
        let available: BTreeSet<_> = tools.iter().map(|tool| tool.endpoint.as_str()).collect();
        return Err(invalid(format!(
            "no callable actions for {endpoint:?}; available endpoints: {available:?}"
        )));
    }
    Ok(json!({"endpoint":endpoint,"actions":actions,
        "call":"call(endpoint, action, resource, payload): payload follows params. For resource_from segments/host, that param becomes the resource suffix/host and leaves the payload."}))
}

fn validate_model_capacity(tools: usize) -> Result<(), Error> {
    if tools + AGENT_TOOL_RESERVE > nucleus_model_interface::MAX_TOOLS {
        Err(Error::new(
            ErrorCode::LimitExceeded,
            "plugin would exceed the model tool limit; stop an existing plugin first",
        ))
    } else {
        Ok(())
    }
}

pub fn specs(context: &ExecutionContext) -> Vec<ToolSpec> {
    let grant = json!({"type":"object","properties":{"endpoint":{"type":"string"},"actions":{"type":"array","items":{"type":"string"}},
        "resource":{"type":"array","items":{"type":"string"}},"delegable":{"type":"boolean"},"max_calls":{"type":"integer","minimum":0},"ttl_ms":{"type":"integer","minimum":0}},
        "required":["endpoint","actions"],"additionalProperties":false});
    // What a child is decides what its request names: a sandbox has binds
    // and a network grant, a process of the project's user has neither.
    let ordinary = context.sandboxed == Some(false);
    let mut program =
        json!({"executable":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}});
    if !ordinary {
        program["binds"] = json!({"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},
            "mount":{"type":"string"},"writable":{"type":"boolean"}},"required":["path"],"additionalProperties":false}});
        program["network"] = json!({"type":"boolean"});
    }
    let parameters = json!({"type":"object","properties":{"name":{"type":"string"},"env":{"type":"string","enum":[context.env]},
        "program":{"type":"object","properties":program,"required":["executable"],"additionalProperties":false},
        "actions":{"type":"array","items":{"type":"string"},"maxItems":64},"grants":{"type":"array","items":grant},
        "ready_timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["name","env","program"],"additionalProperties":false});
    let child = if ordinary {
        "It runs as the Linux user your project runs as, an ordinary process like your shell's commands: it sees the same files at the same paths and has the VM's network, so its request names no binds and no network."
    } else {
        "It is sandboxed: it sees only the binds you give, and has network only if you ask for it and hold it."
    };
    vec![
        ToolSpec {name:START.into(),description:format!("Start an owned program from code you can see, including a name whose previous instance is stopped. {child} Omit actions or use [] for a program without a tool endpoint: success observes process startup, not application readiness. Nonempty actions allocate an endpoint (use {{endpoint}} in args), wait for publication, and expose plugin-<name>_<action>. Supply explicit grants each time; omitted grants give no existing-service access. It stays in your environment and dies with you. See the development guide and plugin_capabilities."),parameters:parameters.clone()},
        ToolSpec {name:REPLACE.into(),description:"Replace your running plugin with a prepared version. Stops the old process and confirms cleanup before starting the replacement, so versions never overlap. Service candidates must publish their actions; ordinary programs only need to be observed alive. Failed startup leaves the name stopped and retains source files; restart explicitly with a full plugin_start request. For editing/building after stop, use plugin_stop, file/shell tools, then plugin_start. No automatic rollback or inherited old grants; plain process startup does not prove application readiness.".into(),parameters},
        ToolSpec {name:STOP.into(),description:"Stop a plugin you created, including descendants, and wait for clean process reap. Retains source files. Reports unconfirmed cleanup explicitly; it cannot stop another agent's or a boot service's plugin.".into(),parameters:json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false})},
        ToolSpec {name:LIST.into(),description:"List only plugin instances created by this agent, with aliases, environment and cleanup state. An empty list says nothing about boot services or other agents' plugins; this is not the system inventory.".into(),parameters:json!({"type":"object","properties":{},"additionalProperties":false})},
        ToolSpec {name:CAPABILITIES.into(),description:"Inspect runtime facts for plugin development or diagnostics. With no arguments returns paths and held grants. Optionally describe an owned plugin by name or a held endpoint to read its callable action schemas. Ordinary MCP connections use mcp_connect directly. Does not grant authority.".into(),parameters:json!({"type":"object","properties":{"describe":{"type":"string","description":"Owned plugin name or exact held endpoint name. Omit for runtime facts."}},"additionalProperties":false})},
        ToolSpec {name:CONNECT.into(),description:"Connect selected tools from a Streamable HTTP MCP server in this agent's environment. Runtime paths and endpoint allocation are automatic. Reuses an identical live connection; tools appear in the next model call. No authentication support. Ends with this agent; plugin_stop disconnects by name. Remote calls may have side effects and are never automatically replayed.".into(),parameters:json!({"type":"object","properties":{"name":{"type":"string"},"url":{"type":"string","description":"HTTPS MCP URL, or loopback HTTP for development."},"tools":{"type":"array","minItems":1,"maxItems":64,"uniqueItems":true,"items":{"type":"string"}}},"required":["name","url","tools"],"additionalProperties":false})},
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use nucleus_protocol::HandleId;

    #[test]
    fn connection_validation_and_selection_preserve_the_start_contract() {
        let mut c = context();
        c.sandboxed = Some(false);
        let mut request = ConnectRequest {
            name: "db".into(),
            url: "https://example.com/mcp".into(),
            tools: vec!["read".into(), "list".into()],
        };
        let start = connection_start(&request, &c).unwrap();
        assert!(start["program"].get("network").is_none());
        assert!(start["program"].get("binds").is_none());
        assert_eq!(start["grants"], json!([]));
        request.tools.reverse();
        assert_eq!(connection_start(&request, &c).unwrap(), start);
        request.tools = (0..64).map(|i| format!("t{i}")).collect();
        let start: StartRequest = serde_json::from_value(connection_start(&request, &c).unwrap()).unwrap();
        assert!(validate_start(&start, &c, &[]).is_ok());
        for url in [
            "http://example.com/mcp",
            "https://user:secret@example.com/mcp",
            "https://example.com/mcp#fragment",
            "https://example.com/{endpoint}",
        ] {
            request.url = url.into();
            assert!(connection_start(&request, &c).is_err());
        }
        request.url = "http://127.0.0.1:1234/mcp".into();
        assert!(connection_start(&request, &c).is_ok());
        request.tools = vec!["read".into(), "read".into()];
        assert!(connection_start(&request, &c).is_err());
    }
    /// A project's agent starts ordinary processes of its user: its plugin
    /// request names neither binds nor network, which a sandboxed child's does.
    #[test]
    fn a_project_agents_plugin_request_names_neither_binds_nor_network() {
        let fields = |context: &ExecutionContext| -> Vec<String> {
            let start = &specs(context)[0];
            let program = &start.parameters["properties"]["program"]["properties"];
            program.as_object().unwrap().keys().cloned().collect()
        };
        let mut c = context();
        assert_eq!(fields(&c), ["args", "binds", "executable", "network"]);
        c.sandboxed = Some(false);
        assert_eq!(fields(&c), ["args", "executable"]);
        assert!(specs(&c)[0].description.contains("has the VM's network"));
    }

    fn context() -> ExecutionContext {
        ExecutionContext {
            env: "local".into(),
            workspace: "chat".into(),
            workspace_dir: "/work/chat".into(),
            programs_dir: "/programs".into(),
            chat_executable: "/programs/chat".into(),
            mcp_executable: "/programs/mcp".into(),
            network: Some(false),
            sandboxed: None,
            python_executable: None,
        }
    }
    #[test]
    fn program_metadata_is_validated_and_never_supplies_authority() {
        let mut c = context();
        assert!(c.validate().is_ok());
        assert!(!Plugins::enabled(Some(&c), &[]));
        c.workspace_dir = "/work/../secret".into();
        assert!(c.validate().is_err());
        c = context();
        c.env = "".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn plugin_grants_require_an_actually_delegable_covering_handle() {
        let mut held = HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new("fs").unwrap(),
            actions: [ActionName::new("read").unwrap()].into(),
            resource: ResourcePath::new(["work"]).unwrap(),
            delegable: true,
            expires_in_ms: Some(100),
            max_calls: Some(3),
        };
        let need:RequestedGrant=serde_json::from_value(json!({"endpoint":"fs","actions":["read"],"resource":["work","one"],"delegable":true,"max_calls":2})).unwrap();
        assert_eq!(
            delegated(std::slice::from_ref(&held), std::slice::from_ref(&need)).unwrap()[0].from,
            HandleId(1)
        );
        held.delegable = false;
        assert!(delegated(std::slice::from_ref(&held), std::slice::from_ref(&need)).is_err());
        held.delegable = true;
        held.resource = ResourcePath::new(["other"]).unwrap();
        assert!(delegated(&[held], &[need]).is_err());
    }
    #[test]
    fn lifecycle_requests_do_not_change_environment_or_address_boot_services() {
        let mut r:StartRequest=serde_json::from_value(json!({"name":"example","env":"local","program":{"executable":"/programs/service"},"actions":["run"]})).unwrap();
        assert_eq!(validate_start(&r, &context(), &[]).unwrap().as_str(), "plugin-example");
        r.env = "compute".into();
        assert_eq!(validate_start(&r, &context(), &[]).unwrap_err().code, ErrorCode::PermissionDenied);
        r.env = "local".into();
        r.program.executable = "/work/../program".into();
        assert!(validate_start(&r, &context(), &[]).is_err());
    }

    #[test]
    fn plain_programs_need_spawn_authority_but_no_endpoint_factory_or_placeholder() {
        let held = HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new("kernel").unwrap(),
            actions: [ActionName::new(ACTION_SPAWN_EXECUTABLE).unwrap()].into(),
            resource: ResourcePath::new(["executable", "programs"]).unwrap(),
            delegable: false,
            expires_in_ms: None,
            max_calls: None,
        };
        assert!(Plugins::enabled(Some(&context()), &[held]));
        let mut request: StartRequest = serde_json::from_value(json!({
            "name":"client", "env":"local", "program":{"executable":"/programs/client"}
        }))
        .unwrap();
        assert!(request.actions.is_empty());
        assert!(validate_start(&request, &context(), &[]).is_ok());
        request.program.args.push("{endpoint}".into());
        assert!(validate_start(&request, &context(), &[]).is_err());
    }

    #[test]
    fn describe_lists_only_a_held_endpoints_actions() {
        let tool = ToolInfo {
            endpoint: EndpointName::new("fs").unwrap(),
            action: ActionName::new("read").unwrap(),
            resource: ResourcePath::new(["work"]).unwrap(),
            effect: nucleus_protocol::EffectClass::Observation,
            description: "Read a file".into(),
            params: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            resource_from: nucleus_protocol::ResourceFrom::Segments { param: "path".into() },
            endpoint_description: String::new(),
        };
        let held = describe(std::slice::from_ref(&tool), "fs").unwrap();
        assert_eq!(held["actions"][0]["action"], "read");
        assert_eq!(held["actions"][0]["params"], tool.params);
        assert_eq!(held["actions"][0]["resource"], json!(["work"]));
        assert_eq!(describe(&[tool], "web").unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn plugins_leave_room_for_lifecycle_tools_and_use_model_compatible_names() {
        let limit = nucleus_model_interface::MAX_TOOLS - AGENT_TOOL_RESERVE;
        assert!(validate_model_capacity(limit).is_ok());
        assert_eq!(validate_model_capacity(limit + 1).unwrap_err().code, ErrorCode::LimitExceeded);
        // Every tool the agent adds beside the catalog: these, the guarded
        // remove, its reminders and `view_image`; the jobs tool has the
        // reserve's last place.
        let added = specs(&context()).len() + 1 + crate::reminders::specs(|_| true).len() + 1;
        assert!(added <= AGENT_TOOL_RESERVE, "{added} tools added, {AGENT_TOOL_RESERVE} reserved");
        let mut request: StartRequest = serde_json::from_value(json!({"name":"example","env":"local","program":{"executable":"/programs/service"},"actions":["has.dot"]})).unwrap();
        assert!(validate_start(&request, &context(), &[]).is_err());
        request.actions = BTreeSet::from([ActionName::new("a".repeat(60)).unwrap()]);
        assert!(validate_start(&request, &context(), &[]).is_err());
    }
}
