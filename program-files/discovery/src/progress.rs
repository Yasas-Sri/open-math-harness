//! One run's progress as its process holds it: which problems
//! have started, which are being explored, what the ended rollouts spent, and
//! the failures seen. Pure: the harness does the spawning, killing and
//! journaling around it, so the rules are testable without a kernel.

use super::*;

/// The run of one process: `queued` until it begins, `running` from its first
/// problem, `done` when its last rollout ends, `failed` when the process
/// cannot carry it on. `stopped` is the service's word, never the run's own.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Progress {
    plan: RunPlan,
    state: RunState,
    /// `plan.problems[..next]` have been started.
    next: usize,
    /// Problems being explored right now.
    active: Vec<String>,
    finished: u64,
    solved: u64,
    /// Of the rollouts that ended; the live ones are added by the caller.
    spend: Spend,
    error: Option<String>,
    /// UNIX milliseconds when the run's first rollout was started.
    started_at_ms: Option<u64>,
    /// Continuations that produced no node, over the run, and the last one's
    /// recorded reason ([`Progress::note_failures`]).
    failed_continuations: u64,
    last_failure: Option<String>,
    /// Every failure seen on the run, grouped ([`failures`]).
    failures: failures::FailureLog,
}

impl Progress {
    /// A run admitted and not begun.
    pub(crate) fn new(plan: RunPlan) -> Self {
        Progress {
            plan,
            state: RunState::Queued,
            next: 0,
            active: Vec::new(),
            finished: 0,
            solved: 0,
            spend: Spend::default(),
            error: None,
            started_at_ms: None,
            failed_continuations: 0,
            last_failure: None,
            failures: failures::FailureLog::default(),
        }
    }

    pub(crate) fn plan(&self) -> &RunPlan {
        &self.plan
    }

    /// The problems started so far: `plan.problems[..started]`.
    pub(crate) fn started(&self) -> u64 {
        self.next as u64
    }

    /// The run ended: `done` or `failed`.
    pub(crate) fn ended(&self) -> bool {
        !matches!(self.state, RunState::Queued | RunState::Running)
    }

    /// The next problem to start, while the run has unstarted problems and
    /// fewer than `concurrency` active. Marks the run `running`.
    pub(crate) fn next_problem(&mut self) -> Option<String> {
        if self.ended() || self.active.len() >= self.plan.concurrency || self.next >= self.plan.problems.len()
        {
            return None;
        }
        self.state = RunState::Running;
        let problem = self.plan.problems[self.next].clone();
        self.next += 1;
        self.active.push(problem.clone());
        Some(problem)
    }

    /// Records when the run began executing, once: the harness's wall clock
    /// at its first [`Self::next_problem`].
    pub(crate) fn note_started(&mut self, now_ms: u64) {
        self.started_at_ms.get_or_insert(now_ms);
    }

    /// A rollout ended on its own; the run is `done` once none is left to
    /// start or finish.
    pub(crate) fn rollout_finished(&mut self, problem: &str, solved: bool, spend: Spend) {
        self.active.retain(|p| p != problem);
        self.finished += 1;
        self.solved += u64::from(solved);
        self.spend = self.spend.plus(spend);
        if self.state == RunState::Running && self.active.is_empty() && self.next >= self.plan.problems.len()
        {
            self.state = RunState::Done;
        }
    }

    /// A rollout was cut short (the run's failure): its spend counts, it
    /// does not count as finished.
    pub(crate) fn rollout_terminated(&mut self, problem: &str, spend: Spend) {
        self.active.retain(|p| p != problem);
        self.spend = self.spend.plus(spend);
    }

    /// The run failed with `why`: its unstarted problems stay unstarted. A
    /// run that already ended is left as it is.
    pub(crate) fn fail(&mut self, why: String) {
        if !self.ended() {
            self.state = RunState::Failed;
            self.error = Some(why);
            self.active.clear();
        }
    }

