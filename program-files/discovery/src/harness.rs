//! The harness of one run, in the run's own process: the run's rollouts,
//! their policy processes and journals, the workers it spawns, the evaluation
//! of every submission, and the reports that tell the service where the run
//! stands. It serves only its run; the service forwards the run's workers'
//! `explore` calls to it.
//!
//! A kernel request answered `shutting_down` means the service stopped the
//! run: the kernel withdrew this process's authority and will send
//! `Terminate`. It is read as that termination ([`live`]), so nothing is
//! recorded as a failure of the run; the journals are closed as `terminated`.

use super::*;
use crate::progress::Progress;
use crate::wire::{self, LiveRounds, Report};
use crate::workers::{Authority, clone_command, retarget_transcript, worker_program, workspace_dir_in};
use nucleus_protocol::DispatchId;
use nucleus_sdk::Reply;

/// Deadline of a report: the service answers every report at once.
const REPORT_DEADLINE_MS: u64 = 30_000;
/// How long the run's process waits for each exit of the workers it still
/// counts once the run has ended, before it exits without it.
const DRAIN_WAIT: Duration = Duration::from_secs(5);

/// One line of a run's `discovery.jsonl`.
#[derive(Serialize)]
struct Record<'a> {
    problem: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<&'a str>,
    process: Option<u64>,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// What can go wrong while the run's process works: the channel to the
/// kernel (its end, a stop among them), or the run's own failure (a journal
/// that cannot be written, a policy that cannot be started).
#[derive(Debug)]
enum Failure {
    Channel(ChannelError),
    Run(String),
}

impl From<ChannelError> for Failure {
    fn from(e: ChannelError) -> Self {
        Failure::Channel(e)
    }
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Failure::Run(why)
    }
}

type Fallible<T> = Result<T, Failure>;

/// A request's reply, with the kernel's `shutting_down`, which it gives a
/// process whose authority it withdrew (its run was stopped), read as the
/// termination it is.
pub(crate) fn live<T>(reply: Reply<T>) -> Reply<T> {
    match reply {
        Ok(Err(e)) if e.code == ErrorCode::ShuttingDown => Err(ChannelError::Terminated),
        other => other,
    }
}

/// A `lean` call of the run's process: `register` and `statement` on their
/// own capabilities, every other action on the delegable one.
pub(crate) fn lean_call(
    kernel: &mut Kernel,
    authority: &Authority,
    action: &str,
    resource: &ResourcePath,
    request: &Payload,
    deadline_ms: u64,
) -> Result<Result<Payload, Error>, ChannelError> {
    if kernel.terminate_requested() {
        return Err(ChannelError::Terminated);
    }
    let handle = match action {
        lean::ACTION_REGISTER => authority.register,
        lean::ACTION_STATEMENT => authority.statement,
        _ => authority.lean,
    };
    live(kernel.call(handle, action, resource, request, Some(deadline_ms)))
}

/// Sends `report` to the service on the run's own resource, `[key]`.
pub(crate) fn send_report(
    kernel: &mut Kernel,
    authority: &Authority,
    report: &Report,
) -> Result<Result<(), Error>, ChannelError> {
    let (payload, resource) = match (wire::encode(report), ResourcePath::new([authority.key.as_str()])) {
        (Ok(payload), Ok(resource)) => (payload, resource),
        (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
    };
    Ok(live(kernel.call(
        authority.report,
        wire::ACTION_REPORT,
        &resource,
        &payload,
        Some(REPORT_DEADLINE_MS),
    ))?
    .map(drop))
}

/// A worker the harness spawned and has not yet seen exiting.
struct WorkerRef {
    problem: String,
    task: String,
}

/// Proof search verifies the submitted file itself; only terminal collection
/// may use the scorer's automatic completion of remaining holes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EvaluationMode {
    Root,
    ProofSearch,
    Collection,
}

/// One problem being explored: its rollout, its policy process, its journal.
pub(crate) struct ActiveRollout {
    pub(crate) run: String,
    /// The run's workers' directory, bound at every worker's `/out`.
    pub(crate) workers_dir: PathBuf,
    pub(crate) rollout: Rollout,
    pub(crate) policy: RolloutPolicy,
    pub(crate) journal: Journal,
    pub(crate) identity: PolicyRecord,
    pub(crate) started: Instant,
    /// The last round's record, held back until the loop knows whether it
    /// was the last, so the stop reason lands on it.
    pub(crate) pending: Option<RoundRecord>,
    /// A round has been planned and not yet finished.
    pub(crate) in_round: bool,
    /// When the open round's wall clock runs out ([`Options::round_timeout`]).
    pub(crate) round_deadline: Option<Instant>,
    /// Until when the next round waits after a round lost to the provider.
    pub(crate) backoff_until: Option<Instant>,
    /// Rounds retried after a provider failure so far (bounded by
    /// [`Options::provider_retries`]).
    pub(crate) provider_retries: u32,
    /// The parents of the round lost to the provider, run again once its
    /// backoff has passed.
    pub(crate) provider_retry_batch: Option<Vec<NodeId>>,
}

impl ActiveRollout {
    /// The next round's batch: after a round lost to the provider, the same
    /// parents again ([`Rollout::retry_round`]); otherwise the policy's.
    pub(crate) fn plan_next(&mut self) -> Result<Vec<NodeId>, Stop> {
        match self.provider_retry_batch.take() {
            Some(parents) => self.rollout.retry_round(&parents),
            None => self.rollout.plan_round_with(&mut self.policy),
        }
    }

    /// Closes the open round, if any, and prepares its record; the caller
    /// deals with stragglers and with what the round's outcome means.
    pub(crate) fn finish_round(&mut self) -> Result<RoundEnd, String> {
        let end = self.rollout.finish_round();
        self.in_round = false;
        if let Some(previous) = self.pending.take() {
            self.journal.round(&previous)?;
        }
        self.pending = Some(RoundRecord::from_end(&end, &self.identity));
        Ok(end)
    }

    /// The rounds the journal does not have yet: the completed round held
    /// back for its stop reason, then the open one. Appended to the records
    /// on disk, they make the live tree `discovery tree` reports.
    pub(crate) fn live_rounds(&self) -> Vec<RoundRecord> {
        let mut rounds: Vec<RoundRecord> = self.pending.iter().cloned().collect();
        if self.in_round {
            rounds.push(self.rollout.open_round_record(&self.identity));
        }
        rounds
    }

    /// Writes the last record and reports whether the tree holds a proof.
    pub(crate) fn close(&mut self, stop: &Stop) -> Result<bool, String> {
        let last = self.pending.take().unwrap_or_else(|| RoundRecord::empty(0, &self.identity)).closed(stop);
        self.journal.round(&last)?;
        Ok(self.rollout.tree().best() >= SOLVED)
    }

