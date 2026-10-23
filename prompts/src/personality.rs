//! Who the agent is, as a slot in the instructions.
//!
//! The text is about values and how disagreement is handled, not about tone
//! decoration: what it changes is whether the agent pushes back, how it says a
//! thing is wrong, and what it does when a decision has a non-obvious cost.

/// Selectable in configuration (`--personality`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Personality {
    #[default]
    Pragmatic,
    Friendly,
    /// No personality section at all, for an agent whose whole prompt is its
    /// job.
    None,
}

const PRAGMATIC: &str = include_str!("../templates/personalities/pragmatic.md");
const FRIENDLY: &str = include_str!("../templates/personalities/friendly.md");

impl Personality {
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Personality::Pragmatic => PRAGMATIC,
            Personality::Friendly => FRIENDLY,
            Personality::None => "",
        }
    }

    /// Parses the configured name; `None` for anything else, so a typo is a
    /// startup error rather than a silently different agent.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pragmatic" => Some(Personality::Pragmatic),
            "friendly" => Some(Personality::Friendly),
            "none" => Some(Personality::None),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_personality_is_named_exactly_or_not_at_all() {
        assert_eq!(Personality::parse("pragmatic"), Some(Personality::Pragmatic));
        assert_eq!(Personality::parse("none"), Some(Personality::None));
        assert_eq!(Personality::parse("Pragmatic"), None, "a typo is not a different agent");
        assert_eq!(Personality::parse(""), None);
        assert!(Personality::Pragmatic.text().contains("Escalation"));
        assert!(Personality::None.text().is_empty());
    }
}
