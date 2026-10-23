//! The service's registry of runs: every run it knows — from its output
//! directory when it starts, and from `start` — with what the run's process
//! last reported and where the run stands. `status`, `list`, `problems` and
//! `tree` are answered from here and from the journals on disk, never by a
//! run's process, so they never wait on one. A run is known by its project
//! and its name.

use super::*;
use crate::progress::problem_states;
use crate::wire::LiveRounds;
use nucleus_protocol::DispatchId;

/// Where a run stands in the service.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Phase {
    /// `process` is admitting the run for the call `held` (a `start`, or a
    /// `resume` when `fresh` is false), within `until`. A fresh run is not
    /// shown until it is admitted.
    Admitting { process: ProcessId, held: Option<DispatchId>, until: Instant, fresh: bool },
    /// Admitted, waiting for a running slot; `seq` is its place in line.
    Queued { process: ProcessId, seq: u64 },
    /// Begun: its process explores it.
    Running { process: ProcessId },
    /// Its process ended or was killed (`Some` until the kernel confirms its
    /// subtree's cleanup), or it is known from disk.
    Ended { process: Option<ProcessId> },
    /// A `start` that did not happen: gone, with anything its process
    /// created, once the process is cleaned up.
    Cancelled { process: ProcessId },
}

/// One run the service knows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Run {
    pub(crate) project: String,
    pub(crate) plan: RunPlan,
    pub(crate) status: RunStatus,
    /// The problems started: `plan.problems[..started]`.
    pub(crate) started: u64,
    /// The active problems' rounds their journals do not have yet.
    pub(crate) live: Vec<LiveRounds>,
    pub(crate) phase: Phase,
    /// Its journals may end in a record cut short: its process ended without
    /// its last report, was stopped, or it is from before a restart.
    pub(crate) unclosed: bool,
    /// The `problems` rows of its ended problems, read from the journal once:
    /// a closed journal does not change, and a window polling `problems`
    /// must not have the service re-read every tree of the run each time.
    rows: BTreeMap<String, ProblemRow>,
}

impl Run {
    pub(crate) fn new(project: &str, plan: RunPlan, status: RunStatus, phase: Phase, unclosed: bool) -> Self {
        Run {
            project: project.to_owned(),
            plan,
            status,
            started: 0,
            live: Vec::new(),
            phase,
            unclosed,
            rows: BTreeMap::new(),
        }
    }

    fn is(&self, project: &str, run: &str) -> bool {
        self.project == project && self.plan.name == run
    }

    /// Admitted, queued or running: counted against `--max-queued-runs`.
    pub(crate) fn pending(&self) -> bool {
        matches!(self.phase, Phase::Admitting { .. } | Phase::Queued { .. } | Phase::Running { .. })
    }

    /// Known to callers: not a `start` being admitted or one that did not
    /// happen.
    pub(crate) fn visible(&self) -> bool {
        !matches!(self.phase, Phase::Admitting { fresh: true, .. } | Phase::Cancelled { .. })
    }

    /// The process that holds the run now, if any.
    pub(crate) fn process(&self) -> Option<ProcessId> {
        match self.phase {
            Phase::Admitting { process, .. }
            | Phase::Queued { process, .. }
            | Phase::Running { process }
            | Phase::Cancelled { process } => Some(process),
            Phase::Ended { process } => process,
        }
    }

    /// The run's state from now on, set by the service: its rows are read
    /// again, its live rounds are gone.
    pub(crate) fn admitted(&mut self, status: RunStatus, phase: Phase) {
        self.status = status;
        self.phase = phase;
        self.started = 0;
        self.live.clear();
        self.rows.clear();
        self.unclosed = false;
    }

    /// The run ended in `state` (`stopped`, or `failed` with `error`) with
    /// no word from its process: what was active stays unfinished.
    pub(crate) fn end(&mut self, state: RunState, error: Option<String>, process: Option<ProcessId>) {
        self.status.state = state;
        self.status.error = error;
        self.status.active.clear();
        self.live.clear();
        self.unclosed = true;
        self.phase = Phase::Ended { process };
    }
}

