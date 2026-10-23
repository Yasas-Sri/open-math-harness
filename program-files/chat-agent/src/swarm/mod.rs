//! Swarms: several identical agents working one hard problem at once, each
//! on a direction no other member holds, judged by the session's agent that
//! started them.
//!
//! Three parts of this program take part, each in its own process:
//!
//! - the **swarm service** (`--swarm-service`, [`service`]), a resident
//!   service of the VM that provides `swarm`: it keeps every board, decides
//!   registrations with a bounded duplicate check ([`check`]) against the
//!   rules of [`board`], starts and stops the members ([`members`]) as the
//!   session's project, writes what happens to a records file of its own,
//!   never into the conversation, appends one `ultramath_ended` reference to
//!   the session's conversation when a swarm ends, and wakes the session
//!   (conversation `wake`) with each member's report and when a swarm runs
//!   out unsolved or a restart interrupted it;
//! - each **member**, this program in one-shot mode with `--swarm-member`,
//!   whose own loop refuses every tool but the board and registration until
//!   it holds a direction ([`member`]);
//! - the **session's agent**, which holds `swarm` `start`, `status`,
//!   `verdict`, `stop` and `records` on its own session as ordinary
//!   kernel-listed tools, and is told how to use them ([`coordinates`]).

pub(crate) mod board;
pub(crate) mod check;
pub mod member;
pub mod members;
pub mod service;

use nucleus_protocol::ToolInfo;

// Model-facing names change independently of the capability endpoint and wire format.
fn tool_action(name: &str, prefix: &str) -> Option<&'static str> {
    let action = name.strip_prefix(prefix)?;
    ["start", "status", "verdict", "stop", "records", "board", "register", "abandon", "post", "submit"]
        .into_iter()
        .find(|known| *known == action)
}

/// The model-facing name of a catalog tool; wire endpoint names stay unchanged.
#[must_use]
pub fn advertised_name(name: &str) -> String {
    tool_action(name, "swarm_").map_or_else(|| name.to_owned(), |action| format!("ultramath_{action}"))
}

pub(crate) fn dispatch_name(name: &str) -> String {
    tool_action(name, "ultramath_").map_or_else(|| name.to_owned(), |action| format!("swarm_{action}"))
}

/// A model call by a legacy `swarm_*` tool name is not executed: the agent
/// answers it with this notice, which names the `ultramath_*` tool to call
/// with the same arguments and swarm ids.
pub(crate) fn upgrade_notice(name: &str) -> Option<String> {
    tool_action(name, "swarm_").map(|action| {
        format!("{name} has been replaced by ultramath_{action}. This call was not executed. Call ultramath_{action} with the same arguments and existing swarm IDs. Use each run's recorded paths: older runs may use swarms/; new runs use .ultramath/. Do not move or rename existing data or rewrite recorded paths.")
    })
}

/// The one tool the duplicate check's model answers with.
pub use check::VERDICT_TOOL as DUPLICATE_VERDICT_TOOL;

/// Whether an agent holding `tools` starts swarms: it may call `swarm start`.
#[must_use]
pub fn coordinates(tools: &[ToolInfo]) -> bool {
    tools.iter().any(|t| {
        t.endpoint.as_str() == nucleus_swarm_interface::ENDPOINT
            && t.action.as_str() == nucleus_swarm_interface::ACTION_START
    })
}
