//! The prompt assembly: which text a model is given, and why that text.
//!
//! A system prompt here is not one file. It is a small number of slots filled
//! from configuration and from the state the agent is actually in, so that what
//! the model reads and what the system does cannot drift apart:
//!
//! ```text
//! instructions template
//!   {{ personality }}      pragmatic | friendly | none
//!   {{ tool_count }}       how many tools the kernel derived for this process
//!   {{ sandbox_mode }}     read-only | workspace-write
//!   {{ approval_policy }}  never | on-request
//! ```
//!
//! The shape follows OpenAI's Codex (see `NOTICE`). Here the permissions prose
//! is description and there is no classifier: what an agent may do is the
//! capability set the kernel gave it, and [`crate::guardian::POLICY`] says so
//! to the model in as many words.
//!
//! **The templates are checked against reality.** `tests/tests/prompts.rs`
//! reads every interface's declaration, derives the tool names the kernel would
//! derive (`{endpoint}_{action}`, tool-callable actions only), applying the
//! chat agent's model-facing aliases, with their parameters, and fails if a
//! template puts anything else in backticks. A template that names a tool this
//! system does not have is worse than no template: the model believes it and
//! calls it.

pub mod compact;
pub mod guardian;
pub mod jobs;
pub mod model_instructions;
pub mod permissions;
pub mod personality;
pub mod skills;
pub mod suggest;
pub mod swarm;

pub use model_instructions::{Assembly, render_model_instructions};
pub use permissions::{ApprovalPolicy, NetworkAccess, SandboxMode};
pub use personality::Personality;

/// Replaces `{{ name }}` with `value`, tolerating any spacing inside the
/// braces. Deliberately not a template engine: these templates have a fixed,
/// small set of slots, and a missing one should be visible in the output rather
/// than raise at render time.
pub(crate) fn fill(text: &str, name: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + value.len());
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}") else { break };
        let close = open + close;
        if rest[open + 2..close].trim() == name {
            out.push_str(&rest[..open]);
            out.push_str(value);
            rest = &rest[close + 2..];
        } else {
            out.push_str(&rest[..close + 2]);
            rest = &rest[close + 2..];
        }
    }
    out.push_str(rest);
    out
}

/// Slots left unfilled after rendering: a prompt with a hole in it would say
/// `{{ sandbox_mode }}` to a model. The tests check every rendering with it.
#[must_use]
pub fn unfilled(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}") else { break };
        let close = open + close;
        found.push(rest[open + 2..close].trim().to_owned());
        rest = &rest[close + 2..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_filled_by_name_and_the_rest_are_left_to_be_seen() {
        let text = "a {{ one }} b {{two}} c";
        assert_eq!(fill(text, "one", "1"), "a 1 b {{two}} c");
        assert_eq!(fill(&fill(text, "one", "1"), "two", "2"), "a 1 b 2 c");
        assert_eq!(unfilled("a {{ one }} b {{two}}"), ["one", "two"]);
        assert!(unfilled("nothing here").is_empty());
        // An unterminated brace is text, not an error: it renders as itself.
        assert_eq!(fill("a {{ one", "one", "1"), "a {{ one");
    }
}
