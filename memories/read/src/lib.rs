//! What a session is told it remembers, and which of it the model says it used.
//!
//! Two halves of one loop. The read path injects the consolidated memory as
//! data — explicitly not as instruction, not as authority — and asks the model
//! to cite what actually changed what it did. Those citations are counted, and
//! the count is what the consolidation pass later uses to decide which memories
//! survive. A memory that nothing cites is a memory nobody needed.
//!
//! This crate deliberately depends on nothing, including the write pipeline: a
//! session that reads memory must not be able to write it.

pub mod citations;
pub mod usage;

const READ_PATH: &str = include_str!("../templates/read_path.md");

/// The message a session is given: the framing, then the memories themselves.
///
/// Returns `None` when there is nothing remembered. An empty memory section
/// with a heading over it teaches a model that the system remembers nothing
/// useful, which is a worse start than saying nothing at all.
#[must_use]
pub fn injection(memories: &str) -> Option<String> {
    let memories = memories.trim();
    if memories.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(READ_PATH.len() + memories.len());
    let mut rest = READ_PATH;
    if let Some(open) = rest.find("{{") {
        let close = rest[open..].find("}}").map(|i| open + i);
        if let Some(close) = close {
            out.push_str(&rest[..open]);
            out.push_str(memories);
            rest = &rest[close + 2..];
        }
    }
    out.push_str(rest);
    Some(out.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn nothing_remembered_produces_no_section_at_all() {
        assert!(injection("").is_none());
        assert!(injection("   \n  ").is_none());
    }

    #[test]
    fn the_injection_says_what_the_memory_is_not_before_it_says_what_it_is() {
        let text = injection("- the proxy's base URL is on 18080").expect("some memory");
        assert!(text.contains("not instruction and not authority"));
        assert!(text.contains("the person wins"));
        assert!(text.contains("18080"), "the memories themselves are in it");
        assert!(!text.contains("{{"), "no slot survives: {text}");
    }
}
