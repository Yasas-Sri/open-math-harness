//! Kernel limits. Deployment-varying, validated once at startup.

use std::collections::{BTreeMap, BTreeSet};

use nucleus_protocol::{Error, ErrorCode, ProgramName};

use crate::namespace::Namespace;

/// Bounds the kernel enforces. Every queue and table has one.
#[derive(Clone, Debug)]
pub struct Config {
    /// Programs the supervisor knows how to start. Spawning any other name is
    /// rejected before a process is created.
    pub programs: BTreeSet<ProgramName>,
    /// What each configured program can see, as the supervisor will confine
    /// it. A program with no entry has an empty namespace and so can start no
    /// executable of its own. Both spawn operations enforce this boundary.
    pub namespaces: BTreeMap<ProgramName, Namespace>,
    /// Whether processes may run as projects: what a `spawn_as` names.
    /// Without, no process can run as anything but the runtime's own identity.
    pub projects: bool,
    pub max_processes: usize,
    pub max_handles_per_process: usize,
    /// Outstanding calls one process may have, queued or in flight.
    pub max_pending_per_caller: usize,
    pub max_queue_per_endpoint: usize,
    pub max_inflight_per_endpoint: usize,
    pub default_deadline_ms: u64,
    pub max_deadline_ms: u64,
    pub max_delegation_depth: u32,
}

impl Config {
    /// The limits every composition runs with (`nucleus_cli::LimitOverrides`
    /// overrides only some of them).
    pub fn with_programs(programs: BTreeSet<ProgramName>) -> Self {
        Self {
            programs,
            namespaces: BTreeMap::new(),
            projects: false,
            max_processes: 64,
            max_handles_per_process: 64,
            max_pending_per_caller: 16,
            max_queue_per_endpoint: 64,
            max_inflight_per_endpoint: 4,
            default_deadline_ms: 30_000,
            max_deadline_ms: 300_000,
            max_delegation_depth: 8,
        }
    }

    /// Rejects configurations that would disable a bound.
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |what: &str| Err(Error::new(ErrorCode::InvalidInput, format!("config: {what}")));
        for namespace in self.namespaces.values() {
            namespace.validate()?;
        }
        if self.max_processes == 0
            || self.max_handles_per_process == 0
            || self.max_pending_per_caller == 0
            || self.max_queue_per_endpoint == 0
            || self.max_inflight_per_endpoint == 0
        {
            return invalid("limits must be at least 1");
        }
        if self.default_deadline_ms == 0 || self.default_deadline_ms > self.max_deadline_ms {
            return invalid("default_deadline_ms must be in 1..=max_deadline_ms");
        }
        if self.max_delegation_depth == 0 {
            return invalid("max_delegation_depth must be at least 1");
        }
        Ok(())
    }
}
