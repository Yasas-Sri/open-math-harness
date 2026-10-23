//! Background shell jobs: a command that keeps running after its call, whose
//! end wakes the session.

/// Appended to a chat agent's instructions when its shell can start one.
pub const GUIDANCE: &str = include_str!("../templates/agents/background_jobs.md");
