//! Turning citations into the counts that decide what survives.
//!
//! One rule, and it is the whole point of the loop: a memory's usage is
//! incremented once per session that cited it, not once per citation. A model
//! that cites the same line five times in one reply has not made it five times
//! more useful, and letting it would let a memory keep itself alive.

use std::collections::BTreeSet;

use crate::citations::Citation;

/// What one session's citations mean for the memories they name: the paths
/// used, each counted once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Used {
    pub paths: BTreeSet<String>,
}

impl Used {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// What a session used, from everything it cited.
#[must_use]
pub fn from_citations(citations: &[Citation]) -> Used {
    let mut used = Used::default();
    for citation in citations {
        used.paths.insert(citation.path.clone());
    }
    used
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(path: &str) -> Citation {
        Citation { path: path.into() }
    }

    #[test]
    fn a_memory_cited_twice_in_one_session_was_used_by_one_session() {
        let used = from_citations(&[cite("a.md"), cite("a.md"), cite("b.md")]);
        assert_eq!(used.paths.len(), 2, "two memories, however many citations");
        assert!(!used.is_empty());
        assert!(from_citations(&[]).is_empty());
    }
}