    /// The run's process cannot carry this rollout on (`why`): its open
    /// round, if any, is closed as it stands and the last record says
    /// `failed: why`, so the journal does not end mid-round. A journal that
    /// cannot take even that is reported, not propagated — the rollout is
    /// being given up either way. Returns the spend to account for.
    pub(crate) fn abandon(&mut self, why: &str) -> Spend {
        let recorded = if self.in_round { self.finish_round().map(drop) } else { Ok(()) }
            .and_then(|()| self.close(&Stop::Failed(why.to_owned())).map(drop));
        if let Err(journal) = recorded {
            eprintln!(
                "nucleus-discovery: {}/{}: cannot record the failure ({why}): {journal}",
                self.run,
                self.rollout.problem()
            );
        }
        self.rollout.spent()
    }
}

pub(crate) type Kernel = Connection<std::io::Stdin, std::io::Stdout>;

/// What the admission of a run hands its harness.
pub(crate) struct Admitted {
    pub(crate) progress: Progress,
    /// The saved problems a resumed run continues, by problem.
    pub(crate) saved: BTreeMap<String, resume::Problem>,
    /// The highest task number the run's earlier executions used.
    pub(crate) next_task: u64,
    /// The run's lease, held for the life of this process.
    pub(crate) lease: File,
    /// A resume's model calls whose outcome its checkpoint cannot tell.
    pub(crate) uncertain_model_calls: u64,
}

pub(crate) struct Harness<'a> {
    kernel: Kernel,
    authority: Authority,
    options: &'a Options,
    progress: Progress,
    saved: BTreeMap<String, resume::Problem>,
    _lease: File,
    tasks: TaskIds,
    /// The run's rollouts, by problem.
    active: BTreeMap<String, ActiveRollout>,
    /// Every worker not yet seen exiting. The kernel's `ChildExited` for a
    /// worker that submitted late in a round arrives after the round —
    /// sometimes after the problem — is over, when the rollout no longer
    /// knows it; this keeps the `exited` record attributed to the right task.
    workers: BTreeMap<ProcessId, WorkerRef>,
    /// The last report the service took.
    reported: Option<Report>,
}

impl admit::LeanService for Harness<'_> {
    fn call(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        request: &Payload,
        deadline_ms: u64,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        lean_call(&mut self.kernel, &self.authority, action, resource, request, deadline_ms)
    }
}

impl<'a> Harness<'a> {
    pub(crate) fn new(
        kernel: Kernel,
        authority: Authority,
        options: &'a Options,
        admitted: Admitted,
    ) -> Self {
        Harness {
            kernel,
            authority,
            options,
            progress: admitted.progress,
            saved: admitted.saved,
            _lease: admitted.lease,
            tasks: TaskIds { next: admitted.next_task },
            active: BTreeMap::new(),
            workers: BTreeMap::new(),
            reported: None,
        }
    }

    fn run(&self) -> &str {
        &self.progress.plan().name
    }