/// The runs, in the order they were started (resumed ones moved last).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Registry {
    runs: Vec<Run>,
}

impl Registry {
    pub(crate) fn get(&self, project: &str, run: &str) -> Option<&Run> {
        self.runs.iter().find(|r| r.is(project, run))
    }

    pub(crate) fn get_mut(&mut self, project: &str, run: &str) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.is(project, run))
    }

    /// The run holding `process`.
    pub(crate) fn of_process(&mut self, process: ProcessId) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.process() == Some(process))
    }

    /// The run whose admission holds the call `dispatch`.
    pub(crate) fn holding(&mut self, dispatch: DispatchId) -> Option<&mut Run> {
        self.runs
            .iter_mut()
            .find(|r| matches!(r.phase, Phase::Admitting { held: Some(d), .. } if d == dispatch))
    }

    pub(crate) fn push(&mut self, run: Run) {
        self.runs.push(run);
    }

    /// Moves the run `process` holds to the end of the order, as a resumed
    /// run is.
    pub(crate) fn move_last(&mut self, process: ProcessId) {
        if let Some(at) = self.runs.iter().position(|r| r.process() == Some(process)) {
            let entry = self.runs.remove(at);
            self.runs.push(entry);
        }
    }

    /// The runs of `project`: each name and where it stands.
    pub(crate) fn of_project(&self, project: &str) -> Vec<(String, Phase)> {
        self.runs
            .iter()
            .filter(|r| r.project == project)
            .map(|r| (r.plan.name.clone(), r.phase.clone()))
            .collect()
    }

    pub(crate) fn remove(&mut self, project: &str, run: &str) -> Option<Run> {
        let at = self.runs.iter().position(|r| r.is(project, run))?;
        Some(self.runs.remove(at))
    }

    /// Whether one more run fits under `max_pending` admitted, queued or
    /// running ones. A full queue is a `ServiceError`: the task is fine, the
    /// service is full, and the caller asks again once a run has ended.
    pub(crate) fn room(&self, max_pending: usize) -> Result<(), Error> {
        let pending = self.runs.iter().filter(|r| r.pending()).count();
        if pending >= max_pending {
            return Err(Error::new(
                ErrorCode::ServiceError,
                format!(
                    "the run queue is full: {pending} run(s) being started, queued or running, the service's \
                     --max-queued-runs; start it again once one has ended"
                ),
            ));
        }
        Ok(())
    }

    /// Runs queued now.
    pub(crate) fn queued(&self) -> usize {
        self.runs.iter().filter(|r| matches!(r.phase, Phase::Queued { .. })).count()
    }

    /// The process of the queued run first in line.
    pub(crate) fn first_queued(&self) -> Option<ProcessId> {
        self.runs
            .iter()
            .filter_map(|r| match r.phase {
                Phase::Queued { process, seq } => Some((seq, process)),
                _ => None,
            })
            .min()
            .map(|(_, process)| process)
    }

    fn visible(&self, project: &str, run: &str) -> Result<&Run, Error> {
        self.get(project, run).filter(|r| r.visible()).ok_or_else(|| invalid(format!("no such run {run:?}")))
    }

    /// `status [project, run]`.
    pub(crate) fn status(&self, resource: &ResourcePath) -> Result<Payload, Error> {
        let (project, run) = run_of(resource)?;
        Payload::encode(&self.visible(project, run)?.status)
    }

    /// `list`: a page of the runs callers know, by project and run name,
    /// from the cursor `after` (`<project>/<run>`): on `[project]` that
    /// project's, on `[]` every project's.
    pub(crate) fn list(&self, resource: &ResourcePath, payload: &Payload) -> Result<Payload, Error> {
        let only = match resource.segments() {
            [] => None,
            [project] => Some(project.as_str()),
            _ => return Err(invalid("list is called on [project], or on [] for every project")),
        };
        let request: ListRequest = payload.decode()?;
        let limit = request.validate()?;
        let after = match request.after.as_deref() {
            None => None,
            Some(after) => Some(
                after.split_once('/').ok_or_else(|| invalid("after is not the next of a list of runs"))?,
            ),
        };
        let mut runs: Vec<&Run> = self
            .runs
            .iter()
            .filter(|r| r.visible() && only.is_none_or(|project| r.project == project))
            .filter(|r| after.is_none_or(|after| (r.project.as_str(), r.plan.name.as_str()) > after))
            .collect();
        runs.sort_by(|a, b| (&a.project, &a.plan.name).cmp(&(&b.project, &b.plan.name)));
        let next = (runs.len() > limit)
            .then(|| format!("{}/{}", runs[limit - 1].project, runs[limit - 1].plan.name));
        let runs = runs.into_iter().take(limit).map(|r| RunSummary::of(&r.project, &r.status)).collect();
        Payload::encode(&ListResult { runs, next })
    }

    /// `problems [project, run]`: one row per problem of the run, in task order — a
    /// queued problem from nothing, an active one from its journal plus the
    /// rounds not yet written, an ended one from its journal (read once,
    /// then remembered, except while the process of a run the service ended
    /// may still be closing it).
    pub(crate) fn problems(&mut self, out: &Path, resource: &ResourcePath) -> Result<Payload, Error> {
        let (project, run) = run_of(resource)?;
        let entry = self
            .runs
            .iter_mut()
            .find(|r| r.is(project, run) && r.visible())
            .ok_or_else(|| invalid(format!("no such run {run:?}")))?;
        let states = problem_states(&entry.plan.problems, entry.started, &entry.status.active);
        let mut problems = Vec::with_capacity(states.len());
        for (problem, state) in states {
            let row = match state {
                ProblemState::Queued => ProblemRow::queued(&problem),
                ProblemState::Done if entry.rows.contains_key(&problem) => entry.rows[&problem].clone(),
                _ => {
                    let row = summarize(&problem, state, &journal_of(out, entry, &problem)?);
                    // A stopped or crashed run's process closes its journals
                    // after the service has ended the run.
                    if state == ProblemState::Done && (!entry.unclosed || entry.process().is_none()) {
                        entry.rows.insert(problem.clone(), row.clone());
                    }
                    row
                }
            };
            problems.push(row);
        }
        Payload::encode(&ProblemsResult { run: run.to_owned(), problems })
    }

    /// `tree [project, run, problem]`: the problem's recorded tree; empty
    /// with state `queued` for a problem not yet started. The policy is the
    /// run's.
    pub(crate) fn tree(&self, out: &Path, resource: &ResourcePath) -> Result<Payload, Error> {
        let (project, run, problem) = match resource.segments() {
            [project, run, problem] => (project.as_str(), run.as_str(), problem.as_str()),
            _ => return Err(invalid("the resource must be [project, run, problem]")),
        };
        let entry = self.visible(project, run)?;
        let state = problem_states(&entry.plan.problems, entry.started, &entry.status.active)
            .into_iter()
            .find(|(p, _)| p == problem)
            .map(|(_, state)| state)
            .ok_or_else(|| invalid(format!("no such problem {problem:?} in run {run:?}")))?;
        if state == ProblemState::Queued {
            return Payload::encode(&TreeResult::queued(run, problem));
        }
        let journal = journal_of(out, entry, problem)?;
        Payload::encode(&tree_of(run, problem, state, &journal, Some(&entry.plan.policy.record())))
    }
}

