//! What the agent is told about where its authority comes from.
//!
//! It is not a boundary: the kernel derives an agent's tools from the
//! capabilities it holds, and a call to anything else fails. [`POLICY`] says
//! that to the model in as many words, because an agent that believes it can
//! argue its way to more authority wastes turns trying.

/// Told to the agent: how tools come to exist, and what a refusal means.
pub const POLICY: &str = include_str!("../templates/guardian/policy.md");