    /// Appends one event line to the run's `discovery.jsonl`.
    fn log(&self, r: &Record) -> Result<(), String> {
        let path = self.progress.plan().dir.join(EVENTS_FILE);
        let mut file =
            OpenOptions::new().append(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        append(&mut file, r)
    }

    /// The task-scoring protocol (module doc): `check`; then `verify` for a
    /// complete file; for a partial one nothing during proof search, and
    /// otherwise `complete` (a `final_offer` submission only, never the
    /// root), `verify` on its splice when every goal closed, and `analyze`
    /// unless that made it a solve. `Err` is a channel failure;
    /// `Ok(Err)` a failed `check` call (no score). A later call that fails
    /// is recorded on the evaluation.
    fn evaluate(
        &mut self,
        problem: &str,
        statement: &str,
        file: &str,
        mode: EvaluationMode,
    ) -> Result<Result<Evaluation, Error>, ChannelError> {
        // Every call names the run's registration of the statement, `[key,
        // problem]`, and carries the candidate alone (module doc, "The
        // loop", 1).
        let resource = match ResourcePath::new([self.authority.key.as_str(), problem]) {
            Ok(r) => r,
            Err(e) => return Ok(Err(invalid(format!("problem {problem:?}: {}", e.message)))),
        };
        let request = CheckRequest { candidate: file.to_owned() };
        let check = match admit::judge::<CheckResponse>(
            self,
            lean::ACTION_CHECK,
            &resource,
            statement,
            &request,
            EVAL_DEADLINE_MS,
        )? {
            Ok(check) => check,
            Err(e) => {
                self.note_lean_failures(problem, &[(FailureKind::LeanCheck, lean_error("check", &e))]);
                return Ok(Err(e));
            }
        };
        let mut replies = LeanReplies::default();
        match check.status {
            CheckStatus::Complete => {
                let request = VerifyRequest { candidate: file.to_owned() };
                replies.verify = Some(admit::judge::<VerifyResponse>(
                    self,
                    lean::ACTION_VERIFY,
                    &resource,
                    statement,
                    &request,
                    EVAL_DEADLINE_MS,
                )?);
            }
            CheckStatus::Sorries if mode != EvaluationMode::ProofSearch => {
                let completed = if mode == EvaluationMode::Root {
                    None
                } else {
                    let request = CompleteRequest { candidate: file.to_owned() };
                    Some(admit::judge::<CompleteResponse>(
                        self,
                        lean::ACTION_COMPLETE,
                        &resource,
                        statement,
                        &request,
                        COMPLETE_DEADLINE_MS,
                    )?)
                };
                if let Some(Ok(c)) = &completed
                    && c.all_closed
                    && let Some(spliced) = &c.spliced
                {
                    let request = VerifyRequest { candidate: spliced.clone() };
                    replies.verify = Some(admit::judge::<VerifyResponse>(
                        self,
                        lean::ACTION_VERIFY,
                        &resource,
                        statement,
                        &request,
                        EVAL_DEADLINE_MS,
                    )?);
                }
                replies.complete = completed;
                if !matches!(&replies.verify, Some(Ok(v)) if v.accepted) {
                    let request = AnalyzeRequest { candidate: file.to_owned() };
                    replies.analyze = Some(admit::judge::<AnalyzeResponse>(
                        self,
                        lean::ACTION_ANALYZE,
                        &resource,
                        statement,
                        &request,
                        ANALYZE_DEADLINE_MS,
                    )?);
                }
            }
            CheckStatus::Sorries | CheckStatus::Errors | CheckStatus::Invalid => {}
        }
        let seen = failures::of_evaluation(&check, &replies);
        self.note_lean_failures(problem, &seen);
        Ok(Ok(Evaluation::combine(file, statement, check, replies)))
    }

    /// Counts what an evaluation showed in the run's failure reasons, unless
    /// the run is being stopped (its calls then fail for that reason alone).
    fn note_lean_failures(&mut self, problem: &str, seen: &[(FailureKind, String)]) {
        if self.kernel.terminate_requested() {
            return;
        }
        for (kind, text) in seen {
            self.progress.note_failure(*kind, problem, text);
        }
    }

    /// Counts a rollout's stop in the run's failure reasons, unless it is a
    /// solve or the run's own end (`terminated`).
    fn note_stop(&mut self, problem: &str, stop: &Stop) {
        match stop {
            Stop::Solved | Stop::Terminated => {}
            Stop::StatementRejected(lean) => {
                self.progress.note_failure(FailureKind::Preflight, problem, lean)
            }
            _ => {
                let text = match stop.policy_error() {
                    Some(error) => format!("{}: {error}", stop.reason()),
                    None => stop.reason(),
                };
                self.progress.note_failure(FailureKind::Stop, problem, &text);
            }
        }
    }

    /// The stragglers that are still alive: `finish_round` reports a worker as
    /// lingering when its exit has not been *read* yet, but a worker that
    /// submitted normally exits at once, and its `ChildExited` is usually still
    /// unread (queued behind the Lean run that evaluated its submission). The
    /// kernel's own listing of this process's live children is the authority
    /// on who is really still running; only those are killed. A failed listing
    /// falls back to the given list (every straggler), never to skipping.
    fn lingering(&mut self, stragglers: &[ProcessId]) -> Result<Vec<ProcessId>, ChannelError> {
        if stragglers.is_empty() {
            return Ok(Vec::new());
        }
        let live: Option<BTreeSet<ProcessId>> =
            live(self.kernel.children(1))?.ok().map(|list| list.into_iter().map(|c| c.process).collect());
        Ok(stragglers.iter().copied().filter(|p| live.as_ref().is_none_or(|set| set.contains(p))).collect())
    }

    /// Kills the live ones among `processes` (workers of the run) and records
    /// it. Nothing is left for them to do. The kernel refuses the kill only if
    /// the worker is gone after all (ids are never reused); that is recorded as
    /// what it is, not as a kill.
    fn kill_live(&mut self, processes: &[ProcessId]) -> Fallible<()> {
        for process in self.lingering(processes)? {
            let killed = live(self.kernel.kill(process))?;
            let (problem, task) = self
                .workers
                .get(&process)
                .map_or(("", None), |w| (w.problem.as_str(), Some(w.task.as_str())));
            self.log(&Record {
                problem,
                task,
                process: Some(process.0),
                event: if killed.is_ok() { "killed" } else { "kill_refused" },
                error: killed.err().map(|e| format!("{:?}: {}", e.code, e.message)),
            })?;
        }
        Ok(())
    }

    // ---- the loop ----

    /// Does everything that can be done without waiting for an event: steps
    /// every rollout that is between rounds and starts rollouts while there
    /// is room. The run's own failure ends it `failed`; a channel failure is
    /// returned.
    fn advance(&mut self) -> Result<(), ChannelError> {
        loop {
            if self.kernel.terminate_requested() || self.progress.ended() {
                return Ok(());
            }
            match self.progress_once() {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(Failure::Channel(e)) => return Err(e),
                Err(Failure::Run(why)) => {
                    eprintln!("nucleus-discovery: run {} failed: {why}", self.run());
                    self.fail(why)?;
                }
            }
        }
    }

    /// One pass over the run; whether anything happened.
    fn progress_once(&mut self) -> Fallible<bool> {
        let mut moved = false;
        let now = Instant::now();
        let ready: Vec<String> = self
            .active
            .iter()
            .filter(|(_, a)| a.rollout.round_done() && a.backoff_until.is_none_or(|until| until <= now))
            .map(|(p, _)| p.clone())
            .collect();
        for problem in ready {
            self.step(&problem)?;
            moved = true;
        }
        while let Some(problem) = self.progress.next_problem() {
            self.progress.note_started(unix_ms());
            self.start_rollout(&problem)?;
            moved = true;
        }
        Ok(moved)
    }

    /// Starts one problem's rollout: the policy process, the journal, the
    /// root. A setup that fails at the kernel's hands (no statement, root not
    /// checkable) is a failed rollout with a round-0 record; one that fails at
    /// this process's hands (policy, journal) fails the run.
    fn start_rollout(&mut self, problem: &str) -> Fallible<()> {
        let plan = self.progress.plan();
        let supplied = plan.statements.get(problem).cloned();
        let (run, dir, workers_dir, source, mut limits, template) = (
            plan.name.clone(),
            plan.dir.clone(),
            plan.workers_dir.clone(),
            plan.policy.clone(),
            plan.limits,
            plan.prompt_template.clone(),
        );
        limits.collection_calls = if self.authority.shell.is_some() { explore::COLLECTION_CALLS } else { 0 };
        let started = Instant::now();
        let identity = source.record();
        // One policy process per rollout, started before the journal so that a
        // program that cannot be started leaves no half-written problem behind.
        let policy = RolloutPolicy::start(&source, &dir)?;
        let saved = self.saved.remove(problem);
        let mut journal = if saved.is_some() {
            resume::journal(&dir.join(problem))?
        } else {
            Journal::open(&dir.join(problem))?
        };
        let (statement, root_eval) =
            match self.root_of(problem, supplied, saved.as_ref().and_then(|s| s.nodes.first()))? {
                Ok(root) => root,
                Err(stop) => {
                    self.note_stop(problem, &stop);
                    journal.round(&RoundRecord::empty(0, &identity).closed(&stop))?;
                    self.progress.rollout_finished(problem, false, Spend::default());
                    return Ok(());
                }
            };
        let mut rollout = Rollout::new(problem, statement.clone(), template, root_eval, limits);
        if let Some(saved) = saved {
            // An interrupted restoration is not mistaken for the old closed
            // execution. No worker can start before this marker is durable.
            journal.round(&RoundRecord::empty(saved.round, &identity))?;
            journal.rounds.sync_data().map_err(|e| e.to_string())?;
            for node in saved.nodes.iter().skip(1) {
                let eval = resume::evaluation(node);
                let parent =
                    usize::try_from(node.parent.ok_or_else(|| "resume node has no parent".to_owned())?)
                        .map_err(|e| e.to_string())?;
                let task = node.task.clone().ok_or_else(|| "resume node has no task".to_owned())?;
                rollout
                    .tree
                    .push(parent, node.round, task, node.file.clone(), eval, node.worker.clone())
                    .ok_or_else(|| "resume node has an invalid parent".to_owned())?;
            }
            rollout.spent = saved.spend;
            rollout.round = saved.round;
            // A resume explicitly opens a new bounded policy execution,
            // while cumulative model, token and monetary budgets remain.
            rollout.retried = saved.round;
        } else if let Some(root) = rollout.node_record(0, elapsed_ms(started)) {
            journal.node(&root)?;
        }
        self.active.insert(
            problem.to_owned(),
            ActiveRollout {
                run,
                workers_dir,
                rollout,
                policy,
                journal,
                identity,
                started,
                pending: None,
                in_round: false,
                round_deadline: None,
                backoff_until: None,
                provider_retries: 0,
                provider_retry_batch: None,
            },
        );
        Ok(())
    }

    /// The current frozen statement — the task's own (`supplied`, as
    /// recorded in `task.json`) or the `lean` service's — registered for
    /// `[key, problem]`, with either its saved evaluation on resume or a
    /// fresh evaluation for a new run. Saved scores are search progress, not
    /// new Lean verdicts. `Err` is the rollout's stop: `statement_rejected`
    /// when Lean refused the statement, `failed: …` otherwise.
    fn root_of(
        &mut self,
        problem: &str,
        supplied: Option<String>,
        saved: Option<&NodeRecord>,
    ) -> Result<Result<(String, Evaluation), Stop>, ChannelError> {
        let failed = |why: String| Ok(Err(Stop::Failed(why)));
        let (benchmark, registration) =
            match (ResourcePath::new([problem]), ResourcePath::new([self.authority.key.as_str(), problem])) {
                (Ok(benchmark), Ok(registration)) => (benchmark, registration),
                (Err(e), _) | (_, Err(e)) => return failed(format!("invalid problem id: {}", e.message)),
            };
        let statement = match supplied {
            Some(source) => source,
            None => match admit::statement(self, &benchmark, EVAL_DEADLINE_MS)? {
                Ok(s) => s.statement,
                Err(e) => return failed(format!("statement: {:?}: {}", e.code, e.message)),
            },
        };
        match admit::register_statement(self, &registration, &statement, admit::STATEMENT_DEADLINE_MS)? {
            Ok(registered) if registered.accepted => {}
            Ok(refused) => return Ok(Err(Stop::StatementRejected(refused.lean_output.unwrap_or_default()))),
            Err(e) => return failed(format!("register: {:?}: {}", e.code, e.message)),
        }
        if let Some(node) = saved {
            if node.file != statement {
                return failed("resume statement differs from the current frozen statement".to_owned());
            }
            return Ok(Ok((statement, resume::evaluation(node))));
        }
        let root_eval = match self.evaluate(problem, &statement, &statement, EvaluationMode::Root)? {
            Ok(eval) => eval,
            Err(e) => return failed(format!("root check: {:?}: {}", e.code, e.message)),
        };
        Ok(Ok((statement, root_eval)))
    }

    /// Moves a rollout that is between rounds: closes the round that just
    /// completed (killing stragglers, recording it), then plans the next one
    /// and spawns its workers, or closes the rollout with its stop reason.
    /// The rollout leaves `active` only closed: on a failure of the process's
    /// own (a journal write, an unaccountable worker) it is abandoned with
    /// that failure on its last record and its spend counted, and the error
    /// then fails the run. When the run is being stopped it stays active, for
    /// the termination to close as `terminated`.
    fn step(&mut self, problem: &str) -> Fallible<()> {
        let Some(mut a) = self.active.remove(problem) else { return Ok(()) };
        match self.advance_rollout(&mut a, problem) {
            Ok(None) => {
                self.active.insert(problem.to_owned(), a);
                Ok(())
            }
            Ok(Some(stop)) => self.close(a, problem, &stop),
            Err(Failure::Channel(ChannelError::Terminated)) => {
                self.active.insert(problem.to_owned(), a);
                Err(Failure::Channel(ChannelError::Terminated))
            }
            Err(failure) => {
                let why = match &failure {
                    Failure::Run(why) => why.clone(),
                    Failure::Channel(e) => format!("kernel channel: {e}"),
                };
                let spend = a.abandon(&why);
                self.progress.rollout_terminated(problem, spend);
                Err(failure)
            }
        }
    }

    /// The body of [`Harness::step`]: `Some(stop)` closes the rollout, `None`
    /// leaves it in its next round.
    fn advance_rollout(&mut self, a: &mut ActiveRollout, problem: &str) -> Fallible<Option<Stop>> {
        if a.in_round {
            let end = a.finish_round()?;
            a.round_deadline = None;
            self.progress.note_failures(problem, &end.log.failed);
            self.kill_live(&end.stragglers)?;
            if provider_failure_round(&end.log) {
                if a.provider_retries >= self.options.provider_retries {
                    return Ok(Some(Stop::ProviderUnavailable));
                }
                let backoff = Duration::from_millis(
                    self.options.provider_backoff_ms.saturating_mul(1u64 << a.provider_retries.min(16)),
                );
                a.provider_retries += 1;
                a.rollout.note_provider_retry();
                a.provider_retry_batch = Some(end.log.batch.clone());
                a.backoff_until = Some(Instant::now() + backoff);
                eprintln!(
                    "nucleus-discovery: run {} {problem}: every continuation of round {} failed at the provider; \
                     retry {} of {} in {} s",
                    a.run,
                    end.round,
                    a.provider_retries,
                    self.options.provider_retries,
                    backoff.as_secs()
                );
                return Ok(None);
            }
            if end.log.children.is_empty() {
                return Ok(Some(Stop::NoChildren));
            }
        }
        if let Some(until) = a.backoff_until {
            if Instant::now() < until {
                return Ok(None);
            }
            a.backoff_until = None;
        }
        match a.plan_next() {
            Ok(batch) => {
                self.spawn_batch(a, problem, &batch)?;
                a.in_round = true;
                a.round_deadline = Some(Instant::now() + self.options.round_timeout(&a.rollout.limits()));
                Ok(None)
            }
            Err(stop) => Ok(Some(stop)),
        }
    }

    /// Ends a rollout: its last record, its count in the run. Dropping it
    /// kills its policy process.
    fn close(&mut self, mut a: ActiveRollout, problem: &str, stop: &Stop) -> Fallible<()> {
        let solved = a.close(stop)?;
        self.note_stop(problem, stop);
        self.progress.rollout_finished(problem, solved, a.rollout.spent());
        Ok(())
    }

    /// The earliest round deadline among the active rollouts, if any round is open.
    fn next_deadline(&self) -> Option<Instant> {
        self.active.values().flat_map(|a| [a.round_deadline, a.backoff_until]).flatten().min()
    }

    /// Rounds whose wall clock ran out: their unsubmitted continuations fail
    /// as `timed_out` and their workers are killed (module doc, "The loop",
    /// 4); [`Harness::advance`] then closes each such round like any other.
    fn expire_rounds(&mut self) -> Result<(), ChannelError> {
        let now = Instant::now();
        let expired: Vec<String> = self
            .active
            .iter()
            .filter(|(_, a)| a.round_deadline.is_some_and(|deadline| deadline <= now))
            .map(|(p, _)| p.clone())
            .collect();
        for problem in expired {
            let Some(mut a) = self.active.remove(&problem) else { continue };
            let late = a.rollout.time_out();
            a.round_deadline = None;
            let killed = self.kill_live(&late);
            self.active.insert(problem, a);
            match killed {
                Ok(()) => {}
                Err(Failure::Channel(e)) => return Err(e),
                Err(Failure::Run(why)) => {
                    eprintln!("nucleus-discovery: run {} failed: {why}", self.run());
                    self.fail(why)?;
                }
            }
        }
        Ok(())
    }

    /// What a continuation of `parent` starts from: when the parent's worker
    /// left a workspace and a transcript, the workspace is copied at the
    /// shell service ([`clone_command`], run in the child's workspace with
    /// this process's own authority on the run's workspaces) and the
    /// transcript copied into the child's output directory as `parent.jsonl`;
    /// the child's assignment then names it. A root parent, or one whose
    /// worker left nothing (the fixed loop, a crash), gives the child a fresh
    /// start on the parent's file — recorded, not hidden.
    ///
    /// The transcript is not copied verbatim: every mention of the parent's
    /// workspace directory is rewritten to the child's
    /// ([`retarget_transcript`]): a parent writes its own absolute workspace
    /// path into its shell commands, and a child that reads them as its own
    /// history goes on working in its parent's directory, changing the
    /// parent's files and returning its own untouched.
    fn prepare_workspace(
        &mut self,
        a: &ActiveRollout,
        problem: &str,
        parent: NodeId,
        task: &str,
    ) -> Fallible<Option<String>> {
        let Some(shell) = self.authority.shell else { return Ok(None) };
        let Some(parent_task) = a.rollout.tree.node(parent).and_then(|n| n.task.clone()) else {
            return Ok(None);
        };
        let source = a.workers_dir.join(problem).join(&parent_task).join(WORKER_TRANSCRIPT);
        if !source.is_file() {
            return Ok(None);
        }
        let resource = self.authority.workspace(problem, task)?;
        let request = shell::ExecRequest {
            command: clone_command(&parent_task),
            stdin: None,
            timeout_ms: Some(EVAL_DEADLINE_MS),
            max_output_bytes: Some(4096),
            ..Default::default()
        };
        let copied =
            live(self.kernel.call(shell, shell::ACTION_EXEC, &resource, &request, Some(EVAL_DEADLINE_MS)))?
                .and_then(|p| p.decode::<shell::ExecResponse>())
                .and_then(|done| {
                    let head = || done.output.chars().take(300).collect::<String>();
                    if done.exit_code != Some(0) {
                        return Err(Error::new(
                            ErrorCode::ServiceError,
                            format!("cp exited {:?}: {}", done.exit_code, head()),
                        ));
                    }
                    workspace_dir_in(&done.output).map(str::to_owned).ok_or_else(|| {
                        Error::new(
                            ErrorCode::ServiceError,
                            format!("the copy printed no usable workspace path: {}", head()),
                        )
                    })
                });
        match copied {
            Ok(child_dir) => {
                let text =
                    std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
                let target_dir = a.workers_dir.join(problem).join(task);
                std::fs::create_dir_all(&target_dir).map_err(|e| format!("{}: {e}", target_dir.display()))?;
                let target = target_dir.join(WORKER_PARENT_TRANSCRIPT);
                let retargeted = retarget_transcript(&text, &child_dir, &parent_task);
                std::fs::write(&target, retargeted).map_err(|e| format!("{}: {e}", target.display()))?;
                Ok(Some(format!("{WORKER_OUT}/{problem}/{task}/{WORKER_PARENT_TRANSCRIPT}")))
            }
            Err(e) => {
                // A workspace that cannot be cloned — or whose directory the
                // copy did not name, leaving the transcript pointing at the
                // parent's — is not continued from a transcript that assumes
                // it: the child starts fresh on the parent's file, and the
                // reason is on the run's record.
                self.log(&Record {
                    problem,
                    task: Some(task),
                    process: None,
                    event: "workspace_not_cloned",
                    error: Some(format!("{:?}: {}", e.code, e.message)),
                })?;
                Ok(None)
            }
        }
    }

    fn spawn_batch(&mut self, a: &mut ActiveRollout, problem: &str, batch: &[NodeId]) -> Fallible<()> {
        let calls = a.rollout.worker_call_limit();
        for &parent in batch {
            let task = self.tasks.mint();
            let grants = self.authority.grants_for(problem, &task, calls, a.rollout.coordinates())?;
            let parent_transcript = if self.authority.shell.is_some() {
                self.prepare_workspace(a, problem, parent, &task)?
            } else {
                None
            };
            let program =
                worker_program(self.options, &a.workers_dir, problem, &task).map_err(|e| e.message)?;
            // Retain the exact reservation, collection calls included, before
            // the worker can spend it: a resume charges a worker that left no
            // whole receipt this reservation.
            let directory = a.workers_dir.join(problem).join(&task);
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let mut reservation = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join("reservation.json"))
                .map_err(|e| e.to_string())?;
            writeln!(reservation, "{}", serde_json::json!({"v":1,"model_calls":calls}))
                .map_err(|e| e.to_string())?;
            reservation.sync_all().map_err(|e| e.to_string())?;
            match live(self.kernel.spawn_executable(self.authority.spawn, program, grants))? {
                Ok(child) => {
                    self.log(&Record {
                        problem,
                        task: Some(&task),
                        process: Some(child.0),
                        event: "spawned",
                        error: None,
                    })?;
                    self.workers.insert(child, WorkerRef { problem: problem.to_owned(), task: task.clone() });
                    a.rollout.open_task_with(&task, parent, child, parent_transcript)?;
                }
                Err(e) => {
                    // The kernel's refusal is authoritative; the continuation fails.
                    let why = format!("{:?}: {}", e.code, e.message);
                    self.log(&Record {
                        problem,
                        task: Some(&task),
                        process: None,
                        event: "spawn_refused",
                        error: Some(why.clone()),
                    })?;
                    a.rollout.fail_task(&task, format!("spawn_refused: {why}"));
                }
            }
        }
        Ok(())
    }

