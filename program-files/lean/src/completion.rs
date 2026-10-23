//! `complete`: the completion step of task-scoring protocol v3.
//! The procedure is part of the protocol, so every detail below is
//! deliberate: changing one is a new [`COMPLETION_SUITE_ID`].
//!
//! For each remaining `sorry` of a `sorries` candidate, in order: decide its
//! syntactic position by splicing `by sorry` at it (a parse error means the
//! `sorry` was a tactic, else a term); then for each tactic of
//! [`COMPLETION_SUITE`] splice `tac` (tactic position) or `(by tac)` (term
//! position) at that one `sorry`, re-elaborate the **whole file** under
//! `set_option maxHeartbeats N` written on its blanked first import line
//! (positions stay the candidate's), and call it closed when the reply has
//! no error and exactly one `sorry` fewer. An `exact?` closer is replaced by
//! its `Try this` suggestion and re-checked. Splicing, not tactic mode,
//! because tactic mode misses term-position sorries and the theorem's own
//! `match_N` auxiliaries. When every `sorry` closes, all closers are spliced
//! at once and the file re-checked; the caller passes the result to `verify`
//! — this never calls a solve itself.
//!
//! A candidate that needs more than the cap on its own (the capped baseline
//! re-check reports a heartbeat error) runs uncapped. Failure: a REPL death
//! or the wall-clock backstops ([`COMPLETE_WALL`], [`COMPLETE_REQUEST_WALL`])
//! end the procedure with `error` set and the closers found so far kept.

use std::time::Duration;

use nucleus_lean_interface::{
    COMPLETION_HEARTBEATS, COMPLETION_SUITE, COMPLETION_SUITE_ID, CheckStatus, Closer, CompleteResponse,
    MAX_ANALYSIS_ITEMS, MAX_ANALYSIS_TEXT_CHARS, SorryPosition, TacticAttempt,
};

use crate::repl::CommandReply;
use crate::sketch::{Abort, Driver, Pos, millis, parse_try_this, replace_at};
use crate::{bounded, status_of, without_header_imports};

/// Wall-clock budget of one `complete` request (13 tactics × every `sorry`,
/// each a file re-check).
pub const COMPLETE_WALL: Duration = Duration::from_secs(180);
/// Backstop for one REPL request inside `complete` (a hung re-elaboration);
/// on expiry the REPL is killed and the reply carries `error`.
pub const COMPLETE_REQUEST_WALL: Duration = Duration::from_secs(60);

/// `body` with `set_option maxHeartbeats N` on its first line when that line
/// is blank (the blanked header import), else prepended.
pub fn with_heartbeats(body: &str, heartbeats: u32) -> String {
    let option = format!("set_option maxHeartbeats {heartbeats}");
    match body.split_once('\n') {
        Some((first, rest)) if first.trim().is_empty() => format!("{option}\n{rest}"),
        _ => format!("{option}\n{body}"),
    }
}

fn is_heartbeat(text: &str) -> bool {
    text.contains("heartbeats") || text.contains("deterministic timeout")
}

fn target_of(goal: &str) -> &str {
    goal.rsplit('⊢').next().unwrap_or_default().trim()
}

/// The outcome of one splice-and-recheck: `closed` iff no error and exactly
/// one `sorry` fewer; else which failure.
pub fn classify(reply: &CommandReply, sorries_before: usize, original_goal: &str) -> &'static str {
    let errors: Vec<&str> =
        reply.messages.iter().filter(|m| m.severity == "error").map(|m| m.data.as_str()).collect();
    if errors.is_empty() {
        return if reply.sorries.len() + 1 == sorries_before { "closed" } else { "odd" };
    }
    let joined = errors.join("\n");
    if is_heartbeat(&joined) {
        return "heartbeat_timeout";
    }
    if joined.contains("maximum recursion depth") {
        return "recdepth";
    }
    if let [only] = errors.as_slice()
        && only.starts_with("unsolved goals")
    {
        return if target_of(only) == target_of(original_goal) { "no_progress" } else { "progress" };
    }
    if joined.contains("unexpected token")
        || joined.contains("expected term")
        || joined.contains("expected command")
    {
        return "parse_error";
    }
    "error"
}

