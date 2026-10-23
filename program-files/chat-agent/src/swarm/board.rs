//! A swarm's board: which member holds which direction, what
//! was found under each, the submissions and their verdicts. Pure: the
//! service says who calls (from what the kernel checked) and when a duplicate
//! check is needed; this keeps the rules and the bounds.
//!
//! The rules, each refused here rather than advised:
//! - a member holds at most one direction, and registers none while it holds
//!   one or while its submission waits for a verdict;
//! - giving a direction up takes a reason, which stays on the board;
//! - findings and a submission go under the direction the member holds;
//! - a direction's first rejection lets its member try once more, the second
//!   kills it; an accepted submission makes it one of the swarm's solutions.
//!
//! A board is rebuilt from its swarm's records ([`Board::restore`]) when the
//! swarm is resumed: every direction, finding, waiting submission and verdict
//! is in them.
//!
//! The bounds: at most [`MAX_DIRECTIONS`] directions and, under each, at most
//! [`MAX_FINDINGS_PER_DIRECTION`] findings; a registration that would make the
//! board, or a finding that would make its direction's page, larger than its
//! byte bound as JSON is refused, never hidden by dropping older entries.

use std::collections::BTreeMap;

use nucleus_protocol::{Error, ErrorCode};
use nucleus_swarm_interface::{
    DirectionState, FindingKind, MAX_DIRECTIONS, MAX_FINDINGS_PER_DIRECTION, PostRequest, SubmitRequest,
    SwarmRecord,
};
use serde::Serialize;
use serde_json::{Value, json};

/// Most bytes of the board as JSON ([`Board::overview`]): what every member
/// reads, and the larger part of the agent's `status`.
pub const MAX_BOARD_BYTES: usize = 44 * 1024;
/// Most bytes of one direction's page as JSON ([`Board::page`]), beside which
/// `status` shows the solution waiting under it.
pub const MAX_PAGE_BYTES: usize = 32 * 1024;
/// How much of a reason the board shows; the direction's page has all of it.
const REASON_PREVIEW_BYTES: usize = 300;
/// How much of a waiting report the agent's `status` shows beside the board;
/// the direction's page and the swarm's records have all of it.
const REPORT_PREVIEW_BYTES: usize = 400;

fn refused(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, why.into())
}

/// `text` cut to at most `max` bytes at a character boundary, marked when cut.
pub fn preview(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub id: String,
    pub kind: FindingKind,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub accepted: bool,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Direction {
    pub id: String,
    pub member: String,
    pub state: DirectionState,
    pub text: String,
    /// Why it was abandoned or died; empty while active.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub findings: Vec<Finding>,
    pub verdicts: Vec<Verdict>,
}

impl Direction {
    fn rejections(&self) -> usize {
        self.verdicts.iter().filter(|v| !v.accepted).count()
    }

    /// The board's line for it: the text, a preview of the reason, counts.
    fn line(&self) -> Value {
        json!({
            "id": self.id,
            "member": self.member,
            "state": self.state,
            "text": self.text,
            "reason": preview(&self.reason, REASON_PREVIEW_BYTES),
            "findings": self.findings.len(),
            "rejections": self.rejections(),
        })
    }
}

/// A report waiting for the agent's verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Submission {
    pub member: String,
    pub direction: String,
    pub report: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// What a verdict did to the submitter's direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judged {
    /// Accepted: the direction succeeded, one of the swarm's solutions.
    Accepted { direction: String },
    /// Rejected once: its member continues in the direction with the reason.
    Retry { direction: String },
    /// Rejected twice: the direction is dead; its member must register another.
    Dead { direction: String },
}

#[derive(Clone, Debug, Default)]
pub struct Board {
    directions: Vec<Direction>,
    /// By member: at most one each.
    pending: BTreeMap<String, Submission>,
    findings: u64,
}

impl Board {
    pub fn directions(&self) -> &[Direction] {
        &self.directions
    }