    /// The run cannot be carried on (`why`): every active rollout gets
    /// `terminated` on its record, the run's live workers are killed, its
    /// unstarted problems stay unstarted, and it is `failed`. Journal
    /// failures here are reported, not propagated: the run is ending anyway.
    fn fail(&mut self, why: String) -> Result<(), ChannelError> {
        if self.progress.ended() {
            return Ok(());
        }
        for (problem, mut a) in std::mem::take(&mut self.active) {
            let recorded = if a.in_round { a.finish_round().map(drop) } else { Ok(()) }
                .and_then(|()| a.close(&Stop::Terminated).map(drop));
            if let Err(journal) = recorded {
                eprintln!("nucleus-discovery: {}/{problem}: cannot record the termination: {journal}", a.run);
            }
            self.progress.rollout_terminated(&problem, a.rollout.spent());
        }
        let mine: Vec<ProcessId> = self.workers.keys().copied().collect();
        match self.kill_live(&mine) {
            Ok(()) => {}
            Err(Failure::Channel(e)) => return Err(e),
            Err(Failure::Run(journal)) => {
                eprintln!("nucleus-discovery: {}: cannot record a kill: {journal}", self.run())
            }
        }
        self.progress.fail(why);
        Ok(())
    }

    // ---- events ----

    fn child_exited(&mut self, child: ProcessId) -> Result<(), ChannelError> {
        let Some(w) = self.workers.remove(&child) else { return Ok(()) };
        if let Some(a) = self.active.get_mut(&w.problem)
            && let Some((task, false)) = a.rollout.worker_exited(child)
        {
            // The worker's own record says why it gave up (model error, no
            // candidate, ...); the kernel's exit event carries no status.
            let record = a.workers_dir.join(&w.problem).join(&task).join("result.json");
            let detail = std::fs::read(&record)
                .ok()
                .filter(|bytes| bytes.len() <= 64 * 1024)
                .and_then(|bytes| worker_failure_detail(&bytes));
            if let Some(detail) = detail {
                a.rollout.explain_failure(&task, &detail);
            }
        }
        let record = Record {
            problem: &w.problem,
            task: Some(&w.task),
            process: Some(child.0),
            event: "exited",
            error: None,
        };
        if let Err(why) = self.log(&record) {
            eprintln!("nucleus-discovery: run {} failed: {why}", self.run());
            self.fail(why)?;
        }
        Ok(())
    }

