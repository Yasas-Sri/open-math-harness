//! A swarm member's own loop: `--swarm-member free|holding`.
//!
//! The rule that a member holds a direction before it uses any other tool is
//! kept here, by this program, not by the model: a tool call other than
//! reading the board or registering is refused until a registration succeeds,
//! and again once the member gives its direction up, until it registers
//! another. The service decides what a registration is worth (duplicates, one
//! direction at a time); this loop only follows what the service answered. A
//! submission the service took ends the run: every later call is refused and
//! the loop finishes the turn without another model call.
//!
//! The loop also tells the service how many model calls its run has made,
//! after each one (`progress`, not a tool), so the person sees the swarm's
//! spending as it goes; the kernel's budget on its model grant is what holds.

use nucleus_protocol::{Error, ErrorCode, HandleId};
use nucleus_sdk::ChannelError;
use nucleus_swarm_interface::{
    ACTION_ABANDON, ACTION_BOARD, ACTION_PROGRESS, ACTION_REGISTER, ACTION_SUBMIT, ENDPOINT, ProgressRequest,
    tool_name,
};

/// The flag, and its two values: whether the member starts holding a
/// direction (a continuation after a first rejection does).
pub const FLAG: &str = "--swarm-member";
pub const FREE: &str = "free";
pub const HOLDING: &str = "holding";

/// How long a member waits for a registration: the service decides
/// registrations one at a time per swarm, each with a model call.
pub const REGISTER_DEADLINE_MS: u64 = 15 * 60 * 1000;

/// Parses the flag's value: whether the member holds a direction.
pub fn parse(value: &str) -> Result<bool, String> {
    match value {
        FREE => Ok(false),
        HOLDING => Ok(true),
        _ => Err(format!("{FLAG} takes {FREE} or {HOLDING}")),
    }
}

/// How long a progress report may take; the run goes on without it.
const PROGRESS_DEADLINE_MS: u64 = 5_000;

/// What the member's loop knows of its own claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub holding: bool,
    pub submitted: bool,
    /// Its `swarm progress` handle, on its own slot.
    progress: Option<HandleId>,
}

impl Member {
    pub fn new(holding: bool, kernel: &crate::Kernel) -> Self {
        Self { holding, submitted: false, progress: kernel.find_handle(ENDPOINT, ACTION_PROGRESS) }
    }

    /// Tells the service the model calls this run has made. A report that
    /// fails is only said on stderr: it is what the person sees, not what
    /// limits the run.
    pub fn report(&self, kernel: &mut crate::Kernel, calls: u64) -> Result<(), ChannelError> {
        let Some(handle) = self.progress else { return Ok(()) };
        let Some(slot) = kernel.handles().iter().find(|h| h.handle == handle).map(|h| h.resource.clone())
        else {
            return Ok(());
        };
        let request = ProgressRequest { calls };
        if let Err(e) = kernel.call(handle, ACTION_PROGRESS, &slot, &request, Some(PROGRESS_DEADLINE_MS))? {
            eprintln!("nucleus-chat-agent: the swarm did not take this run's progress: {e}");
        }
        Ok(())
    }

    /// Why `tool` may not run now, if it may not.
    pub fn refuse(&self, tool: &str) -> Option<Error> {
        if self.submitted {
            return Some(Error::new(
                ErrorCode::InvalidInput,
                "you submitted your report: this run is over; nothing else runs",
            ));
        }
        let free = [tool_name(ACTION_BOARD), tool_name(ACTION_REGISTER)];
        if self.holding || free.iter().any(|name| name == tool) {
            return None;
        }
        Some(Error::new(
            ErrorCode::InvalidInput,
            "you hold no direction: read the board with ultramath_board, then register your direction with \
             ultramath_register before using any other tool",
        ))
    }

    /// Follows what the service answered `tool`.
    pub fn observe(&mut self, tool: &str, outcome: &Result<String, Error>) {
        if outcome.is_err() {
            return;
        }
        if tool == tool_name(ACTION_REGISTER) {
            self.holding = true;
        } else if tool == tool_name(ACTION_ABANDON) {
            self.holding = false;
        } else if tool == tool_name(ACTION_SUBMIT) {
            self.submitted = true;
        }
    }
}

/// The deadline of a member's call to `tool`, when it needs a longer one than
/// the kernel's default.
pub fn deadline_ms(tool: &str) -> Option<u64> {
    (tool == tool_name(ACTION_REGISTER)).then_some(REGISTER_DEADLINE_MS)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Before a registration only the board and registering run; a refused
    /// registration changes nothing; giving the direction up closes the other
    /// tools again; after a submission nothing runs.
    #[test]
    fn a_member_uses_no_other_tool_before_it_holds_a_direction() {
        let mut member = Member { holding: false, submitted: false, progress: None };
        for tool in ["fs_read", "shell_exec", "web_fetch", "swarm_post", "swarm_submit", "swarm_abandon"] {
            assert!(member.refuse(tool).is_some(), "{tool}");
        }
        assert!(member.refuse("swarm_board").is_none() && member.refuse("swarm_register").is_none());
        member.observe("swarm_register", &Err(Error::new(ErrorCode::InvalidInput, "duplicate of d1")));
        assert!(member.refuse("shell_exec").is_some(), "a refused registration holds nothing");
        member.observe("swarm_register", &Ok(r#"{"direction":"d2"}"#.into()));
        assert!(member.refuse("shell_exec").is_none() && member.refuse("swarm_post").is_none());
        member.observe("swarm_abandon", &Ok("{}".into()));
        assert!(member.refuse("fs_create").is_some(), "abandoning needs a new registration");
        let continued = Member { holding: true, ..member };
        assert!(continued.refuse("fs_create").is_none(), "a continued member keeps its direction");
        member.observe("swarm_register", &Ok(r#"{"direction":"d3"}"#.into()));
        member.observe("swarm_submit", &Ok("{}".into()));
        for tool in ["swarm_board", "swarm_register", "fs_read"] {
            assert!(member.refuse(tool).is_some(), "{tool} after submitting");
        }
        assert_eq!(parse("holding"), Ok(true));
        assert!(parse("yes").is_err());
        assert_eq!(deadline_ms("swarm_register"), Some(REGISTER_DEADLINE_MS));
        assert_eq!(deadline_ms("swarm_post"), None);
    }
}
