//! Parsing what the model says it used.
//!
//! The format follows Codex's: a block of lines, each naming a file, a line
//! range, and a note saying what it was good for. Only the file is kept; a
//! line without a well-formed range and note is not a citation.
//!
//! ```text
//! <citation_entries>
//! notes/http-retries.md:12-18|note=[why the backoff is two seconds]
//! </citation_entries>
//! ```
//!
//! Anything that does not parse is dropped rather than guessed at. A citation
//! is used to decide what is kept and what is thrown away, so a malformed one
//! must not be able to keep a memory alive by accident.

/// One thing the model says it used: the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
}

const OPEN: &str = "<citation_entries>";
const CLOSE: &str = "</citation_entries>";

/// Every citation in a reply, in the order given, without duplicates.
#[must_use]
pub fn parse(reply: &str) -> Vec<Citation> {
    let mut found: Vec<Citation> = Vec::new();
    let mut rest = reply;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else { break };
        for line in after[..close].lines() {
            if let Some(entry) = parse_entry(line)
                && !found.contains(&entry)
            {
                found.push(entry);
            }
        }
        rest = &after[close + CLOSE.len()..];
    }
    found
}

/// The reply without its citation blocks.
///
/// The citations are a report to the harness, not part of the answer: a
/// person reading the conversation should see what was said, not the
/// bookkeeping that went with it. Text with no block comes back unchanged.
#[must_use]
pub fn strip(reply: &str) -> String {
    if !reply.contains(OPEN) {
        return reply.to_owned();
    }
    let mut out = String::with_capacity(reply.len());
    let mut rest = reply;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else { break };
        out.push_str(&rest[..open]);
        rest = &after[close + CLOSE.len()..];
    }
    out.push_str(rest);
    out.trim().to_owned()
}

fn parse_entry(line: &str) -> Option<Citation> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (location, note) = line.rsplit_once("|note=[")?;
    note.strip_suffix(']')?;
    let (path, range) = location.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    let (start, end): (u32, u32) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
    if path.trim().is_empty() || start > end {
        return None;
    }
    Some(Citation { path: path.trim().to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(path: &str) -> Citation {
        Citation { path: path.into() }
    }

    #[test]
    fn a_well_formed_block_parses_and_repeats_are_counted_once() {
        let reply = "an answer\n\n<citation_entries>\nnotes/a.md:1-4|note=[why]\n\
                     notes/a.md:1-4|note=[why]\nnotes/b.md:10-10|note=[the number]\n</citation_entries>";
        assert_eq!(parse(reply), [one("notes/a.md"), one("notes/b.md")]);
    }

    #[test]
    fn anything_malformed_is_dropped_rather_than_guessed_at() {
        // A citation keeps a memory alive, so a broken one must not count.
        for bad in [
            "<citation_entries>\nnotes/a.md:1-4\n</citation_entries>", // no note
            "<citation_entries>\nnotes/a.md|note=[why]\n</citation_entries>", // no range
            "<citation_entries>\n:1-4|note=[why]\n</citation_entries>", // no path
            "<citation_entries>\nnotes/a.md:9-4|note=[why]\n</citation_entries>", // backwards
            "<citation_entries>\nnotes/a.md:x-y|note=[why]\n</citation_entries>", // not numbers
            "notes/a.md:1-4|note=[why]",                               // no block
            "<citation_entries>\nnotes/a.md:1-4|note=[why]",           // unterminated
        ] {
            assert!(parse(bad).is_empty(), "should not have parsed: {bad}");
        }
    }

    /// What the person reads is the answer. An unterminated block is left
    /// alone rather than swallowing the rest of the reply.
    #[test]
    fn the_bookkeeping_is_not_part_of_what_was_said() {
        let reply = "The answer is 42.\n\n<citation_entries>\nnotes/a.md:1-4|note=[why]\n                     </citation_entries>";
        assert_eq!(strip(reply), "The answer is 42.");
        assert_eq!(strip("just an answer"), "just an answer");
        assert_eq!(strip(""), "");
        let unterminated = "answer\n<citation_entries>\nnotes/a.md:1-4|note=[why]";
        assert_eq!(strip(unterminated), unterminated);
    }

    #[test]
    fn a_reply_with_no_citations_cites_nothing() {
        assert!(parse("just an answer").is_empty());
        assert!(parse("").is_empty());
    }
}