    /// A call on this process's endpoint once the run has begun: only
    /// `forward` is served.
    fn dispatch(&mut self, d: Dispatch) -> Result<(), ChannelError> {
        match d.action.as_str() {
            wire::ACTION_FORWARD => self.forward(d),
            wire::ACTION_ADMIT | wire::ACTION_BEGIN => {
                self.kernel.reply(d.id, Err(invalid(format!("run {} has begun", self.run()))))
            }
            _ => self.kernel.reply(d.id, Err(invalid("unknown action"))),
        }
    }

    /// A worker's `explore` call, forwarded by the service: served by the
    /// rollout of its problem, which binds the task to the worker spawned for
    /// it; a submission is evaluated before it is answered.
    fn forward(&mut self, d: Dispatch) -> Result<(), ChannelError> {
        let forward = match wire::decode::<wire::Forward>(&d.payload) {
            Ok(forward) => forward,
            Err(e) => return self.kernel.reply(d.id, Err(e)),
        };
        let open = explore::task_of(&d.resource)
            .ok()
            .filter(|(key, problem, _)| *key == self.authority.key && self.active.contains_key(*problem))
            .map(|(_, problem, _)| problem.to_owned());
        let Some((problem, mut a)) = open.and_then(|p| self.active.remove(&p).map(|a| (p, a))) else {
            return self.kernel.reply(d.id, Err(invalid("that task is not open")));
        };
        let call = Call {
            dispatch: d.id,
            action: forward.action.as_str(),
            resource: &d.resource,
            caller: forward.caller,
            payload: &forward.payload,
        };
        let outcome = self.serve_explore(&mut a, &call);
        self.active.insert(problem, a);
        match outcome {
            Ok(()) => Ok(()),
            Err(Failure::Channel(e)) => Err(e),
            Err(Failure::Run(why)) => {
                eprintln!("nucleus-discovery: run {} failed: {why}", self.run());
                self.fail(why)
            }
        }
    }