    pub fn direction(&self, id: &str) -> Option<&Direction> {
        self.directions.iter().find(|d| d.id == id)
    }

    /// The direction `member` holds.
    pub fn held_by(&self, member: &str) -> Option<&Direction> {
        self.directions.iter().find(|d| d.member == member && d.state == DirectionState::Active)
    }

    pub fn pending(&self) -> impl Iterator<Item = &Submission> {
        self.pending.values()
    }

    pub fn pending_of(&self, member: &str) -> Option<&Submission> {
        self.pending.get(member)
    }

    /// Whether `member` may register now; the reason when it may not. Says
    /// nothing about duplicates, which the service decides.
    pub fn may_register(&self, member: &str) -> Result<(), Error> {
        if let Some(held) = self.held_by(member) {
            return Err(refused(format!(
                "you already hold direction {}; give it up with ultramath_abandon first",
                held.id
            )));
        }
        if self.pending.contains_key(member) {
            return Err(refused("your submission waits for its verdict"));
        }
        if self.directions.len() >= MAX_DIRECTIONS {
            return Err(refused(format!("the board holds {MAX_DIRECTIONS} directions, its limit")));
        }
        Ok(())
    }

    /// A direction whose text is `text` up to case and spacing: a duplicate
    /// that needs no model call to recognise.
    pub fn same_text(&self, text: &str) -> Option<&Direction> {
        let normal = |t: &str| t.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        let wanted = normal(text);
        self.directions.iter().find(|d| normal(&d.text) == wanted)
    }

    /// Registers `text` as the direction `member` holds, once the service
    /// has decided it duplicates none. Returns its id.
    pub fn register(&mut self, member: &str, text: &str) -> Result<String, Error> {
        self.may_register(member)?;
        let id = format!("d{}", self.directions.len() + 1);
        self.directions.push(Direction {
            id: id.clone(),
            member: member.to_owned(),
            state: DirectionState::Active,
            text: text.to_owned(),
            reason: String::new(),
            findings: Vec::new(),
            verdicts: Vec::new(),
        });
        if bytes(&self.overview()) > MAX_BOARD_BYTES {
            self.directions.pop();
            return Err(refused("the board is full: no more directions fit"));
        }
        Ok(id)
    }

    fn held_mut(&mut self, member: &str) -> Result<&mut Direction, Error> {
        if self.pending.contains_key(member) {
            return Err(refused("your submission waits for its verdict"));
        }
        self.directions
            .iter_mut()
            .find(|d| d.member == member && d.state == DirectionState::Active)
            .ok_or_else(|| refused("you hold no direction: register one with ultramath_register first"))
    }

    /// `member` gives up the direction it holds, for `reason`.
    pub fn abandon(&mut self, member: &str, reason: &str) -> Result<String, Error> {
        let direction = self.held_mut(member)?;
        direction.state = DirectionState::Abandoned;
        direction.reason = reason.to_owned();
        Ok(direction.id.clone())
    }

    /// Posts `request` under the direction `member` holds: its id and the
    /// direction's.
    pub fn post(&mut self, member: &str, request: &PostRequest) -> Result<(String, String), Error> {
        let id = format!("f{}", self.findings + 1);
        let direction = self.held_mut(member)?;
        if direction.findings.len() >= MAX_FINDINGS_PER_DIRECTION {
            return Err(refused(format!(
                "direction {} holds {MAX_FINDINGS_PER_DIRECTION} findings, its limit: keep notes in your \
                 directory's files and post their paths",
                direction.id
            )));
        }
        direction.findings.push(Finding {
            id: id.clone(),
            kind: request.kind,
            text: request.text.clone(),
            paths: request.paths.clone(),
        });
        let direction_id = direction.id.clone();
        if self.page(&direction_id).map(|page| bytes(&page)).unwrap_or(usize::MAX) > MAX_PAGE_BYTES {
            if let Some(direction) = self.directions.iter_mut().find(|d| d.id == direction_id) {
                direction.findings.pop();
            }
            return Err(refused(format!(
                "direction {direction_id}'s page is full: keep notes in your directory's files and post their paths"
            )));
        }
        self.findings += 1;
        Ok((id, direction_id))
    }