fn form(position: SorryPosition, tactic: &str) -> String {
    match position {
        SorryPosition::Tactic => tactic.to_owned(),
        SorryPosition::Term | SorryPosition::Unknown => format!("(by {tactic})"),
    }
}

/// Try to close every remaining `sorry` of `candidate` (the whole file, as
/// the caller holds it). Never fails: whatever stopped the procedure is in
/// the reply's `error`.
pub(crate) fn complete(driver: &mut Driver, mathlib_env: u64, candidate: &str) -> CompleteResponse {
    let mut out = CompleteResponse {
        status: CheckStatus::Sorries,
        closers: vec![],
        all_closed: false,
        spliced: None,
        auto_closable: 0,
        truncated: false,
        suite: COMPLETION_SUITE_ID.to_owned(),
        error: None,
        elapsed_ms: 0,
    };
    if let Err(abort) = run(driver, mathlib_env, candidate, &mut out) {
        out.error = Some(abort.message());
    }
    out.auto_closable = out.closers.iter().filter(|c| c.tactic.is_some()).count() as u64;
    if out.closers.len() > MAX_ANALYSIS_ITEMS {
        out.truncated = true;
        out.closers.truncate(MAX_ANALYSIS_ITEMS);
    }
    out.elapsed_ms = millis(driver.elapsed());
    out
}

