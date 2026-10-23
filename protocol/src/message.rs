//! Wire messages between a process and the kernel, protocol version 6.
//!
//! The sender of a message is the process that owns the channel it arrived on.
//! No message has a field that names its sender, and none would be believed.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::error::{Error, ErrorCode};
use crate::ids::{
    ActionName, DispatchId, EndpointName, Generation, HandleId, ProcessId, ProgramName, RequestId,
    ResourcePath,
};

/// Version carried by every [`Envelope`]. A peer speaking another version is
/// rejected rather than reinterpreted. A change an existing peer would read
/// differently bumps it; a message only a new peer sends, and whose answer
/// only that peer receives, does not: every other peer reads everything as
/// before.
pub const PROTOCOL_VERSION: u32 = 6;

/// Endpoint name reserved for authority over kernel mechanisms. Nothing can
/// provide or call it: a `Provide` or `Call` that names it is refused.
pub const KERNEL_ENDPOINT: &str = "kernel";
/// Kernel action authorizing [`ToKernel::Spawn`] of `["program", <name>]`.
pub const ACTION_SPAWN: &str = "spawn";
/// Kernel action authorizing [`ToKernel::Provide`] of `["endpoint", <name>]`.
pub const ACTION_PROVIDE: &str = "provide";
/// Creates a fresh owned endpoint under `["endpoint"]`; cannot adopt an
/// existing endpoint. Returned capabilities share this authority's lineage.
pub const ACTION_CREATE_ENDPOINT: &str = "create_endpoint";
/// Bounds one fresh endpoint's invocation interface.
pub const MAX_ENDPOINT_ACTIONS: usize = 64;
/// Reserved for kernel-allocated endpoints. Names are never reused in a run.
pub const CREATED_ENDPOINT_PREFIX: &str = "dynamic.";
/// Kernel action authorizing [`ToKernel::SpawnExecutable`]. The resource is
/// `["executable", <segments of the executable's guest path>]`, so a
/// capability on `["executable"]` covers every executable the caller can see,
/// and one on `["executable", "programs"]` covers `/programs/...` only.
pub const ACTION_SPAWN_EXECUTABLE: &str = "spawn_executable";
/// Kernel action authorizing a [`ToKernel::SpawnExecutable`] child to run as a
/// project's process. The resource is `["identity", <project>]`. Only a process
/// running as the runtime's own identity may use it; a project's processes
/// start children only as that project.
pub const ACTION_SPAWN_AS: &str = "spawn_as";
/// First resource segment of [`ACTION_SPAWN_AS`] and [`ACTION_STOP_PROJECT`].
pub const IDENTITY_RESOURCE: &str = "identity";
/// Kernel action authorizing [`ToKernel::StopProject`], on
/// `["identity", <project>]`: ending every process of a project at once.
/// Only a process running as the runtime's own identity may use it.
pub const ACTION_STOP_PROJECT: &str = "stop_project";
/// Deepest [`ToKernel::Children`] listing a process may ask for.
pub const MAX_CHILDREN_DEPTH: u32 = 16;

/// A directory or file of the spawner's own namespace made visible to a child
/// it starts with [`ToKernel::SpawnExecutable`]. Guest paths are absolute,
/// `/`-separated, and free of `.` and `..` segments, on every host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildBind {
    /// The path in the spawner's own namespace to share.
    pub path: String,
    /// Where the child sees it. Defaults to `path` when absent.
    #[serde(default)]
    pub mount: Option<String>,
    /// Writable in the child. Requires the spawner's own view to be writable.
    #[serde(default)]
    pub writable: bool,
}

/// A program started from an executable the caller can itself read, under the
/// caller with a subset of the caller's authority. The kernel checks, purely
/// against the caller's own namespace, that the executable and every bind lie
/// beneath one of the caller's binds, that a writable bind has a writable one
/// behind it, and that `network` is not claimed without the caller's own; the
/// platform then resolves the real files (symlinks included) at start.
///
/// Started as a project's process it is not sandboxed: it is an ordinary
/// process of the projects' Linux user, takes no `binds` (they are refused),
/// sees the files that user may reach at their real paths, and has the VM's
/// network: `network` is `true`. Only the holder of `spawn_as` may start one
/// with `network: false`, which is then cut off from the network (the discovery
/// workers' shell); from a project's process it is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildProgram {
    /// Absolute path of the executable in the spawner's namespace.
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub binds: Vec<ChildBind>,
    #[serde(default)]
    pub network: bool,
}

