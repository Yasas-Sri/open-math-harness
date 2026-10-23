//! The worker's transcript file: the standard
//! message transcript of one tool loop, append-only, and the only record a
//! continuation starts from.
//!
//! `transcript.jsonl` is a header line, then one line per message as it was
//! appended, with context resets between. [`load_transcript`] rebuilds the
//! buffer the model saw last. A context reset is one line that carries the
//! notice the model read next, so a reset boundary is never followed by a
//! missing notice. Only version [`TRANSCRIPT_VERSION`] is read.

use std::io::Write;
use std::path::Path;

use nucleus_model_interface::{Message, Role};
use serde::{Deserialize, Serialize};

/// The worker's own transcript, in its output directory.
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";
/// The transcript file's line-format version.
pub const TRANSCRIPT_VERSION: u32 = 2;

/// The transcript file's first line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Header {
    pub v: u32,
    pub problem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// `fresh`, `continued` or `submission`.
    pub start: String,
    pub model: String,
}

/// One line after the header.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Line {
    Message(Message),
    /// A context reset: the buffer drops back to its system and first user
    /// message, and `notice` is the user message after them.
    ContextReset {
        before_tokens: u64,
        notice: String,
    },
}

/// The append-only transcript file of one worker.
pub struct Transcript {
    pub(crate) file: std::fs::File,
}

impl Transcript {
    pub(crate) fn create(path: &Path, header: &Header) -> Result<Self, String> {
        let mut file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let line = serde_json::to_string(header).map_err(|e| e.to_string())?;
        writeln!(file, "{line}").map_err(|e| e.to_string())?;
        Ok(Self { file })
    }

    pub(crate) fn write(&mut self, line: &Line) -> Result<(), String> {
        let text = serde_json::to_string(line).map_err(|e| e.to_string())?;
        writeln!(self.file, "{text}").map_err(|e| e.to_string())
    }

    pub(crate) fn message(&mut self, message: &Message) -> Result<(), String> {
        self.write(&Line::Message(message.clone()))
    }
}

/// Rebuilds the message buffer a transcript file ends with: the header is
/// checked, messages are appended in order, and a reset drops the buffer to
/// its system message and first user message ([`kept_prefix`]), then
/// appends its notice. Refuses any version but [`TRANSCRIPT_VERSION`], a
/// reset without a notice, and a line that is not a record, naming the line.
pub fn load_transcript(path: &Path) -> Result<Vec<Message>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let (_, first) = lines.next().ok_or_else(|| format!("{}: empty transcript", path.display()))?;
    let header: Header = serde_json::from_str(first)
        .map_err(|e| format!("{}:1: not a transcript header: {e}", path.display()))?;
    if header.v != TRANSCRIPT_VERSION {
        return Err(format!(
            "{}: transcript version {} is not supported; this runtime reads version {TRANSCRIPT_VERSION}",
            path.display(),
            header.v
        ));
    }
    let mut buffer: Vec<Message> = Vec::new();
    for (index, line) in lines {
        let parsed: Line = serde_json::from_str(line)
            .map_err(|e| format!("{}:{}: not a transcript line: {e}", path.display(), index + 1))?;
        match parsed {
            Line::Message(message) => buffer.push(message),
            Line::ContextReset { notice, .. } => {
                if notice.trim().is_empty() {
                    return Err(format!("{}:{}: invalid context reset", path.display(), index + 1));
                }
                buffer = kept_prefix(&buffer);
                buffer.push(Message::text(Role::User, notice));
            }
        }
    }
    if buffer.is_empty() {
        return Err(format!("{}: the transcript holds no messages", path.display()));
    }
    Ok(buffer)
}

/// What survives a reset: the system message (when the buffer starts with
/// one) and the first user message — the task and the rules.
pub(crate) fn kept_prefix(buffer: &[Message]) -> Vec<Message> {
    let mut kept = Vec::new();
    let mut rest = buffer;
    if let Some((first, tail)) = rest.split_first()
        && first.role == Role::System
    {
        kept.push(first.clone());
        rest = tail;
    }
    if let Some(task) = rest.iter().find(|m| m.role == Role::User) {
        kept.push(task.clone());
    }
    kept
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn text(role: Role, s: &str) -> Message {
        Message::text(role, s)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nucleus-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A reset drops every route taken before it from what a
    /// continuation replays, and its notice is part of the same line.
    #[test]
    fn context_resets_replay_without_restoring_old_routes_or_requiring_a_following_message() {
        let dir = scratch("context-reset");
        let path = dir.join(TRANSCRIPT_FILE);
        let header = Header {
            v: TRANSCRIPT_VERSION,
            problem: "p".into(),
            task: Some("t1".into()),
            start: "continued".into(),
            model: "m".into(),
        };
        let mut t = Transcript::create(&path, &header).unwrap();
        for m in [
            text(Role::System, "rules and tools"),
            text(Role::User, "original problem"),
            text(Role::User, "old node verdict and resume instruction"),
            text(Role::Assistant, "old route"),
            text(Role::Tool, "old result"),
        ] {
            t.message(&m).unwrap();
        }
        let prefix = [text(Role::System, "rules and tools"), text(Role::User, "original problem")];
        for notice in ["saved work is in the directory; choose a route", "reconsider using saved files"] {
            t.write(&Line::ContextReset { before_tokens: 800_000, notice: notice.into() }).unwrap();
            let expected = [prefix.to_vec(), vec![text(Role::User, notice)]].concat();
            assert_eq!(load_transcript(&path).unwrap(), expected, "the event includes its own notice");
            t.message(&text(Role::Assistant, "reasoning since that reset")).unwrap();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("old route") && raw.contains("old result"), "the file keeps them for audit");

        // A reset without its notice and a line that is not a record are
        // refused, and so is every version but 2.
        let head = |v: u32| serde_json::to_string(&Header { v, ..header.clone() }).unwrap();
        let reset = |notice: &str| {
            serde_json::to_string(&Line::ContextReset { before_tokens: 1, notice: notice.into() }).unwrap()
        };
        for (file, refusal) in [
            (format!("{}\n{}\n", head(2), reset(" ")), "invalid context reset"),
            (format!("{}\nnot json\n", head(2)), ":2:"),
            (format!("{}\n{}\n", head(1), reset("notice")), "version 1 is not supported"),
            (format!("{}\n", head(3)), "version 3 is not supported"),
        ] {
            std::fs::write(&path, file).unwrap();
            assert!(load_transcript(&path).unwrap_err().contains(refusal), "{refusal}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
