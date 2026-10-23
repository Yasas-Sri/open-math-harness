//! Worker-local coordination ordering, separate from discovery's ownership and
//! timestamp enforcement. Plans are advisory; semantic overlap is model judged.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Disabled,
    Publish,
    Read,
    Review,
    Work,
}

#[derive(Debug)]
pub(crate) struct Planning {
    pub phase: Phase,
    worked: bool,
}

impl Planning {
    pub fn new(enabled: bool) -> Self {
        Self { phase: if enabled { Phase::Publish } else { Phase::Disabled }, worked: false }
    }

    pub fn is_planning(&self) -> bool {
        matches!(self.phase, Phase::Publish | Phase::Read | Phase::Review)
    }

    pub fn incomplete(&self) -> bool {
        self.phase != Phase::Disabled && !self.worked
    }

    pub fn board(&mut self, has_own: bool) {
        self.phase = if has_own { Phase::Review } else { Phase::Publish };
    }

    pub fn work(&mut self) {
        if self.phase != Phase::Disabled {
            self.phase = Phase::Work;
            self.worked = true;
        }
    }

    /// A plan call went out and no board came back (an error, or a reply
    /// that is not a board). A publication may have landed all the same, so
    /// only reading the board settles it: publishing again would move the
    /// plan behind the earlier ones.
    pub fn unsettled(&mut self) {
        if self.phase != Phase::Disabled {
            self.phase = Phase::Read;
        }
    }

    pub fn reset(&mut self) {
        if matches!(self.phase, Phase::Review | Phase::Work) {
            self.phase = Phase::Read;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn publication_and_reset_are_not_work_and_reset_preserves_completion() {
        let mut state = Planning::new(true);
        assert_eq!(state.phase, Phase::Publish);
        state.board(true);
        assert!(state.incomplete());
        state.reset();
        assert_eq!(state.phase, Phase::Read);
        state.board(false);
        assert_eq!(state.phase, Phase::Publish);
        state.board(true);
        state.work();
        state.reset();
        assert_eq!(state.phase, Phase::Read);
        assert!(!state.incomplete());
        state.board(true);
        assert!(state.is_planning());
        assert!(!state.incomplete());
        let mut off = Planning::new(false);
        off.work();
        off.reset();
        off.unsettled();
        assert_eq!(off.phase, Phase::Disabled);
        assert!(!off.incomplete() && !off.is_planning());
    }

    /// A publication whose reply was an error, or not a board, may have
    /// landed all the same: it is read back, never sent a second time.
    #[test]
    fn a_publication_without_a_board_in_reply_is_read_back_not_repeated() {
        let mut unclear = Planning::new(true);
        unclear.unsettled();
        assert_eq!(unclear.phase, Phase::Read);
        unclear.board(false);
        assert_eq!(unclear.phase, Phase::Publish, "it had not landed: publish");
        unclear.unsettled();
        unclear.board(true);
        assert_eq!(unclear.phase, Phase::Review, "it had landed: compare");
        assert!(unclear.incomplete());
    }
}