    /// Whether `member` may submit now: the direction it holds, under which
    /// its report would go.
    pub fn may_submit(&mut self, member: &str) -> Result<String, Error> {
        Ok(self.held_mut(member)?.id.clone())
    }

    /// Records the report `member` submits under `direction`, the one it
    /// holds; it waits for a verdict.
    pub fn submit(&mut self, member: &str, direction: &str, request: &SubmitRequest) -> Result<(), Error> {
        if self.may_submit(member)? != direction {
            return Err(refused(format!("you no longer hold direction {direction}")));
        }
        self.pending.insert(
            member.to_owned(),
            Submission {
                member: member.to_owned(),
                direction: direction.to_owned(),
                report: request.report.clone(),
                paths: request.paths.clone(),
            },
        );
        Ok(())
    }

    /// The verdict on `member`'s waiting submission.
    pub fn judge(&mut self, member: &str, accepted: bool, reason: &str) -> Result<Judged, Error> {
        let submission = self
            .pending
            .remove(member)
            .ok_or_else(|| refused(format!("no submission of {member} waits for a verdict")))?;
        let Some(direction) = self.directions.iter_mut().find(|d| d.id == submission.direction) else {
            return Err(Error::new(ErrorCode::ServiceError, "a submission names no direction"));
        };
        direction.verdicts.push(Verdict { accepted, reason: reason.to_owned() });
        let id = direction.id.clone();
        if accepted {
            direction.state = DirectionState::Succeeded;
            return Ok(Judged::Accepted { direction: id });
        }
        if direction.rejections() >= 2 {
            direction.state = DirectionState::Dead;
            direction.reason = reason.to_owned();
            return Ok(Judged::Dead { direction: id });
        }
        Ok(Judged::Retry { direction: id })
    }

    /// How many directions' submissions were accepted.
    pub fn accepted(&self) -> usize {
        self.directions.iter().filter(|d| d.state == DirectionState::Succeeded).count()
    }

    /// The board as `records` leave it, in their order: each direction as its
    /// last record says, its findings, the submissions still waiting and the
    /// verdicts given.
    pub fn restore<'a>(records: impl IntoIterator<Item = &'a SwarmRecord>) -> Self {
        let mut board = Self::default();
        for record in records {
            match record {
                SwarmRecord::Direction { member, direction, state, text, reason, .. } => {
                    match board.directions.iter_mut().find(|d| d.id == *direction) {
                        Some(known) => {
                            known.state = *state;
                            known.reason.clone_from(reason);
                        }
                        None => board.directions.push(Direction {
                            id: direction.clone(),
                            member: member.clone(),
                            state: *state,
                            text: text.clone(),
                            reason: reason.clone(),
                            findings: Vec::new(),
                            verdicts: Vec::new(),
                        }),
                    }
                }
                SwarmRecord::Finding { direction, finding, label, text, paths, .. } => {
                    if let Some(known) = board.directions.iter_mut().find(|d| d.id == *direction) {
                        known.findings.push(Finding {
                            id: finding.clone(),
                            kind: *label,
                            text: text.clone(),
                            paths: paths.clone(),
                        });
                    }
                    board.findings += 1;
                }
                SwarmRecord::Submission { member, direction, report, paths, .. } => {
                    let submission = Submission {
                        member: member.clone(),
                        direction: direction.clone(),
                        report: report.clone(),
                        paths: paths.clone(),
                    };
                    board.pending.insert(member.clone(), submission);
                }
                SwarmRecord::Verdict { member, direction, accepted, reason, .. } => {
                    board.pending.remove(member);
                    if let Some(known) = board.directions.iter_mut().find(|d| d.id == *direction) {
                        known.verdicts.push(Verdict { accepted: *accepted, reason: reason.clone() });
                    }
                }
                _ => {}
            }
        }
        board
    }

    /// `member`'s run ended without a submission waiting: the direction it
    /// held, if any, is given up for `reason`, so that nobody repeats it.
    pub fn member_ended(&mut self, member: &str, reason: &str) -> Option<String> {
        if self.pending.contains_key(member) {
            return None;
        }
        self.abandon(member, reason).ok()
    }

    /// Submissions nobody will judge any more (the swarm stopped); they stay
    /// in the swarm's records.
    pub fn drop_pending(&mut self) -> Vec<Submission> {
        std::mem::take(&mut self.pending).into_values().collect()
    }

    /// The board every member reads: each direction's line.
    pub fn overview(&self) -> Value {
        json!({"directions": self.directions.iter().map(Direction::line).collect::<Vec<_>>()})
    }

    /// One direction's page: its text, reason, findings and verdicts.
    pub fn page(&self, id: &str) -> Result<Value, Error> {
        let direction = self.direction(id).ok_or_else(|| refused(format!("no direction {id}")))?;
        serde_json::to_value(direction).map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))
    }

    /// Previews of the waiting reports, for the agent's `status`, with each
    /// one's size: the whole of one is in its direction's page.
    pub fn pending_previews(&self) -> Vec<Value> {
        self.pending
            .values()
            .map(|s| {
                json!({"member": s.member, "direction": s.direction,
                       "report": preview(&s.report, REPORT_PREVIEW_BYTES), "bytes": s.report.len(),
                       "paths": s.paths})
            })
            .collect()
    }
}

