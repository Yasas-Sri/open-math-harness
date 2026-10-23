//! Writing memory: extracting one session's worth, and consolidating what has
//! been extracted.
//!
//! Two passes. Extraction is per session. Consolidation touches the shared
//! artifacts. One memory service owns the store and runs one job at a time, so
//! two passes never rewrite the index over each other.
//!
//! ```text
//! extraction   session record -> model -> {session_summary, session_slug} -> store
//! consolidation  top-N stored rows -> files under the memory root
//!                                  -> unified diff against the stored baseline
//!                                  -> a confined agent that edits MEMORY.md
//! ```
//!
//! Which rows are "top-N" is decided by what has been cited: a memory nothing
//! used, for long enough, stops being selected. That is the loop closing — the
//! read path counts citations, and this is where the count is spent.
//!
//! The split of substrate is deliberate. Job state and the consolidation
//! baseline are in SQLite, because moving the baseline must be atomic. The
//! memory itself is plain files, because what an agent edits should be readable
//! by a person, and the diff against the baseline *is* the prompt that tells
//! the consolidating agent what changed.

pub mod prompts;
pub mod storage;
pub mod workspace;

pub use storage::{Claim, Outcome, Stage1Output, Store};
