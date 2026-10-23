//! The pair of texts that carry work across a context boundary.
//!
//! A compaction is a handoff, and both halves have to say so. The writing half
//! is told it is writing for a reader with none of the conversation; the
//! reading half is told that the summary is history rather than a plan, **and
//! that everything outside the conversation is still there** — the files, the
//! workspace, the job it started.

/// Given to the model that is summarising, in place of a reply.
pub const PROMPT: &str = include_str!("../templates/compact/prompt.md");

/// Put in front of the summary for the model that picks the work up.
pub const SUMMARY_PREFIX: &str = include_str!("../templates/compact/summary_prefix.md");

/// The message a resuming model sees: the prefix, then the summary.
#[must_use]
pub fn resume_message(summary: &str) -> String {
    format!("{}\n\n{}", SUMMARY_PREFIX.trim(), summary.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resuming_model_reads_the_prefix_then_the_summary() {
        let message = resume_message("  tried A, it failed because B.  ");
        assert!(message.starts_with(SUMMARY_PREFIX.trim()));
        assert!(message.ends_with("\n\ntried A, it failed because B."));
    }
}