    fn serve_explore(&mut self, a: &mut ActiveRollout, call: &Call) -> Fallible<()> {
        match a.rollout.serve(call.action, call.resource, call.caller, call.payload) {
            Served::Reply(result) => Ok(self.kernel.reply(call.dispatch, result)?),
            Served::Accepted { task, parent, process, submission } => {
                // Lean decides before anything is recorded. The worker waits
                // for the verdict, because the verdict is what it asked for.
                let problem = a.rollout.problem().to_owned();
                let mode = if submission.final_offer {
                    EvaluationMode::Collection
                } else {
                    EvaluationMode::ProofSearch
                };
                let evaluated = self.evaluate(&problem, &a.rollout.statement, &submission.file, mode)?;
                if self.kernel.terminate_requested() {
                    return Ok(());
                }
                match evaluated {
                    Ok(eval) => {
                        // During proof search only a complete, verified proof
                        // is recorded. Anything else is refused with Lean's
                        // own words, and the task's one submission is NOT
                        // spent: the model has budget left and now knows what
                        // to fix. Its last word (`final_offer`) is recorded
                        // whatever it says, because there is no later attempt
                        // to make it better.
                        if !submission.final_offer
                            && (eval.check.status != CheckStatus::Complete || eval.verified != Some(true))
                        {
                            let why = format!(
                                "Submission refused: a complete verified proof is required during proof search. Continue solving the remaining obligations.\n{}",
                                refusal(&eval.check)
                            );
                            self.kernel
                                .reply(call.dispatch, Err(Error::new(ErrorCode::InvalidInput, why)))?;
                            return Ok(());
                        }
                        let solved = eval.verified == Some(true);
                        let id = a.rollout.add_child(parent, &task, process, &submission, eval)?;
                        // One terminal artifact: settle its measured charge.
                        a.rollout.record_submission(&task, &submission);
                        if let Some(record) = a.rollout.node_record(id, elapsed_ms(a.started)) {
                            a.journal.node(&record)?;
                            a.journal.nodes.sync_data().map_err(|e| e.to_string())?;
                        }
                        self.kernel.reply(call.dispatch, Payload::encode(&serde_json::json!({})))?;
                        // The problem is answered: nothing this round's other
                        // workers can still produce will be continued, so stop
                        // paying for it. `plan_round` already refuses the next
                        // round; this ends the current one.
                        if solved {
                            let cut = a.rollout.stop_for_solve(&task);
                            self.kill_live(&cut)?;
                        }
                    }
                    Err(e) => {
                        self.kernel.reply(call.dispatch, Err(e.clone()))?;
                        // No artifact was accepted. Keep the task open and its
                        // full reservation held while the worker may retry.
                        // Exit without an accepted node is recorded separately.
                    }
                }
                Ok(())
            }
        }
    }

    /// Where the run stands, as the service is told: its status with the
    /// live rollouts' spend, the problems started, and the rounds of every
    /// active problem its journal does not have yet — within
    /// [`wire::MAX_REPORT_BYTES`]: the live rounds' `failed` entries go first,
    /// then the live rounds (the journal has them once they close).
    fn current_report(&self) -> Report {
        let live_spend = self.active.values().fold(Spend::default(), |sum, a| sum.plus(a.rollout.spent()));
        let mut report = Report {
            v: wire::PROTOCOL,
            status: Some(self.progress.status(live_spend)),
            started: self.progress.started(),
            live: self
                .active
                .iter()
                .map(|(problem, a)| LiveRounds { problem: problem.clone(), rounds: a.live_rounds() })
                .filter(|live| !live.rounds.is_empty())
                .collect(),
        };
        if wire::encode(&report).is_err() {
            report.live.iter_mut().flat_map(|live| live.rounds.iter_mut()).for_each(|r| r.failed.clear());
        }
        if wire::encode(&report).is_err() {
            report.live.clear();
        }
        report
    }