fn run(
    driver: &mut Driver,
    mathlib_env: u64,
    candidate: &str,
    out: &mut CompleteResponse,
) -> Result<(), Abort> {
    let body = without_header_imports(candidate);
    let real = driver.command(&body, mathlib_env, false)?;
    out.status = status_of(&real);
    if out.status != CheckStatus::Sorries {
        return Ok(());
    }
    let sorries = real.sorries;
    let n = sorries.len();
    // The candidate itself may need more than the cap: then no cap.
    let baseline = driver.command(&with_heartbeats(&body, COMPLETION_HEARTBEATS), mathlib_env, false)?;
    let cap = if baseline.messages.iter().any(|m| m.severity == "error" && is_heartbeat(&m.data)) {
        None
    } else {
        Some(COMPLETION_HEARTBEATS)
    };
    let check = |driver: &mut Driver, text: &str, capped: bool| -> Result<CommandReply, Abort> {
        let body = without_header_imports(text);
        match cap.filter(|_| capped) {
            Some(hb) => driver.command(&with_heartbeats(&body, hb), mathlib_env, false),
            None => driver.command(&body, mathlib_env, false),
        }
    };
    for s in &sorries {
        let started = driver.elapsed();
        let pos = Pos::of(s.line, s.column);
        let index = out.closers.len();
        out.closers.push(Closer {
            line: s.line,
            column: s.column,
            position: SorryPosition::Unknown,
            tactic: None,
            attempts: vec![],
            elapsed_ms: 0,
        });
        let Some(probe) = replace_at(candidate, pos, "sorry", "by sorry") else { continue };
        let position = if check(driver, &probe, false)?.has_errors() {
            SorryPosition::Tactic
        } else {
            SorryPosition::Term
        };
        if let Some(closer) = out.closers.get_mut(index) {
            closer.position = position;
        }
        for tactic in COMPLETION_SUITE {
            let Some(text) = replace_at(candidate, pos, "sorry", &form(position, tactic)) else { break };
            let t0 = driver.elapsed();
            let reply = check(driver, &text, true)?;
            let mut attempt = TacticAttempt {
                tactic: (*tactic).to_owned(),
                outcome: classify(&reply, n, &s.goal).to_owned(),
                elapsed_ms: 0,
            };
            let mut found = None;
            if attempt.outcome == "closed" {
                if tactic == "exact?" {
                    match parse_try_this(&reply.messages) {
                        Some(suggestion) => {
                            let confirmed = replace_at(candidate, pos, "sorry", &form(position, &suggestion))
                                .map(|text| check(driver, &text, false))
                                .transpose()?
                                .is_some_and(|r| classify(&r, n, &s.goal) == "closed");
                            if confirmed {
                                found = Some(suggestion);
                            } else {
                                attempt.outcome = "closed_but_suggestion_failed".to_owned();
                            }
                        }
                        None => attempt.outcome = "closed_but_no_suggestion".to_owned(),
                    }
                } else {
                    found = Some((*tactic).to_owned());
                }
            }
            attempt.elapsed_ms = millis(driver.elapsed().saturating_sub(t0));
            let Some(closer) = out.closers.get_mut(index) else { break };
            closer.attempts.push(attempt);
            closer.elapsed_ms = millis(driver.elapsed().saturating_sub(started));
            if let Some(tactic) = found {
                closer.tactic = Some(bounded(&tactic, MAX_ANALYSIS_TEXT_CHARS));
                break;
            }
        }
    }
    let closers: Vec<(Pos, SorryPosition, String)> = out
        .closers
        .iter()
        .filter_map(|c| c.tactic.clone().map(|t| (Pos::of(c.line, c.column), c.position, t)))
        .collect();
    if closers.is_empty() || closers.len() != n {
        return Ok(());
    }
    out.all_closed = true;
    let mut spliced = candidate.to_owned();
    let mut ordered = closers;
    ordered.sort_by_key(|(pos, _, _)| std::cmp::Reverse(*pos));
    for (pos, position, tactic) in &ordered {
        spliced = replace_at(&spliced, *pos, "sorry", &form(*position, tactic))
            .ok_or_else(|| Abort::Failed(format!("cannot splice at {}:{}", pos.line, pos.column)))?;
    }
    let recheck = check(driver, &spliced, false)?;
    if status_of(&recheck) == CheckStatus::Complete {
        out.spliced = Some(spliced);
    } else {
        let errors: Vec<String> = recheck
            .messages
            .iter()
            .filter(|m| m.severity == "error")
            .map(|m| m.data.chars().take(160).collect())
            .take(2)
            .collect();
        out.error = Some(format!(
            "the spliced file does not re-check complete: {} sorries left; {}",
            recheck.sorries.len(),
            errors.join("; ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::repl::{Message, Sorry};

    #[test]
    fn the_heartbeat_cap_takes_the_blanked_import_line_and_a_recheck_is_classified() {
        assert_eq!(
            with_heartbeats("\n\ntheorem t : True := by\n  sorry\n", 7),
            "set_option maxHeartbeats 7\n\ntheorem t : True := by\n  sorry\n"
        );
        assert_eq!(
            with_heartbeats("theorem t : True := trivial", 7),
            "set_option maxHeartbeats 7\ntheorem t : True := trivial"
        );
        let error = |data: &str| Message { severity: "error".into(), data: data.into(), line: 1, column: 0 };
        let sorry = Sorry {
            proof_state: 0,
            goal: "h : P\n⊢ Q".into(),
            line: 2,
            column: 2,
            end_line: 2,
            end_column: 7,
        };
        let reply = |messages: Vec<Message>, sorries: usize| CommandReply {
            env: 1,
            messages,
            sorries: vec![sorry.clone(); sorries],
            tactics: vec![],
        };
        assert_eq!(classify(&reply(vec![], 1), 2, "⊢ Q"), "closed");
        assert_eq!(classify(&reply(vec![], 2), 2, "⊢ Q"), "odd");
        assert_eq!(
            classify(&reply(vec![error("unsolved goals\nh : P\n⊢ Q")], 2), 2, "h : P\n⊢ Q"),
            "no_progress"
        );
        assert_eq!(classify(&reply(vec![error("unsolved goals\n⊢ Q'")], 2), 2, "⊢ Q"), "progress");
        assert_eq!(
            classify(
                &reply(
                    vec![error(
                        "(deterministic) timeout at `whnf`, maximum number of heartbeats (100000) has been reached"
                    )],
                    2
                ),
                2,
                "⊢ Q"
            ),
            "heartbeat_timeout"
        );
        assert_eq!(
            classify(&reply(vec![error("unexpected token 'at'; expected term")], 2), 2, "⊢ Q"),
            "parse_error"
        );
        assert_eq!(
            classify(&reply(vec![error("linarith failed to find a contradiction")], 2), 2, "⊢ Q"),
            "error"
        );
    }
}