    /// A round closed with these failed continuations: count them for
    /// `status` and keep the last reason. A continuation the harness itself
    /// cut because the problem was already solved does not count: it is in
    /// the round record, where the audit trail belongs, but an operator
    /// reading `failed_continuations` wants to know what went wrong, and
    /// nothing did.
    pub(crate) fn note_failures(&mut self, problem: &str, failed: &[Failed]) {
        let real: Vec<&Failed> = failed.iter().filter(|f| f.why != CUT_FOR_SOLVE).collect();
        self.failed_continuations += real.len() as u64;
        if let Some(last) = real.last() {
            self.last_failure = Some(last.why.clone());
        }
        for f in real {
            self.failures.note(FailureKind::Worker, problem, &f.why);
        }
    }

    /// One failure of `kind` on `problem`, for the run's failure reasons.
    pub(crate) fn note_failure(&mut self, kind: FailureKind, problem: &str, text: &str) {
        self.failures.note(kind, problem, text);
    }

    /// The run's status; `live` is what its active rollouts have spent so far
    /// (the caller holds them).
    pub(crate) fn status(&self, live: Spend) -> RunStatus {
        let spend = self.spend.plus(live);
        let (failure_reasons, other_failures) = self.failures.report();
        RunStatus {
            run: self.plan.name.clone(),
            state: self.state,
            problems: self.plan.problems.len() as u64,
            finished: self.finished,
            solved: self.solved,
            active: self.active.clone(),
            tokens_in: spend.tokens_in,
            tokens_out: spend.tokens_out,
            usd: spend.usd_micro().map(usd),
            error: self.error.clone(),
            task: TaskShape::of(&self.plan.document),
            failed_continuations: self.failed_continuations,
            last_failure: self.last_failure.clone(),
            started_at_ms: self.started_at_ms,
            failure_reasons,
            other_failures,
        }
    }
}