    /// Tells the service where the run stands, when that changed since it
    /// was last told. A refused report is reported here and not retried: the
    /// next change sends another.
    fn report(&mut self) -> Result<(), ChannelError> {
        let report = self.current_report();
        if self.reported.as_ref() == Some(&report) {
            return Ok(());
        }
        match send_report(&mut self.kernel, &self.authority, &report)? {
            Ok(()) => self.reported = Some(report),
            Err(e) => eprintln!(
                "nucleus-discovery: run {}: the service refused a report: {:?}: {}",
                self.run(),
                e.code,
                e.message
            ),
        }
        Ok(())
    }

    /// Waits for the service's `begin`; `false` when the process is
    /// terminated first. Nothing else is served before it.
    fn wait_for_begin(&mut self) -> Result<bool, ChannelError> {
        loop {
            match self.kernel.next_event()? {
                Event::Dispatch(d) if d.action.as_str() == wire::ACTION_BEGIN => {
                    let answer = wire::decode::<wire::Begin>(&d.payload)
                        .and_then(|_| wire::encode(&wire::Ack { v: wire::PROTOCOL }));
                    let begun = answer.is_ok();
                    self.kernel.reply(d.id, answer)?;
                    if begun {
                        return Ok(true);
                    }
                }
                Event::Dispatch(d) => {
                    let why = format!("run {} has not begun", self.run());
                    self.kernel.reply(d.id, Err(invalid(why)))?;
                }
                Event::Terminate => return Ok(false),
                Event::ChildExited(_) | Event::ChildReaped { .. } | Event::CancelDispatch(_) => {}
            }
        }
    }

