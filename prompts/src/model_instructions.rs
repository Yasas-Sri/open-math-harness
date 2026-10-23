//! The instructions a model is given: one template, the same for every model.
//!
//! The slots are filled from what is true at the moment of the call: the
//! personality from configuration, the tool count from what the kernel actually
//! derived, and the two permission paragraphs from the capabilities the agent
//! holds (the chat agent derives them in `nucleus_chat_agent::Held`).

use crate::personality::Personality;
use crate::{ApprovalPolicy, NetworkAccess, SandboxMode, fill};

const DEFAULT: &str = include_str!("../templates/model_instructions/default_instructions_template.md");

/// What the instructions need to know about the agent they are for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assembly {
    pub personality: Personality,
    pub sandbox_mode: SandboxMode,
    pub approval_policy: ApprovalPolicy,
    pub network: NetworkAccess,
    /// How many tools the kernel derived for this process. Told to the model
    /// because "this list is all there is" is more credible with a number on it.
    pub tools: usize,
}

/// Renders the instructions. Every slot is filled (the tests check it with
/// [`crate::unfilled`]).
#[must_use]
pub fn render_model_instructions(assembly: &Assembly) -> String {
    let text = fill(DEFAULT, "personality", assembly.personality.text().trim());
    let text = fill(&text, "tool_count", &assembly.tools.to_string());
    let text = fill(&text, "sandbox_mode", &assembly.sandbox_mode.text(assembly.network));
    fill(&text, "approval_policy", assembly.approval_policy.text().trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_leaves_no_slot_behind() {
        let rendered = render_model_instructions(&Assembly { tools: 22, ..Assembly::default() });
        assert!(crate::unfilled(&rendered).is_empty(), "{:?}", crate::unfilled(&rendered));
        assert!(rendered.contains("`22`"), "the tool count is the real one");
        assert!(rendered.contains("# Personality"));
    }

    #[test]
    fn a_personality_of_none_leaves_the_slot_empty_rather_than_the_braces() {
        let rendered =
            render_model_instructions(&Assembly { personality: Personality::None, ..Assembly::default() });
        assert!(!rendered.contains("# Personality"));
        assert!(crate::unfilled(&rendered).is_empty());
    }
}