/// Where each problem of a run stands, in task order, from what its process
/// last reported: started and being explored is `active`, started and not is
/// `done` (its journal closed, or left by a process that ended), not started
/// is `queued` (in a run that ended early it never will be; the run's state
/// says so).
#[must_use]
pub fn problem_states(problems: &[String], started: u64, active: &[String]) -> Vec<(String, ProblemState)> {
    problems
        .iter()
        .enumerate()
        .map(|(i, problem)| {
            let state = if i as u64 >= started {
                ProblemState::Queued
            } else if active.contains(problem) {
                ProblemState::Active
            } else {
                ProblemState::Done
            };
            (problem.clone(), state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{plan, spend};

    #[test]
    fn a_run_explores_its_problems_within_its_concurrency_and_is_done_when_the_last_ends() {
        let mut p = Progress::new(plan("first", &["a", "b", "c"], 2));
        assert_eq!((p.status(Spend::default()).state, p.started()), (RunState::Queued, 0));
        assert_eq!(p.next_problem().as_deref(), Some("a"));
        assert_eq!(p.status(Spend::default()).state, RunState::Running);
        assert_eq!(p.next_problem().as_deref(), Some("b"));
        assert_eq!(p.next_problem(), None, "concurrency 2");
        assert_eq!(p.status(Spend::default()).active, ["a", "b"]);
        p.rollout_finished("a", true, spend(10, 20));
        assert_eq!(p.next_problem().as_deref(), Some("c"));
        assert_eq!((p.next_problem(), p.started()), (None, 3), "nothing left to start");
        p.rollout_finished("b", false, spend(1, 1));
        assert_eq!(p.status(Spend::default()).state, RunState::Running, "c is still active");
        p.rollout_finished("c", false, spend(1, 1));
        assert_eq!(p.status(Spend::default()).state, RunState::Done);
        let status = p.status(Spend::default());
        assert_eq!((status.problems, status.finished, status.solved), (3, 3, 1));
        assert_eq!((status.tokens_in, status.tokens_out, status.usd), (12, 22, Some(5.6e-5)));
        assert!(status.active.is_empty());
    }

    #[test]
    fn a_failure_ends_the_run_and_leaves_its_unstarted_problems_unstarted() {
        let mut p = Progress::new(plan("run", &["a", "b", "c"], 1));
        assert_eq!(p.next_problem().as_deref(), Some("a"));
        // The harness terminates the active rollout (its spend counts; it is
        // not finished) and fails the run.
        p.rollout_terminated("a", spend(5, 5));
        p.fail("journal: disk full".into());
        assert_eq!(p.next_problem(), None, "b and c are never started");
        let status = p.status(Spend::default());
        assert_eq!(
            (status.state, status.finished, status.tokens_in, status.error.as_deref()),
            (RunState::Failed, 0, 5, Some("journal: disk full"))
        );
        assert!(status.active.is_empty());
        p.fail("later".into());
        assert_eq!(p.status(Spend::default()).error.as_deref(), Some("journal: disk full"), "ended once");
        let states: Vec<ProblemState> =
            problem_states(&p.plan().problems, p.started(), &[]).into_iter().map(|(_, s)| s).collect();
        assert_eq!(states, [ProblemState::Done, ProblemState::Queued, ProblemState::Queued]);
    }

    #[test]
    fn status_adds_the_live_spend_and_its_cost_and_groups_what_keeps_failing() {
        let mut p = Progress::new(plan("run", &["a", "b"], 2));
        p.next_problem();
        p.next_problem();
        p.rollout_finished("a", true, spend(10, 20));
        let status = p.status(spend(1, 2));
        assert_eq!(
            status,
            RunStatus {
                run: "run".into(),
                state: RunState::Running,
                problems: 2,
                finished: 1,
                solved: 1,
                active: vec!["b".into()],
                tokens_in: 11,
                tokens_out: 22,
                usd: Some(0.000055),
                error: None,
                task: status.task.clone(),
                failed_continuations: 0,
                last_failure: None,
                started_at_ms: None,
                failure_reasons: vec![],
                other_failures: 0,
            }
        );
        assert_eq!(status.task.concurrency, 2, "the accepted task's shape travels with the status");
        let summary = RunSummary::of("p", &status);
        assert!(serde_json::to_value(&summary).unwrap().get("active").is_none());
        // A continuation cut by a round-mate's solve is not a failure.
        let cut = Failed { task: "t2".into(), why: CUT_FOR_SOLVE.into() };
        p.note_failures("a", &[Failed { task: "t1".into(), why: "timed_out".into() }, cut]);
        p.note_failure(FailureKind::Preflight, "b", "unknown constant `b`");
        let status = p.status(Spend::default());
        assert_eq!((status.failed_continuations, status.last_failure.as_deref()), (1, Some("timed_out")));
        assert_eq!(
            status.failure_reasons.iter().map(|r| (r.kind, r.reason.as_str(), r.count)).collect::<Vec<_>>(),
            [(FailureKind::Worker, "timed_out", 1), (FailureKind::Preflight, "unknown constant `b`", 1)]
        );
    }

    #[test]
    fn problem_states_follow_what_the_process_reported_in_task_order() {
        let problems: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
        let states = |started, active: &[&str]| -> Vec<ProblemState> {
            let active: Vec<String> = active.iter().map(|p| (*p).to_owned()).collect();
            problem_states(&problems, started, &active).into_iter().map(|(_, s)| s).collect()
        };
        let names: Vec<String> = problem_states(&problems, 0, &[]).into_iter().map(|(p, _)| p).collect();
        assert_eq!(names, problems, "task order");
        assert_eq!(states(0, &[]), [ProblemState::Queued; 3], "a queued run");
        assert_eq!(
            states(2, &["a", "b"]),
            [ProblemState::Active, ProblemState::Active, ProblemState::Queued]
        );
        assert_eq!(states(3, &["b", "c"]), [ProblemState::Done, ProblemState::Active, ProblemState::Active]);
        // A run that ended early: what started is done, the rest never starts.
        assert_eq!(states(2, &[]), [ProblemState::Done, ProblemState::Done, ProblemState::Queued]);
    }
}
