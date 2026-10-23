//! Public protocol of Nucleus: identifiers, errors, messages, and framing.
//!
//! This crate is the only thing the kernel, services, agents, and SDKs share.
//! It contains no enforcement; holding any value defined here grants nothing.
//! The supervisor parses every process's frames with it for the kernel, so a
//! parser bug here is a kernel-side bug; [`MAX_FRAME_BYTES`] bounds each frame
//! before it is parsed.

pub mod error;
pub mod frame;
pub mod ids;
pub mod message;
pub mod text;

pub use error::{Error, ErrorCode};
pub use frame::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame};
pub use ids::{
    ActionName, DispatchId, EndpointName, Generation, HandleId, ProcessId, ProgramName, RequestId,
    ResourcePath,
};
pub use message::{
    ACTION_CREATE_ENDPOINT, ACTION_PROVIDE, ACTION_SPAWN, ACTION_SPAWN_AS, ACTION_SPAWN_EXECUTABLE,
    ACTION_STOP_PROJECT, ActionDecl, CREATED_ENDPOINT_PREFIX, ChildBind, ChildInfo, ChildProgram,
    CreatedEndpoint, EffectClass, FromKernel, Grant, HandleInfo, IDENTITY_RESOURCE, IdentityRequest,
    InterfaceDecl, KERNEL_ENDPOINT, MAX_CHILDREN_DEPTH, MAX_ENDPOINT_ACTIONS, PROTOCOL_VERSION, Payload,
    ProjectStopped, ResourceFrom, ResponseBody, ToKernel, ToolInfo, valid_project,
};
