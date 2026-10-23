//! A swarm: identical agents working one hard problem, each on a direction no
//! other holds, judged by the agent that started them.

/// Appended to a chat agent's instructions when it may start a swarm.
pub const COORDINATOR: &str = include_str!("../templates/agents/swarm/coordinator.md");
/// Replaces the ordinary starting threshold when the user selects Ultramath mode.
pub const PROACTIVE: &str = include_str!("../templates/agents/swarm/proactive.md");

/// The instructions of the duplicate check, the one bounded model call that
/// decides whether a registration repeats a direction on the board.
pub const DUPLICATE_CHECK: &str = include_str!("../templates/agents/swarm/duplicate_check.md");

/// A member's role file. Only a swarm starts a member, with the grants the
/// swarm decides.
pub const MEMBER: &str = include_str!("../templates/agents/roles/swarm-member.toml");