/// The project a [`ToKernel::SpawnExecutable`] child runs as, and the
/// `spawn_as` capability that allows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityRequest {
    /// Handle of a `kernel spawn_as` capability covering
    /// `["identity", <project>]`.
    pub authority: HandleId,
    /// The project's name ([`valid_project`]).
    pub project: String,
}

/// Whether `name` may name a project identity: 1–40 bytes of lowercase ASCII
/// letters, digits and `-`, starting with a letter or a digit. It is also
/// the last part of the name of the project's cgroup, and the projects
/// interface's rule for a project's name.
pub fn valid_project(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=40).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The payload answering a [`ToKernel::StopProject`]: nothing of the
/// project ran any more when it was sent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectStopped {}

/// One process in a [`ToKernel::Children`] listing: only what the asking
/// process owns, never a sibling or anything above it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildInfo {
    pub process: ProcessId,
    /// The configured program name, or the executable path it was started from.
    pub program: String,
    /// Endpoints this process currently provides, with their generations.
    pub provides: Vec<(EndpointName, Generation)>,
    pub handles: usize,
    pub children: usize,
}

/// Versioned wrapper around every frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<M> {
    pub v: u32,
    pub msg: M,
}

/// Service-defined JSON that the kernel transports without interpreting.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(Box<RawValue>);

impl Payload {
    /// Serializes `value` as a payload.
    pub fn encode<T: Serialize>(value: &T) -> Result<Self, Error> {
        serde_json::value::to_raw_value(value)
            .map(Self)
            .map_err(|e| Error::new(ErrorCode::InvalidInput, format!("payload encode: {e}")))
    }

    /// Parses the payload. Receivers must treat the content as untrusted input.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_str(self.0.get())
            .map_err(|e| Error::new(ErrorCode::InvalidInput, format!("payload decode: {e}")))
    }

    /// Size of the JSON text in bytes, used for limit enforcement.
    pub fn len(&self) -> usize {
        self.0.get().len()
    }

    /// JSON text is never empty; provided for API completeness.
    pub fn is_empty(&self) -> bool {
        self.0.get().is_empty()
    }
}

impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

/// How an action behaves, declared by its interface. The kernel uses it only
/// to report an interrupted call honestly: an interrupted observation is
/// `unavailable`/`timeout`/`cancelled`, anything else is `unknown_outcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Observation,
    ReversibleMutation,
    Compensatable,
    Irreversible,
}

/// A versioned service interface offered under an endpoint name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceDecl {
    pub endpoint: EndpointName,
    pub version: u32,
    pub actions: BTreeMap<ActionName, ActionDecl>,
    /// What the provider says about itself for a model that is given its
    /// tools: the environment its actions run in, its limits, what it is
    /// not. The kernel passes it through ([`ToolInfo::endpoint_description`])
    /// without reading it; it is the provider's text, trusted like the rest of
    /// the declaration and never authority. Empty when the provider says
    /// nothing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// What a provider declares about one action: how it behaves, and — when the
/// action is meant to be offered to a model — how to present and call it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDecl {
    pub effect: EffectClass,
    /// Present when this action should appear as a callable tool. Internal
    /// actions (e.g. an agent's `receive`) leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolSchema>,
}

impl ActionDecl {
    /// An action that is not offered as a tool.
    pub fn internal(effect: EffectClass) -> Self {
        Self { effect, tool: None }
    }

    /// An action offered to a model as a tool.
    pub fn tool(
        effect: EffectClass,
        description: impl Into<String>,
        params: serde_json::Value,
        resource_from: ResourceFrom,
    ) -> Self {
        Self { effect, tool: Some(ToolSchema { description: description.into(), params, resource_from }) }
    }
}

/// How a tool-callable action is described and how a call's resource is formed
/// from its arguments. The kernel transports this without interpreting it; a
/// caller's tool layer uses it to render the tool and route the call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSchema {
    pub description: String,
    /// JSON Schema for the call's payload. Opaque to the kernel.
    pub params: serde_json::Value,
    #[serde(default)]
    pub resource_from: ResourceFrom,
}

