//! What keeps going wrong in a run ([`RunStatus::failure_reasons`]): every
//! failure the run's process sees, grouped by where it was seen and by
//! [`failure_reason`] of its text, counted, and reported most frequent first.
//! The process feeds it from the continuations that yielded no node, the Lean
//! verdicts it gets on the run's files, the rollouts that stopped without a
//! solve and the statements Lean refused. It lives in the process's memory,
//! so a resumed run starts counting again.
//!
//! [`RunStatus::failure_reasons`]: nucleus_discovery_interface::RunStatus::failure_reasons

use nucleus_discovery_interface::{
    FailureKind, FailureReason, MAX_FAILURE_KEYS, MAX_FAILURE_PROBLEMS, MAX_FAILURE_REASONS, failure_excerpt,
    failure_reason,
};
use nucleus_lean_interface::{CheckResponse, CheckStatus};

use super::LeanReplies;

/// A run's failures, first seen first.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct FailureLog {
    /// At most [`MAX_FAILURE_KEYS`].
    reasons: Vec<FailureReason>,
    /// Occurrences of reasons first seen after the key cap was reached.
    unkept: u64,
}

impl FailureLog {
    /// Counts one failure of `kind` on `problem`, whose text is `text`
    /// (untrusted: Lean's or a model's words may be in it).
    pub(super) fn note(&mut self, kind: FailureKind, problem: &str, text: &str) {
        let mut reason = failure_reason(text);
        if reason.is_empty() {
            reason = "(no text)".to_owned();
        }
        if let Some(seen) = self.reasons.iter_mut().find(|r| r.kind == kind && r.reason == reason) {
            seen.count = seen.count.saturating_add(1);
            if seen.problems.len() < MAX_FAILURE_PROBLEMS && !seen.problems.iter().any(|p| p == problem) {
                seen.problems.push(problem.to_owned());
            }
            seen.last = failure_excerpt(text);
        } else if self.reasons.len() < MAX_FAILURE_KEYS {
            self.reasons.push(FailureReason {
                kind,
                reason,
                count: 1,
                problems: vec![problem.to_owned()],
                last: failure_excerpt(text),
            });
        } else {
            self.unkept = self.unkept.saturating_add(1);
        }
    }

    /// The [`MAX_FAILURE_REASONS`] most frequent reasons, most frequent
    /// first (first seen first among equals), and how many occurrences the
    /// list leaves out.
    pub(super) fn report(&self) -> (Vec<FailureReason>, u64) {
        let mut ranked: Vec<&FailureReason> = self.reasons.iter().collect();
        ranked.sort_by_key(|r| std::cmp::Reverse(r.count));
        let listed = ranked.iter().take(MAX_FAILURE_REASONS).map(|r| (*r).clone()).collect();
        let other =
            ranked.iter().skip(MAX_FAILURE_REASONS).fold(self.unkept, |sum, r| sum.saturating_add(r.count));
        (listed, other)
    }
}

