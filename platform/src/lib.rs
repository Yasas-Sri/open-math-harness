//! Host platform for Nucleus: confinement, process supervision, and the
//! authenticated channels that give each process its identity.
//!
//! This crate is part of the trusted computing base. It holds operator
//! authority (it can boot and terminate anything) and is the only code that
//! starts the kernel's processes. A flaw here can forge a process's identity,
//! skip its confinement, or start it as the wrong user or in the wrong
//! project's cgroup.

mod cgroup;
mod confinement;
pub mod diagnostics;
mod identity;
mod supervisor;

pub use confinement::{Bind, Bubblewrap, CgroupFiles, Confinement, ConfinementError, ProgramSpec, detect};
pub use supervisor::{EventSink, ReapReport, ShutdownReport, StartError, Supervisor, SupervisorConfig};