/// Where a tool call's resource path comes from. Lets a generic tool layer put
/// the fine-grained target (a file path, a host) into the resource so the
/// kernel still checks it, without the caller knowing the endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceFrom {
    /// The resource is the handle's own resource (the default).
    #[default]
    Handle,
    /// The named string argument, split on `/`, is the resource (e.g. a path).
    Segments { param: String },
    /// The host of the named URL argument is the resource (e.g. a web host).
    Host { param: String },
}

/// One callable tool for a process: an action it holds a handle for, whose
/// provider declared it tool-callable. Returned by [`ToKernel::Tools`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub endpoint: EndpointName,
    pub action: ActionName,
    /// The handle's resource scope.
    pub resource: ResourcePath,
    pub effect: EffectClass,
    pub description: String,
    pub params: serde_json::Value,
    pub resource_from: ResourceFrom,
    /// The provider's description of itself ([`InterfaceDecl::description`]),
    /// the same on every tool of one endpoint; empty when it gave none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint_description: String,
}

/// One delegation made while spawning a child. Every field is stated in full;
/// the kernel rejects the spawn if the source handle is not delegable or the
/// actions or resource are wider than the source's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// Handle in the spawner's table to delegate from.
    pub from: HandleId,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
    /// Lifetime from now. The result never outlives the source capability.
    pub ttl_ms: Option<u64>,
    pub delegable: bool,
    /// Calls the child may make. They are also charged to every ancestor, so a
    /// parent's budget is shared, never multiplied.
    pub max_calls: Option<u64>,
}

/// Description of a handle for its holder. Informational only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HandleInfo {
    pub handle: HandleId,
    pub endpoint: EndpointName,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
    pub delegable: bool,
    pub expires_in_ms: Option<u64>,
    pub max_calls: Option<u64>,
}

/// A newly allocated endpoint and the caller's two owned handles. Closing
/// either original handle releases the endpoint and invalidates its use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedEndpoint {
    pub endpoint: EndpointName,
    pub provide: HandleInfo,
    pub invoke: HandleInfo,
}

/// Messages a process sends to the kernel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToKernel {
    /// Invoke `action` on `resource` through `handle`.
    Call {
        request: RequestId,
        handle: HandleId,
        action: ActionName,
        resource: ResourcePath,
        payload: Payload,
        /// Relative deadline. The kernel's configured default applies if absent.
        deadline_ms: Option<u64>,
    },
    /// Stop waiting for `request`. If it was already dispatched the provider is
    /// told, but a mutation may still have happened.
    Cancel { request: RequestId },
    /// Provider's outcome for a dispatch.
    Reply { dispatch: DispatchId, result: Result<Payload, Error> },
    /// Start a configured program as a child holding exactly `grants`.
    Spawn { request: RequestId, authority: HandleId, program: ProgramName, grants: Vec<Grant> },
    /// Start a child from an executable in the sender's own namespace, with a
    /// namespace and grants that are subsets of the sender's. Refused before
    /// any process exists if the executable, a bind, the network, or a grant
    /// is wider than the sender holds. The child runs as the sender's
    /// identity, or as the project `identity` names when the sender, running
    /// as the runtime's own, holds `spawn_as` for it.
    SpawnExecutable {
        request: RequestId,
        authority: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<IdentityRequest>,
    },
    /// List the sender's own subtree to `depth` levels (1 = direct children).
    /// No capability is needed: a process may always see what it owns.
    /// `depth` is clamped to [`MAX_CHILDREN_DEPTH`].
    Children { request: RequestId, depth: u32 },
    /// List the sender's callable tools: the actions it holds a handle for whose
    /// live provider declared them tool-callable, with their schemas. No
    /// capability is needed; it reveals only what the sender may already call.
    Tools { request: RequestId },
    /// Become the provider of an endpoint, starting a new generation.
    Provide { request: RequestId, authority: HandleId, interface: InterfaceDecl },
    /// Allocate a fresh endpoint and its provide/invoke handles. Requires a
    /// delegable create_endpoint capability over `["endpoint"]`; no caller
    /// chooses the endpoint name. Every derived operation shares its budget.
    CreateEndpoint { request: RequestId, authority: HandleId, actions: BTreeSet<ActionName> },
    /// Bind a stale handle to the endpoint's current generation. Explicit so
    /// that a holder never talks to a replacement provider unknowingly.
    Rebind { request: RequestId, handle: HandleId },
    /// Terminate a direct child of the sender and everything it owns. A
    /// parent's authority over the lifetime of what it spawned; no capability
    /// names another process, so nothing else can be killed this way.
    Kill { request: RequestId, child: ProcessId },
    /// Cooperatively finish this process. Authority over its whole subtree is
    /// withdrawn immediately; Done is returned only after all descendants have
    /// confirmed physical cleanup. Then the process must perform local cleanup
    /// and exit. Missing cleanup evidence is never converted into permission to
    /// exit. The supervisor applies its ordinary bounded stop after the reply.
    Exit { request: RequestId },
    /// Give up a handle.
    Close { request: RequestId, handle: HandleId },
    /// End every process of `project` at once, whoever started it, authorized
    /// by a `stop_project` capability on `["identity", <project>]`. The
    /// kernel admits it and the supervisor kills the project's cgroup; the
    /// response, a [`ResponseBody::Payload`] holding [`ProjectStopped`], comes
    /// once nothing of the project runs. It is not cancelled. Each process
    /// ended so is torn down as one that exited.
    StopProject { request: RequestId, authority: HandleId, project: String },
}