/// The failures one evaluation of a file shows: Lean's errors in it (the
/// first, which names the rest's cause more often than not) or why it was
/// invalid, a `verify` rejection with its detail, and an `analyze` that did
/// not run to the end. `complete` is opportunistic and not counted.
pub(super) fn of_evaluation(check: &CheckResponse, replies: &LeanReplies) -> Vec<(FailureKind, String)> {
    let mut seen = Vec::new();
    match check.status {
        CheckStatus::Invalid => seen.push((
            FailureKind::LeanCheck,
            check.invalid_reason.clone().unwrap_or_else(|| "invalid".to_owned()),
        )),
        CheckStatus::Errors => {
            if let Some(first) = check.errors.first() {
                seen.push((
                    FailureKind::LeanCheck,
                    format!("line {}, column {}: {}", first.line, first.column, first.message),
                ));
            }
        }
        CheckStatus::Sorries | CheckStatus::Complete => {}
    }
    match &replies.verify {
        Some(Ok(v)) if !v.accepted => seen.push((
            FailureKind::Verify,
            format!(
                "{}: {}",
                v.rejection.as_deref().unwrap_or("rejected"),
                v.detail.as_deref().unwrap_or_default()
            ),
        )),
        Some(Err(e)) => seen.push((FailureKind::Verify, format!("verifier: {:?}: {}", e.code, e.message))),
        _ => {}
    }
    match &replies.analyze {
        Some(Err(e)) => seen.push((FailureKind::LeanCheck, format!("analyze: {:?}: {}", e.code, e.message))),
        Some(Ok(a)) => {
            if let Some(error) = &a.error {
                seen.push((FailureKind::LeanCheck, format!("analyze: {error}")));
            }
        }
        None => {}
    }
    seen
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use nucleus_lean_interface::{Diagnostic, VerifyResponse};
    use nucleus_protocol::{Error, ErrorCode};

    #[test]
    fn failures_group_by_kind_and_reason_and_the_report_keeps_the_most_frequent() {
        let mut log = FailureLog::default();
        // Positions and task ids differ between occurrences of one failure.
        log.note(FailureKind::Worker, "a", "exited_without_submitting: t000001 timed out after 300000 ms");
        log.note(FailureKind::Worker, "b", "exited_without_submitting: t000017 timed out after 300000 ms");
        log.note(FailureKind::Worker, "a", "exited_without_submitting: t000018 timed out after 300000 ms\n");
        // The same text seen elsewhere is another reason.
        log.note(FailureKind::Stop, "a", "exited_without_submitting: t000001 timed out after 300000 ms");
        // Control characters never reach the reason, nor the quoted text.
        log.note(FailureKind::LeanCheck, "c", "line 3, column 7: \u{1b}[31munknown identifier 'x'\r\nmore");
        let (listed, other) = log.report();
        assert_eq!(other, 0);
        assert_eq!(listed.len(), 3);
        let top = &listed[0];
        assert_eq!((top.kind, top.count), (FailureKind::Worker, 3));
        assert_eq!(top.reason, "exited_without_submitting: tN timed out after N ms");
        assert_eq!(top.problems, ["a", "b"], "each problem once, first seen first");
        assert_eq!(top.last, "exited_without_submitting: t000018 timed out after 300000 ms\n", "the latest");
        assert_eq!((listed[1].kind, listed[2].kind), (FailureKind::Stop, FailureKind::LeanCheck));
        assert!(!listed[2].reason.contains('\u{1b}') && !listed[2].last.contains(['\u{1b}', '\r']));
        assert_eq!(listed[2].reason, "line N, column N:  [Nmunknown identifier 'x'");

        // The report lists the top eight; the rest, and whatever arrives
        // after the key cap, is counted in `other`.
        let mut log = FailureLog::default();
        for key in 0..MAX_FAILURE_KEYS + 5 {
            let word: String = (0..=key).map(|_| 'x').collect();
            for _ in 0..=usize::from(key < 2) {
                log.note(FailureKind::Verify, &format!("p{}", key % 10), &word);
            }
        }
        let (listed, other) = log.report();
        assert_eq!(listed.len(), MAX_FAILURE_REASONS);
        assert_eq!((listed[0].reason.as_str(), listed[0].count), ("x", 2));
        assert_eq!((listed[1].reason.as_str(), listed[1].count), ("xx", 2));
        assert_eq!(listed[2].reason, "xxx", "equal counts keep the order they were first seen in");
        let total = 2 * 2 + (MAX_FAILURE_KEYS + 5 - 2) as u64;
        assert_eq!(listed.iter().map(|r| r.count).sum::<u64>() + other, total, "nothing goes uncounted");
        assert_eq!(log.reasons.len(), MAX_FAILURE_KEYS, "a bounded number of keys");
        for _ in 0..MAX_FAILURE_PROBLEMS + 3 {
            log.note(FailureKind::Verify, &format!("q{}", log.reasons[0].problems.len()), "x");
        }
        assert_eq!(log.reasons[0].problems.len(), MAX_FAILURE_PROBLEMS);

        // What one evaluation contributes.
        let check = |status, errors: Vec<Diagnostic>| CheckResponse {
            status,
            invalid_reason: None,
            errors,
            sorries: vec![],
            elapsed_ms: 1,
        };
        let error = Diagnostic { line: 12, column: 4, message: "unsolved goals\n⊢ False".into() };
        let rejected = VerifyResponse {
            accepted: false,
            rejection: Some("non_standard_axioms".into()),
            detail: Some("sorryAx".into()),
            axioms: vec![],
        };
        let replies = LeanReplies {
            verify: Some(Ok(rejected)),
            analyze: Some(Err(Error::new(ErrorCode::Timeout, "analysis ran out of time"))),
            complete: None,
        };
        assert_eq!(
            of_evaluation(&check(CheckStatus::Errors, vec![error]), &LeanReplies::default()),
            [(FailureKind::LeanCheck, "line 12, column 4: unsolved goals\n⊢ False".to_owned())]
        );
        assert_eq!(
            of_evaluation(&check(CheckStatus::Complete, vec![]), &replies),
            [
                (FailureKind::Verify, "non_standard_axioms: sorryAx".to_owned()),
                (FailureKind::LeanCheck, "analyze: Timeout: analysis ran out of time".to_owned())
            ]
        );
        assert!(of_evaluation(&check(CheckStatus::Sorries, vec![]), &LeanReplies::default()).is_empty());
    }
}