/// `[project, run]` of a call.
pub(crate) fn run_of(resource: &ResourcePath) -> Result<(&str, &str), Error> {
    match resource.segments() {
        [project, run] => Ok((project.as_str(), run.as_str())),
        _ => Err(invalid("the resource must be [project, run]")),
    }
}

/// A started problem's journal as recorded under the run's directory and,
/// while its rollout is active, the rounds its process holds and has not
/// written yet. A journal may end in a write in progress while its process
/// writes it or when the process ended without closing it; any other torn
/// line is a failed write, reported, never a shorter tree. It is read
/// beneath the service's output directory `out` with no link followed.
fn journal_of(out: &Path, run: &Run, problem: &str) -> Result<ProblemJournal, Error> {
    let active = run.status.active.iter().any(|p| p == problem);
    let live = active || run.unclosed;
    let mut journal = read_problem(out, &run.plan.dir.join(problem), live).map_err(|why| {
        Error::new(ErrorCode::ServiceError, format!("{}/{problem}: journal: {why}", run.plan.name))
    })?;
    if let Some(live) = run.live.iter().find(|l| l.problem == problem) {
        // The process's last report may be older than its journal: a round
        // written since is listed once, from the journal.
        let written = journal.rounds.iter().map(|r| r.round).max();
        journal.rounds.extend(live.rounds.iter().filter(|r| written.is_none_or(|w| r.round > w)).cloned());
    }
    Ok(journal)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::progress::Progress;
    use crate::tests::plan;

    fn queued(name: &str, seq: u64) -> Run {
        let plan = plan(name, &["a", "b"], 1);
        let status = Progress::new(plan.clone()).status(Spend::default());
        Run::new("p", plan, status, Phase::Queued { process: ProcessId(seq + 10), seq }, false)
    }

    #[test]
    fn the_queue_is_bounded_by_the_runs_started_queued_or_running_and_is_first_come_first_served() {
        let mut registry = Registry::default();
        registry.room(1).unwrap();
        registry.push(queued("second", 2));
        registry.push(queued("first", 1));
        assert_eq!(registry.first_queued(), Some(ProcessId(11)), "admitted first");
        let full = registry.room(2).unwrap_err();
        assert_eq!(full.code, ErrorCode::ServiceError, "the task is fine; the service is full");
        assert!(full.message.contains("queue is full: 2 run(s)"), "{}", full.message);
        // Room again once a run ends, however it ends; an ended run is listed still.
        registry.get_mut("p", "first").unwrap().end(RunState::Stopped, None, Some(ProcessId(11)));
        registry.room(2).unwrap();
        assert_eq!(registry.first_queued(), Some(ProcessId(12)));
        let list = |resource: &[&str], request: serde_json::Value| {
            let payload = Payload::encode(&request).unwrap();
            registry.list(&ResourcePath::new(resource.iter().copied()).unwrap(), &payload)
        };
        let page: ListResult = list(&["p"], serde_json::json!({})).unwrap().decode().unwrap();
        let names: Vec<(&str, RunState)> = page.runs.iter().map(|r| (r.run.as_str(), r.state)).collect();
        assert_eq!(names, [("first", RunState::Stopped), ("second", RunState::Queued)], "by name");
        // Page by page, each run once; a page that holds the rest ends the list.
        let first: ListResult = list(&[], serde_json::json!({"limit": 1})).unwrap().decode().unwrap();
        assert_eq!((first.runs[0].run.as_str(), first.next.as_deref()), ("first", Some("p/first")));
        let rest: ListResult =
            list(&[], serde_json::json!({"after": "p/first", "limit": 1})).unwrap().decode().unwrap();
        assert_eq!((rest.runs[0].run.as_str(), rest.next), ("second", None));
        for bad in [
            serde_json::json!({"limit": 0}),
            serde_json::json!({"limit": 201}),
            serde_json::json!({"after": "x"}),
        ] {
            assert_eq!(list(&[], bad).unwrap_err().code, ErrorCode::InvalidInput);
        }
        // A start being admitted counts, and is not shown until admitted.
        let mut fresh = queued("third", 3);
        fresh.phase =
            Phase::Admitting { process: ProcessId(13), held: None, until: Instant::now(), fresh: true };
        registry.push(fresh);
        assert!(registry.room(2).is_err());
        let empty = Payload::encode(&serde_json::json!({})).unwrap();
        assert_eq!(
            registry.list(&ResourcePath::root(), &empty).unwrap().decode::<ListResult>().unwrap().runs.len(),
            2
        );
        let third = ResourcePath::new(["p", "third"]).unwrap();
        assert!(registry.status(&third).unwrap_err().message.contains("no such run"));
        // Another project's runs are not this one's, whatever their names.
        let other: ListResult =
            registry.list(&ResourcePath::new(["q"]).unwrap(), &empty).unwrap().decode().unwrap();
        assert!(other.runs.is_empty());
        let elsewhere = ResourcePath::new(["q", "first"]).unwrap();
        assert!(registry.status(&elsewhere).unwrap_err().message.contains("no such run"));
    }

    /// A round a run's process reported live and has written since is
    /// listed once.
    #[test]
    fn a_live_round_the_journal_has_since_is_listed_once() {
        let mut registry = Registry::default();
        let mut run = queued("live", 1);
        run.plan.dir = crate::tests::scratch("live_rounds");
        run.phase = Phase::Running { process: ProcessId(11) };
        run.started = 1;
        run.status.active = vec!["a".into()];
        let record = |round: u32| {
            serde_json::json!({"v": 1, "round": round, "batch": [0], "children": [], "tokens_in": 0,
                               "tokens_out": 0, "reason": null})
        };
        std::fs::create_dir_all(run.plan.dir.join("a")).unwrap();
        std::fs::write(run.plan.dir.join("a").join(ROUNDS_FILE), format!("{}\n", record(1))).unwrap();
        let live = |round| serde_json::from_value::<RoundRecord>(record(round)).unwrap();
        run.live = vec![LiveRounds { problem: "a".into(), rounds: vec![live(1), live(2)] }];
        let dir = run.plan.dir.clone();
        let out = dir.parent().unwrap().to_owned();
        registry.push(run);
        let tree: TreeResult =
            registry.tree(&out, &ResourcePath::new(["p", "live", "a"]).unwrap()).unwrap().decode().unwrap();
        assert_eq!(tree.rounds.iter().map(|r| r.round).collect::<Vec<_>>(), [1, 2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A stopped run's rows are read again until the kernel confirms its
    /// process gone, which closes the journals after the stop; then they are
    /// remembered.
    #[test]
    fn a_stopped_runs_rows_are_remembered_only_once_its_process_is_gone() {
        let mut registry = Registry::default();
        let mut run = queued("stopped", 1);
        run.plan.dir = crate::tests::scratch("stopped_rows");
        run.started = 1;
        run.end(RunState::Stopped, None, Some(ProcessId(11)));
        let rounds = run.plan.dir.join("a").join(ROUNDS_FILE);
        let out = run.plan.dir.parent().unwrap().to_owned();
        registry.push(run);
        let stop = |registry: &mut Registry| {
            let rows: ProblemsResult = registry
                .problems(&out, &ResourcePath::new(["p", "stopped"]).unwrap())
                .unwrap()
                .decode()
                .unwrap();
            rows.problems[0].stop.clone()
        };
        assert_eq!(stop(&mut registry), None, "the process has not closed the journal yet");
        std::fs::create_dir_all(rounds.parent().unwrap()).unwrap();
        let closing = serde_json::json!({"v": 1, "round": 1, "batch": [0], "children": [], "tokens_in": 0,
                                         "tokens_out": 0, "reason": "terminated"});
        std::fs::write(&rounds, format!("{closing}\n")).unwrap();
        assert_eq!(stop(&mut registry).as_deref(), Some("terminated"));
        registry.get_mut("p", "stopped").unwrap().phase = Phase::Ended { process: None };
        assert_eq!(stop(&mut registry).as_deref(), Some("terminated"));
        std::fs::remove_file(&rounds).unwrap();
        assert_eq!(stop(&mut registry).as_deref(), Some("terminated"), "remembered once the process is gone");
        let _ = std::fs::remove_dir_all(rounds.parent().unwrap().parent().unwrap());
    }
}
