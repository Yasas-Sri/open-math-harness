//! What the agent is told about its own confinement.
//!
//! Composed from two axes: where it may write, and whether anyone can be
//! asked. Both describe; neither enforces. The kernel decides what the agent
//! holds, and what the shell service runs as decides what a command can touch
//! — these files exist so the model is not guessing about either. A caller
//! must derive the mode, the policy and the network fact from what the agent
//! actually holds, never from configuration.

use crate::fill;

/// Where commands may write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// The agent holds no file or shell tool that writes.
    ReadOnly,
    /// The agent's tools write, only where their capabilities reach.
    #[default]
    WorkspaceWrite,
}

/// Whether a person can be asked, and on what terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// No one to ask: what is not held is refused.
    Never,
    /// Some actions reach a person, as ordinary tools that block on an answer.
    #[default]
    OnRequest,
}

/// Whether this agent can reach the network: a held tool that fetches, or
/// network the operator's composition gave its process. A fact about the
/// composition, never a preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkAccess {
    #[default]
    Disabled,
    Enabled,
}

const READ_ONLY: &str = include_str!("../templates/permissions/sandbox_mode/read_only.md");
const WORKSPACE_WRITE: &str = include_str!("../templates/permissions/sandbox_mode/workspace_write.md");
const NEVER: &str = include_str!("../templates/permissions/approval_policy/never.md");
const ON_REQUEST: &str = include_str!("../templates/permissions/approval_policy/on_request.md");

impl NetworkAccess {
    /// Said about this agent, not about every service it calls: whether a
    /// given tool's service can reach the network is that service's
    /// configuration, and its own description is where that is stated.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            NetworkAccess::Disabled => {
                "not available: you hold no tool that fetches from the network and your process was not given \
                 network access"
            }
            NetworkAccess::Enabled => {
                "available: you hold a tool that fetches from the network, or your process was given network access"
            }
        }
    }
}

impl SandboxMode {
    /// The paragraph for this mode, with the network fact filled in.
    #[must_use]
    pub fn text(self, network: NetworkAccess) -> String {
        let template = match self {
            SandboxMode::ReadOnly => READ_ONLY,
            SandboxMode::WorkspaceWrite => WORKSPACE_WRITE,
        };
        fill(template, "network_access", network.text()).trim().to_owned()
    }
}

impl ApprovalPolicy {
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            ApprovalPolicy::Never => NEVER,
            ApprovalPolicy::OnRequest => ON_REQUEST,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_paragraph_states_the_network_fact_it_was_given() {
        let confined = SandboxMode::WorkspaceWrite.text(NetworkAccess::Disabled);
        assert!(confined.contains("workspace-write"), "{confined}");
        assert!(confined.contains("not available"), "{confined}");
        assert!(!confined.contains("{{"), "no slot survives: {confined}");
        let open = SandboxMode::ReadOnly.text(NetworkAccess::Enabled);
        assert!(open.contains("Network access is available"), "{open}");
    }
}