    /// The run, from `begin` to its end: its rollouts, the calls the service
    /// forwards, its workers' exits, its round deadlines, and a report after
    /// every change. Returns once the run ended or the process is terminated.
    fn run_loop(&mut self) -> Result<(), ChannelError> {
        if !self.wait_for_begin()? {
            return Ok(());
        }
        loop {
            if self.kernel.terminate_requested() {
                return Ok(());
            }
            self.advance()?;
            self.report()?;
            if self.progress.ended() {
                return Ok(());
            }
            // Wait for the kernel, but no longer than the nearest deadline.
            let event = match self.next_deadline() {
                None => self.kernel.next_event()?,
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match self.kernel.next_event_within(wait)? {
                        Some(event) => event,
                        None => {
                            self.expire_rounds()?;
                            continue;
                        }
                    }
                }
            };
            match event {
                Event::Dispatch(d) => self.dispatch(d)?,
                Event::ChildExited(child) => self.child_exited(child)?,
                Event::ChildReaped { .. } | Event::CancelDispatch(_) => {}
                Event::Terminate => return Ok(()),
            }
        }
    }

    /// Reads the exits of the workers it still counts once the run has
    /// ended, so their `exited` records are written; each is already on its
    /// way (a worker that is not killed has exited), and a wait of
    /// [`DRAIN_WAIT`] with none arriving ends it.
    fn drain(&mut self) {
        while !self.workers.is_empty() && !self.kernel.terminate_requested() {
            match self.kernel.next_event_within(DRAIN_WAIT) {
                Ok(Some(Event::ChildExited(child))) => {
                    if self.child_exited(child).is_err() {
                        return;
                    }
                }
                Ok(Some(Event::Dispatch(d))) => {
                    let why = format!("run {} has ended", self.run());
                    if self.kernel.reply(d.id, Err(invalid(why))).is_err() {
                        return;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    }

    /// Serves the run and closes its journals, even when termination
    /// interrupted an awaited kernel call. The kernel's `shutting_down` is
    /// the service's stop and counts as termination.
    pub(crate) fn serve(mut self) -> Result<(), String> {
        let outcome = self.run_loop();
        let terminated =
            matches!(outcome, Err(ChannelError::Terminated)) || self.kernel.terminate_requested();
        if outcome.is_ok() && !terminated {
            // The run ended, done or failed, and the service has its last report.
            self.drain();
            return Ok(());
        }
        let failure = outcome.err().filter(|_| !terminated).map(|e| format!("kernel channel: {e}"));
        let stop = failure.clone().map_or(Stop::Terminated, Stop::Failed);
        match (failure, self.finish_after_exit(&stop)) {
            (None, closed) => closed,
            (Some(failure), Ok(())) => Err(failure),
            (Some(failure), Err(cleanup)) => Err(format!("{failure}; closing journals: {cleanup}")),
        }
    }

    /// Close local journals even if termination interrupted an awaited kernel
    /// call. Authority is already withdrawn: the supervisor owns child reaping,
    /// so cleanup must not make kill/list/reply calls under the old handles.
    ///
    /// This covers a graceful termination only: a kill past the supervisor's
    /// grace period, a crash or a failed write leaves journals open, and a
    /// later `resume` closes them ([`resume::close_open`]) once the kernel
    /// has confirmed the process gone.
    fn finish_after_exit(&mut self, stop: &Stop) -> Result<(), String> {
        let mut errors = Vec::new();
        for (problem, mut a) in std::mem::take(&mut self.active) {
            let recorded = if a.in_round { a.finish_round().map(drop) } else { Ok(()) }
                .and_then(|()| a.close(stop).map(drop));
            if let Err(why) = recorded {
                errors.push(format!("{}/{problem}: {why}", a.run));
            }
            for file in [&a.journal.nodes, &a.journal.rounds] {
                if let Err(why) = file.sync_data() {
                    errors.push(format!("{}/{problem}: journal sync: {why}", a.run));
                }
            }
            self.progress.rollout_terminated(&problem, a.rollout.spent());
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}

/// A forwarded worker call, as the harness serves it.
struct Call<'c> {
    /// The service's dispatch, answered with what the worker gets.
    dispatch: DispatchId,
    action: &'c str,
    resource: &'c ResourcePath,
    caller: ProcessId,
    payload: &'c Payload,
}

/// `<what>: <code>: <message>` of a failed `lean` call.
fn lean_error(what: &str, e: &Error) -> String {
    format!("{what}: {:?}: {}", e.code, e.message)
}

/// Why a submission was handed back instead of recorded: Lean's own verdict
/// on the file, in the words Lean used, so the caller can fix it rather than
/// guess what displeased the harness. Bounded — a file with forty errors
/// should not cost the model its context to be told so.
fn refusal(check: &CheckResponse) -> String {
    let mut why = format!(
        "No artifact was recorded. Lean status: {}; {} remaining proof obligation(s), {} error(s). ",
        observe::status_name(check.status),
        check.sorries.len(),
        check.errors.len()
    );
    if let Some(reason) = &check.invalid_reason {
        why.push_str(reason);
        return why;
    }
    why.push_str("Lean says:");
    for error in check.errors.iter().take(5) {
        why.push_str(&format!("\n  line {}, column {}: {}", error.line, error.column, error.message));
    }
    if check.errors.len() > 5 {
        why.push_str(&format!("\n  … and {} more", check.errors.len() - 5));
    }
    why
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{eval, grow, limits, rollout, scratch, submission};

    #[test]
    fn an_abandoned_rollout_closes_its_open_round_with_the_failure_and_hands_back_its_spend() {
        let dir = scratch("abandon");
        let identity = PolicySource::Builtin.record();
        let mut a = ActiveRollout {
            run: "run".into(),
            workers_dir: dir.join("run.workers"),
            rollout: rollout(0.5, limits()),
            policy: RolloutPolicy::Builtin(InitialPolicy),
            journal: Journal::open(&dir.join("demo")).unwrap(),
            identity: identity.clone(),
            started: Instant::now(),
            pending: None,
            in_round: false,
            round_deadline: None,
            backoff_until: None,
            provider_retries: 0,
            provider_retry_batch: None,
        };
        // A completed round is held back until the loop knows whether it is
        // the last (its record is `pending`); then a second round opens.
        a.rollout.plan_round(&mut InitialPolicy).unwrap();
        a.rollout.open_task("t1", 0, ProcessId(11)).unwrap();
        a.rollout.add_child(0, "t1", ProcessId(11), &submission("f"), eval(0.5)).unwrap();
        a.in_round = true;
        a.finish_round().unwrap();
        a.rollout.plan_round(&mut InitialPolicy).unwrap();
        a.rollout.open_task("t2", 1, ProcessId(12)).unwrap();
        a.in_round = true;
        a.round_deadline = Some(Instant::now());

        let spend = a.abandon("journal: disk full");
        assert_eq!(spend.calls, 8 + 8, "both grants: t1 never reported usage here, t2 is open");
        let rounds: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("demo/rounds.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rounds.len(), 2, "both rounds are on disk: {rounds:?}");
        assert!(rounds[0].get("reason").is_none());
        assert_eq!(
            (rounds[1]["round"].as_u64(), rounds[1]["batch"].as_array().map(Vec::len)),
            (Some(2), Some(2))
        );
        assert_eq!(rounds[1]["reason"], "failed: journal: disk full");
        assert!(!a.in_round, "nothing is left open");
        // A rollout between rounds gets a closing record too.
        let mut idle = ActiveRollout {
            run: "run".into(),
            workers_dir: dir.join("run.workers"),
            rollout: rollout(0.5, limits()),
            policy: RolloutPolicy::Builtin(InitialPolicy),
            journal: Journal::open(&dir.join("idle")).unwrap(),
            identity,
            started: Instant::now(),
            pending: None,
            in_round: false,
            round_deadline: None,
            backoff_until: None,
            provider_retries: 0,
            provider_retry_batch: None,
        };
        assert_eq!(idle.abandon("x"), Spend::default());
        let text = std::fs::read_to_string(dir.join("idle/rounds.jsonl")).unwrap();
        assert!(text.contains("\"reason\":\"failed: x\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_rollout_adds_the_rounds_the_journal_does_not_have_yet() {
        let dir = scratch("live");
        let identity = PolicySource::Builtin.record();
        let mut a = ActiveRollout {
            run: "run".into(),
            workers_dir: dir.join("run.workers"),
            rollout: rollout(0.5, Limits { w: 2, ..limits() }),
            policy: RolloutPolicy::Builtin(InitialPolicy),
            journal: Journal::open(&dir.join("demo")).unwrap(),
            identity,
            started: Instant::now(),
            pending: None,
            in_round: false,
            round_deadline: None,
            backoff_until: None,
            provider_retries: 0,
            provider_retry_batch: None,
        };
        assert!(a.live_rounds().is_empty(), "nothing planned yet");
        // Round 1 open: its batch, no child yet, the reserved spend; no reason.
        a.rollout.plan_round(&mut InitialPolicy).unwrap();
        a.rollout.open_task("t1", 0, ProcessId(11)).unwrap();
        a.rollout.open_task("t2", 0, ProcessId(12)).unwrap();
        a.in_round = true;
        let open = a.live_rounds();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].round, &open[0].batch, &open[0].children, &open[0].reason),
            (1, &vec![0, 0], &vec![], &None)
        );
        assert_eq!(open[0].policy, Some(PolicySource::Builtin.record()));
        // A child arrives: the open record shows it and the reported tokens.
        let mut sub = submission("f");
        sub.input_tokens = 18;
        sub.output_tokens = 8;
        let res = ResourcePath::new(["run", "demo", "t1"]).unwrap();
        a.rollout.accept_submission(&res, ProcessId(11), sub.clone()).unwrap();
        a.rollout.record_submission("t1", &sub);
        a.rollout.add_child(0, "t1", ProcessId(11), &sub, eval(0.5)).unwrap();
        let open = a.live_rounds();
        assert_eq!((&open[0].children, open[0].tokens_in, open[0].tokens_out), (&vec![1], 18, 8));
        // Round 1 completed but not yet written (held back for its reason),
        // then round 2 open: both are live; the journal has neither.
        a.rollout.worker_exited(ProcessId(12));
        a.finish_round().unwrap();
        assert!(std::fs::read_to_string(dir.join("demo/rounds.jsonl")).unwrap().is_empty());
        a.rollout.plan_round(&mut InitialPolicy).unwrap();
        a.in_round = true;
        let live = a.live_rounds();
        assert_eq!(live.iter().map(|r| r.round).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(live[0].failed, [Failed { task: "t2".into(), why: "exited_without_submitting".into() }]);
        assert!(live.iter().all(|r| r.reason.is_none()));
        // What the journal and the live rounds together say about the problem.
        let mut journal = read_problem(&dir, &dir.join("demo"), false).unwrap();
        journal.rounds.extend(live);
        let row = summarize("demo", ProblemState::Active, &journal);
        assert_eq!((row.rounds, row.tokens_in, row.stop), (2, 18, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_round_lost_to_the_provider_runs_the_same_parents_again_without_asking_the_policy() {
        let dir = scratch("provider_replay");
        let mut r = rollout(0.5, limits());
        let weak = grow(&mut r.tree, 0, 0, 2);
        let strong = grow(&mut r.tree, 0, 0, 0);
        let mut a = ActiveRollout {
            run: "run".into(),
            workers_dir: dir.join("run.workers"),
            rollout: r,
            policy: RolloutPolicy::Builtin(InitialPolicy),
            journal: Journal::open(&dir.join("demo")).unwrap(),
            identity: PolicySource::Builtin.record(),
            started: Instant::now(),
            pending: None,
            in_round: false,
            round_deadline: None,
            backoff_until: None,
            provider_retries: 1,
            provider_retry_batch: Some(vec![weak]),
        };
        assert_eq!(a.plan_next().unwrap(), [weak], "the lost round's parents, not the policy's choice");
        a.rollout.finish_round();
        assert_eq!(a.plan_next().unwrap()[0], strong, "then the policy decides again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
