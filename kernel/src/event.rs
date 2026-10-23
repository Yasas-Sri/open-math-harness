//! Security-relevant events. They carry identities and decisions, never
//! payloads, handle numbers, or credentials.

use nucleus_protocol::{ActionName, EndpointName, ErrorCode, Generation, ProcessId, ResourcePath};

/// Emitted by the kernel for an external audit sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecurityEvent {
    ProcessStarted {
        process: ProcessId,
        parent: Option<ProcessId>,
        /// The configured program name, or the executable's guest path when the
        /// child was started with `SpawnExecutable`.
        program: String,
        /// The project it runs as; `None` is the runtime's own.
        identity: Option<String>,
    },
    ProcessStopped {
        process: ProcessId,
    },
    /// A request was refused before dispatch.
    Denied {
        process: ProcessId,
        endpoint: Option<EndpointName>,
        action: Option<ActionName>,
        resource: Option<ResourcePath>,
        reason: ErrorCode,
    },
    Delegated {
        from: ProcessId,
        to: ProcessId,
        endpoint: EndpointName,
        actions: Vec<ActionName>,
        resource: ResourcePath,
    },
    EndpointCreated {
        endpoint: EndpointName,
        owner: ProcessId,
        actions: Vec<ActionName>,
    },
    EndpointReleased {
        endpoint: EndpointName,
        owner: ProcessId,
    },
    EndpointBound {
        endpoint: EndpointName,
        generation: Generation,
        provider: ProcessId,
    },
    EndpointDown {
        endpoint: EndpointName,
        generation: Generation,
    },
    /// A process sent something the protocol does not allow from it.
    ProtocolViolation {
        process: ProcessId,
        detail: &'static str,
    },
    ShutdownStarted,
}