/// Successful result of a [`ToKernel`] request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBody {
    Payload(Payload),
    Spawned { child: ProcessId },
    Bound { generation: Generation },
    EndpointCreated(Box<CreatedEndpoint>),
    Children(Vec<ChildInfo>),
    Tools(Vec<ToolInfo>),
    Done,
}

/// Messages the kernel sends to a process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromKernel {
    /// First message on every channel.
    Hello {
        process: ProcessId,
        handles: Vec<HandleInfo>,
    },
    Response {
        request: RequestId,
        result: Result<ResponseBody, Error>,
    },
    /// A call admitted for an endpoint this process provides. `caller` is
    /// asserted by the kernel from the caller's channel; `endpoint` says which
    /// of the process's endpoints was called.
    Dispatch {
        dispatch: DispatchId,
        endpoint: EndpointName,
        caller: ProcessId,
        action: ActionName,
        resource: ResourcePath,
        payload: Payload,
        remaining_ms: u64,
    },
    /// The caller no longer wants the outcome of `dispatch`.
    CancelDispatch {
        dispatch: DispatchId,
    },
    /// Logical teardown: authority is gone; the host may still be exiting.
    ChildExited {
        child: ProcessId,
    },
    /// The supervisor confirmed cleanup of this host and its stopped subtree,
    /// including separately supervised children. `success` additionally requires
    /// an ordinary channel close or cooperative Exit and exit status zero,
    /// never a forced kill or protocol failure. `cleanup_confirmed` is independent of that successful-completion
    /// result: an intentionally killed host can be fully cleaned up. This is
    /// lifecycle evidence, not artifact validation. Unconfirmed cleanup does not
    /// produce this event.
    ChildReaped {
        child: ProcessId,
        success: bool,
        cleanup_confirmed: bool,
    },
    /// Sent by the supervisor, not the kernel core: exit now. The host process
    /// is killed when the supervisor's grace period ends.
    Terminate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_survives_a_round_trip_uninterpreted() -> Result<(), Box<dyn std::error::Error>> {
        let msg = ToKernel::Call {
            request: RequestId(7),
            handle: HandleId(0),
            action: ActionName::new("read")?,
            resource: ResourcePath::new(["work", "a.txt"])?,
            payload: Payload::encode(&serde_json::json!({"offset": 3, "nested": [1, {"k": null}]}))?,
            deadline_ms: Some(500),
        };
        let text = serde_json::to_string(&Envelope { v: PROTOCOL_VERSION, msg: msg.clone() })?;
        let back: Envelope<ToKernel> = serde_json::from_str(&text)?;
        assert_eq!(back.msg, msg);
        Ok(())
    }

    #[test]
    fn replies_round_trip_both_arms() -> Result<(), Box<dyn std::error::Error>> {
        for result in
            [Ok(Payload::encode(&"text")?), Err(Error::new(ErrorCode::ServiceError, "no such file"))]
        {
            let msg = ToKernel::Reply { dispatch: DispatchId(1), result };
            let back: ToKernel = serde_json::from_str(&serde_json::to_string(&msg)?)?;
            assert_eq!(back, msg);
        }
        Ok(())
    }

    #[test]
    fn a_sender_field_is_not_part_of_the_protocol() {
        let forged = r#"{"v":1,"msg":{"cancel":{"request":1}},"process":1}"#;
        assert!(serde_json::from_str::<Envelope<ToKernel>>(forged).is_err());
    }
}
