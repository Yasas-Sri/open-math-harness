//! Nucleus kernel core.
//!
//! Mechanisms only: process identity bookkeeping, capabilities, per-process
//! handle tables, endpoint generations, bounded IPC admission and dispatch, and
//! ownership-driven teardown. No I/O, no clock, no policy, and no dependency on
//! any agent, service, SDK, or host platform code.
//!
//! It decides every authorization, so it is the trusted computing base for
//! authority: a flaw here can give any process any authority. No process is
//! privileged. Root capabilities are minted only through the operator
//! interface ([`Kernel::boot_process`], from the launcher's validated
//! configuration); no message mints one, so a service name, a tool
//! description or a model's output grants nothing. Every other capability is
//! derived from one: delegated in the grants of a spawn, never wider than its
//! source, or made with a fresh endpoint by `CreateEndpoint`. No operation
//! moves a capability between existing processes. A process manages its own
//! subtree with its own authority: it starts children, lists them and stops a
//! direct child. Nothing is persisted: capabilities, handles and endpoint
//! generations live in memory for the life of the kernel.

mod capability;
mod config;
mod event;
mod kernel;
mod namespace;

pub use capability::CapSpec;
pub use config::Config;
pub use event::SecurityEvent;
pub use kernel::{Kernel, MAX_PROJECT_STOPS, Output};
pub use namespace::{Namespace, NamespaceBind};