/// Bytes of `value` as JSON.
pub fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use nucleus_swarm_interface::{MAX_DIRECTION_BYTES, MAX_FINDING_BYTES};

    fn post(text: &str) -> PostRequest {
        PostRequest { kind: FindingKind::Lemma, text: text.into(), paths: Vec::new() }
    }

    fn submit(text: &str) -> SubmitRequest {
        SubmitRequest { report: text.into(), paths: Vec::new() }
    }

    /// One direction at a time; giving it up takes a reason that stays on the
    /// board; nothing is posted or submitted without one.
    #[test]
    fn a_member_holds_one_direction_and_works_only_under_it() {
        let mut board = Board::default();
        assert!(board.post("m1", &post("x")).is_err(), "no direction yet");
        assert!(board.submit("m1", "d1", &submit("x")).is_err());
        assert!(board.abandon("m1", "nothing to give up").is_err());
        assert_eq!(board.register("m1", "induction on n").unwrap(), "d1");
        let second = board.register("m1", "a parity argument").unwrap_err();
        assert!(second.message.contains("d1"), "{}", second.message);
        assert_eq!(board.post("m1", &post("f(2) = 4")).unwrap(), ("f1".to_owned(), "d1".to_owned()));
        assert!(board.post("m2", &post("not mine")).is_err(), "a member posts under its own direction only");
        assert_eq!(board.abandon("m1", "the induction step needs f(n+2)").unwrap(), "d1");
        let d1 = board.direction("d1").unwrap();
        assert_eq!(
            (d1.state, d1.reason.as_str()),
            (DirectionState::Abandoned, "the induction step needs f(n+2)")
        );
        assert_eq!(board.overview()["directions"][0]["reason"], "the induction step needs f(n+2)");
        assert!(board.post("m1", &post("y")).is_err(), "after abandoning, register again first");
        assert_eq!(board.register("m1", "a parity argument").unwrap(), "d2");
        assert_eq!(board.same_text("  A PARITY\nargument ").map(|d| d.id.as_str()), Some("d2"));
        assert_eq!(
            board.same_text("induction on N").map(|d| d.id.as_str()),
            Some("d1"),
            "abandoned ones count"
        );
    }

    /// A rejection lets the member try once more in the same direction; the
    /// second kills it; an accept marks it a solution.
    #[test]
    fn the_second_rejection_kills_a_direction_and_an_accept_solves_it() {
        let mut board = Board::default();
        board.register("m1", "induction").unwrap();
        board.register("m2", "parity").unwrap();
        assert!(board.judge("m1", false, "nothing submitted").is_err());
        board.submit("m1", "d1", &submit("proof one")).unwrap();
        assert!(board.register("m1", "other").is_err(), "waiting for a verdict");
        assert!(board.post("m1", &post("x")).is_err(), "waiting for a verdict");
        assert_eq!(
            board.member_ended("m1", "its run ended"),
            None,
            "a waiting submission keeps the direction"
        );
        assert_eq!(
            board.judge("m1", false, "step 3 assumes n odd").unwrap(),
            Judged::Retry { direction: "d1".into() }
        );
        assert_eq!(board.held_by("m1").unwrap().id, "d1", "it still holds its direction");
        board.submit("m1", "d1", &submit("proof two")).unwrap();
        assert_eq!(
            board.judge("m1", false, "still assumes n odd").unwrap(),
            Judged::Dead { direction: "d1".into() }
        );
        let dead = board.direction("d1").unwrap();
        assert_eq!(
            (dead.state, dead.reason.as_str(), dead.verdicts.len()),
            (DirectionState::Dead, "still assumes n odd", 2)
        );
        assert!(board.held_by("m1").is_none(), "it must register another");
        assert!(board.submit("m2", "d1", &submit("parity proof")).is_err(), "d1 is not m2's");
        board.submit("m2", "d2", &submit("parity proof")).unwrap();
        assert_eq!(board.pending_previews()[0]["report"], "parity proof");
        assert_eq!(board.judge("m2", true, "checked").unwrap(), Judged::Accepted { direction: "d2".into() });
        assert_eq!(board.direction("d2").unwrap().state, DirectionState::Succeeded);
        assert_eq!(board.member_ended("m3", "never registered"), None);
    }

    /// The board and a direction's page stay under their bounds: what does not
    /// fit is refused, and nothing already on the board is dropped.
    #[test]
    fn the_board_and_its_pages_are_bounded() {
        let long = |n: usize, c: char| c.to_string().repeat(n);
        let fill = |c: char| {
            let mut board = Board::default();
            for n in 0.. {
                let member = format!("m{n}");
                if let Err(e) = board.register(&member, &format!("{n} {}", long(MAX_DIRECTION_BYTES - 8, c)))
                {
                    return (board, e);
                }
                board.abandon(&member, &long(2000, 'r')).unwrap();
            }
            unreachable!()
        };
        // Plain text: the count is the bound.
        let (board, full) = fill('x');
        assert!(full.message.contains("limit"), "{}", full.message);
        assert_eq!(board.directions().len(), MAX_DIRECTIONS);
        assert!(bytes(&board.overview()) <= MAX_BOARD_BYTES);
        // Text JSON escapes to twice its size: the bytes are.
        let (board, full) = fill('"');
        assert!(full.message.contains("full"), "{}", full.message);
        assert!(board.directions().len() < MAX_DIRECTIONS);
        assert!(bytes(&board.overview()) <= MAX_BOARD_BYTES, "nothing past the bound was kept");

        let mut board = Board::default();
        board.register("m1", "induction").unwrap();
        let mut posted = 0;
        let refusal = loop {
            match board.post("m1", &post(&long(MAX_FINDING_BYTES, 'y'))) {
                Ok(_) => posted += 1,
                Err(e) => break e,
            }
        };
        assert!(posted > 0 && posted < MAX_FINDINGS_PER_DIRECTION, "{posted}");
        assert!(refusal.message.contains("page is full"), "{}", refusal.message);
        assert_eq!(board.direction("d1").unwrap().findings.len(), posted, "nothing earlier was dropped");
        assert!(bytes(&board.page("d1").unwrap()) <= MAX_PAGE_BYTES);
        let mut small = Board::default();
        small.register("m1", "induction").unwrap();
        for _ in 0..MAX_FINDINGS_PER_DIRECTION {
            small.post("m1", &post("short")).unwrap();
        }
        assert!(small.post("m1", &post("one more")).is_err(), "the count is bounded too");
    }
}
