//! Explicit new execution of a saved, versioned journal, in the run's own
//! process. No saved process, handle, approval or tool invocation becomes
//! live authority here.
//!
//! A run whose previous process ended without closing its journals — killed
//! past its grace period, crashed, or lost with the whole Nucleus — is closed
//! here first ([`close_open`]), exactly as a stop would have closed it: the
//! service starts a resuming process only once the kernel confirmed the
//! previous process and its whole subtree gone, or after a restart of the
//! whole Nucleus, and this process holds the run's lease, so it is the only
//! writer.
use super::*;
use crate::harness::Admitted;
use crate::progress::Progress;
use crate::wire::RunCommand;
use std::io::Read;

pub(super) struct Problem {
    pub nodes: Vec<NodeRecord>,
    pub spend: Spend,
    pub round: u32,
}

/// Restore discovery's own recorded observation, without rerunning Lean or
/// recalculating its score under a newer scoring implementation. This carries
/// search state, never permissions or a claim of a new verification run.
pub(super) fn evaluation(node: &NodeRecord) -> Evaluation {
    Evaluation {
        check: node.check.clone(),
        verified: node.verified,
        verify_rejection: node.verify_rejection.clone(),
        score: node.score,
        sketch: node.sketch.clone(),
        completion: node.completion.clone(),
        spliced: node.completion.as_ref().and_then(|c| c.original_file.as_ref()).map(|_| node.file.clone()),
        errors_obs: node.errors_obs.clone(),
        analysis_failed: node.analysis_failed.clone(),
    }
}

struct Saved {
    problems: BTreeMap<String, Problem>,
    next_task: u64,
    uncertain: u64,
}

/// The store is a private service bind. Locks also exclude another launcher
/// using the same store; losing this service closes the OS-owned lease.
pub(super) fn lease(dir: &Path) -> Result<File, Error> {
    let path = dir.join(".session.lock");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(invalid("session lease must be a regular file"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| invalid(format!("session lease: {e}")))?;
    file.try_lock().map_err(|e| {
        Error::new(
            ErrorCode::ServiceError,
            format!("session is owned by another live runtime (or locking is unavailable): {e}"),
        )
    })?;
    Ok(file)
}

pub(super) fn journal(dir: &Path) -> Result<Journal, String> {
    let open = |name| OpenOptions::new().append(true).open(dir.join(name)).map_err(|e| e.to_string());
    Ok(Journal { nodes: open("nodes.jsonl")?, rounds: open("rounds.jsonl")? })
}

struct Reader {
    root: PathBuf,
    remaining: u64,
}
impl Reader {
    fn read(&mut self, path: &Path) -> Result<Vec<u8>, Error> {
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(invalid(format!("{} is not a regular checkpoint file", path.display())));
        }
        let path = path.canonicalize().map_err(|e| invalid(e.to_string()))?;
        if !path.starts_with(&self.root) {
            return Err(invalid("checkpoint path escapes the session store"));
        }
        if meta.len() > self.remaining {
            return Err(invalid("resume exceeds --resume-max-bytes"));
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| invalid(e.to_string()))?
            .take(self.remaining.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| invalid(e.to_string()))?;
        let count = u64::try_from(bytes.len()).map_err(|e| invalid(e.to_string()))?;
        self.remaining =
            self.remaining.checked_sub(count).ok_or_else(|| invalid("resume exceeds --resume-max-bytes"))?;
        Ok(bytes)
    }

    fn json<T: DeserializeOwned>(&mut self, path: &Path) -> Result<T, Error> {
        serde_json::from_slice(&self.read(path)?).map_err(|e| invalid(format!("{}: {e}", path.display())))
    }

    fn lines<T: DeserializeOwned>(&mut self, path: &Path) -> Result<Vec<T>, Error> {
        let bytes = self.read(path)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(invalid(format!(
                "{} has an incomplete final record; source retained",
                path.display()
            )));
        }
        bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).map_err(|e| invalid(format!("{}: {e}", path.display()))))
            .collect()
    }

    /// A killed worker's record file as far as it was written: a last line
    /// without its line feed counts when it is a whole record and is left
    /// out when the kill cut it short. Any other broken line is refused.
    fn worker_lines<T: DeserializeOwned>(&mut self, path: &Path) -> Result<Vec<T>, Error> {
        let bytes = self.read(path)?;
        let (whole, tail) = match bytes.iter().rposition(|b| *b == b'\n') {
            Some(end) => bytes.split_at(end + 1),
            None => (&[][..], &bytes[..]),
        };
        let mut records: Vec<T> = whole
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).map_err(|e| invalid(format!("{}: {e}", path.display()))))
            .collect::<Result<_, _>>()?;
        records.extend(serde_json::from_slice(tail).ok());
        Ok(records)
    }
}

fn task_number(task: &str) -> Result<u64, Error> {
    let digits = task
        .strip_prefix('t')
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| invalid("invalid saved worker ID"))?;
    digits.parse().map_err(|_| invalid("saved worker ID is too large"))
}

#[derive(Deserialize)]
struct Receipt {
    attempts: u64,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Deserialize)]
struct Attempt {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cost_nano_usd: Option<u64>,
}

/// What the workers of `problem` spent, as their own records say. Every
/// worker of an earlier execution is gone (the service resumes only then),
/// so its records are final; a worker killed while writing one is read as
/// far as it got, and one without a whole receipt is charged its whole
/// reservation.
fn receipts(
    reader: &mut Reader,
    plan: &RunPlan,
    problem: &str,
    floor: &BTreeMap<String, Spend>,
) -> Result<(Spend, u64, u64), Error> {
    let dir = plan.workers_dir.join(problem);
    let mut total = Spend::default();
    let (mut uncertain, mut next_task) = (0u64, 0u64);
    if !dir.exists() {
        return Ok((total, uncertain, next_task));
    }
    let resolved = dir.canonicalize().map_err(|e| invalid(e.to_string()))?;
    if !resolved.starts_with(&reader.root) {
        return Err(invalid("worker directory escapes the store"));
    }
    for entry in std::fs::read_dir(&dir).map_err(|e| invalid(e.to_string()))? {
        let entry = entry.map_err(|e| invalid(e.to_string()))?;
        if !entry.file_type().map_err(|e| invalid(e.to_string()))?.is_dir() {
            return Err(invalid("worker checkpoint must be a regular directory"));
        }
        let name = entry.file_name().into_string().map_err(|_| invalid("invalid worker directory"))?;
        next_task = next_task.max(task_number(&name)?);
        let mut spent = Spend::default();
        let path = entry.path();
        let receipt = if path.join("result.json").exists() {
            serde_json::from_slice::<Receipt>(&reader.read(&path.join("result.json"))?).ok()
        } else {
            None
        };
        if let Some(r) = receipt {
            spent.calls = r.attempts;
            spent.tokens_in = r.input_tokens;
            spent.tokens_out = r.output_tokens;
        } else {
            // A killed/crashed worker did not reconcile its grant. Keep the
            // entire reservation charged, never infer zero use from silence.
            let reservation = path.join("reservation.json");
            spent.calls = if reservation.exists() {
                let value: serde_json::Value = reader.json(&reservation)?;
                if value["v"] != 1 {
                    return Err(invalid("unsupported worker reservation version"));
                }
                value["model_calls"]
                    .as_u64()
                    .filter(|calls| *calls > 0)
                    .ok_or_else(|| invalid("invalid worker reservation"))?
            } else {
                // Without a reservation, the search calls alone.
                plan.limits.calls_per_continuation
            };
        }
        let mut priced_calls = 0u64;
        if path.join("attempts.jsonl").exists() {
            let steps: Vec<Attempt> = reader.worker_lines(&path.join("attempts.jsonl"))?;
            let mut measured = Spend::default();
            for step in steps {
                if step.kind.as_deref() != Some("model") {
                    continue;
                }
                measured.calls = measured.calls.saturating_add(1);
                measured.tokens_in = measured.tokens_in.saturating_add(step.input_tokens);
                measured.tokens_out = measured.tokens_out.saturating_add(step.output_tokens);
                if let Some(cost) = step.cost_nano_usd {
                    priced_calls = priced_calls.saturating_add(1);
                    measured.cost_nano = measured.cost_nano.saturating_add(cost);
                }
            }
            spent.calls = spent.calls.max(measured.calls);
            spent.tokens_in = spent.tokens_in.max(measured.tokens_in);
            spent.tokens_out = spent.tokens_out.max(measured.tokens_out);
            spent.cost_nano = measured.cost_nano;
        }
        if let Some(minimum) = floor.get(&name) {
            spent.calls = spent.calls.max(minimum.calls);
            spent.tokens_in = spent.tokens_in.max(minimum.tokens_in);
            spent.tokens_out = spent.tokens_out.max(minimum.tokens_out);
        }
        uncertain = uncertain.saturating_add(spent.calls.saturating_sub(priced_calls));
        total = total.plus(spent);
    }
    Ok((total, uncertain, next_task))
}

#[derive(Deserialize)]
struct LifeRecord {
    problem: String,
    task: Option<String>,
    event: String,
}

/// The saved state of every problem of `plan` that has a journal, whose
/// journals [`close_open`] has closed.
fn load(reader: &mut Reader, plan: &RunPlan) -> Result<Saved, Error> {
    let life: Vec<LifeRecord> = reader.lines(&plan.dir.join(EVENTS_FILE))?;
    let mut saved = Saved { problems: BTreeMap::new(), next_task: 0, uncertain: 0 };
    for event in &life {
        if let Some(task) = &event.task {
            saved.next_task = saved.next_task.max(task_number(task)?);
        }
    }
    for problem in &plan.problems {
        let dir = plan.dir.join(problem);
        if !dir.join(NODES_FILE).exists() && !dir.join(ROUNDS_FILE).exists() {
            continue;
        }
        let nodes: Vec<NodeRecord> = reader.lines(&dir.join(NODES_FILE))?;
        let rounds: Vec<RoundRecord> = reader.lines(&dir.join(ROUNDS_FILE))?;
        let closed = rounds.last().is_some_and(|r| r.reason.is_some());
        if nodes.is_empty() && closed {
            // Ended before it had a root (its statement refused, its root
            // check failed): no checkpoint, so it starts again from its root,
            // after what its journal says.
            continue;
        }
        if nodes.is_empty() || !closed {
            return Err(invalid("the run is not closed at a recorded checkpoint"));
        }
        if rounds.iter().any(|r| r.v != JOURNAL_VERSION) {
            return Err(invalid("unsupported rounds journal version"));
        }
        for (i, node) in nodes.iter().enumerate() {
            if node.v != JOURNAL_VERSION
                || node.id != node_u64(i)
                || (i == 0 && (node.parent.is_some() || node.task.is_some()))
                || (i > 0 && node.parent.is_none_or(|p| p >= node.id))
            {
                return Err(invalid("invalid or unsupported node journal"));
            }
            if i > 0 {
                let task = node.task.as_deref().ok_or_else(|| invalid("missing worker ID"))?;
                task_number(task)?;
                if !plan.workers_dir.join(problem).join(task).is_dir() {
                    return Err(invalid(
                        "worker receipts were removed; cannot reconstruct cumulative budget",
                    ));
                }
                saved.next_task = saved
                    .next_task
                    .max(task_number(node.task.as_deref().ok_or_else(|| invalid("missing worker ID"))?)?);
            }
        }
        let mut by_task: BTreeMap<String, Spend> = BTreeMap::new();
        for node in &nodes {
            if let (Some(task), Some(worker)) = (&node.task, &node.worker) {
                let floor = by_task.entry(task.clone()).or_default();
                floor.calls = floor.calls.max(u64::from(worker.model_calls));
                floor.tokens_in = floor.tokens_in.max(worker.input_tokens);
                floor.tokens_out = floor.tokens_out.max(worker.output_tokens);
            }
        }
        let (mut spend, mut unknown, next) = receipts(reader, plan, problem, &by_task)?;
        // A child can be killed before it creates its first receipt directory.
        // The parent's spawn journal still reserves its entire call grant.
        let missing: BTreeSet<_> = life
            .iter()
            .filter(|r| r.problem == *problem && r.event == "spawned")
            .filter_map(|r| r.task.as_ref())
            .filter(|t| !plan.workers_dir.join(problem).join(t).is_dir())
            .collect();
        for _ in missing {
            spend.calls = spend.calls.saturating_add(plan.limits.calls_per_continuation);
            unknown = unknown.saturating_add(plan.limits.calls_per_continuation);
        }
        saved.next_task = saved.next_task.max(next);
        saved.uncertain = saved.uncertain.saturating_add(unknown);
        // Round records carry the cumulative tokens and the USD rounded up
        // to micro-USD: keep that bound when worker receipts say less.
        for r in &rounds {
            spend.tokens_in = spend.tokens_in.max(r.tokens_in);
            spend.tokens_out = spend.tokens_out.max(r.tokens_out);
            if let Some(usd) = r.usd {
                if !usd.is_finite() || usd < 0.0 {
                    return Err(invalid("invalid recorded USD"));
                }
                // Saturation denies further spend; it cannot increase credit.
                spend.cost_nano = spend.cost_nano.max((usd * 1_000_000_000.0).ceil() as u64);
            }
        }
        let round = rounds.iter().map(|r| r.round).max().unwrap_or(0);
        if round > u32::MAX - plan.limits.k1 {
            return Err(invalid("session round counter exhausted"));
        }
        saved.problems.insert(problem.clone(), Problem { nodes, spend, round });
    }
    if saved.next_task == u64::MAX {
        return Err(invalid("session worker counter exhausted"));
    }
    Ok(saved)
}

/// The mode a tool-loop worker renders from its run's template at startup,
/// before any model call, for the notice after a context reset.
const CONTEXT_RESET_MODE: &str = "context_reset";

/// A tool-loop worker (`tool_loop`: the run's workers get a shell) stops at
/// startup when its run's template has no [`CONTEXT_RESET_MODE`], so a task
/// with such a template is refused before anything is registered or
/// created. The fixed loop renders no such mode.
pub(super) fn fits_tool_loop(template: &str, tool_loop: bool) -> Result<(), Error> {
    if tool_loop && !template.contains(CONTEXT_RESET_MODE) {
        return Err(invalid(format!(
            "task: prompt_template has no `{CONTEXT_RESET_MODE}` mode, which every tool-loop worker renders; \
             start from prompts/prover-agent.j2"
        )));
    }
    Ok(())
}

/// `discovery resume`, in the run's own process: the saved task is accepted
/// again as it stands today (its template first, then the current
/// ceilings), its supplied statements are registered with Lean again by
/// `until` (the `lean` service may have restarted, or refuse a text it once
/// took; a refusal refuses the resume), and only then is the run's lease
/// taken, what its previous process left open closed, and its checkpoint
/// loaded and recorded as admitted.
pub(super) fn admit(
    lean: &mut dyn admit::LeanService,
    command: &RunCommand,
    key: &str,
    until: Instant,
    tool_loop: bool,
) -> Result<Result<Admitted, Error>, ChannelError> {
    let (plan, reader) = match saved_plan(command, tool_loop) {
        Ok(saved) => saved,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = admit::register(&plan, key, until, lean)? {
        return Ok(Err(e));
    }
    Ok(checkpoint(plan, reader, command.options.resume_max_bytes))
}

/// The saved task of the run, accepted again.
fn saved_plan(command: &RunCommand, tool_loop: bool) -> Result<(RunPlan, Reader), Error> {
    let (options, run) = (&command.options, command.run.as_str());
    let root = options.out.canonicalize().map_err(|e| invalid(e.to_string()))?;
    let mut reader = Reader { root, remaining: options.resume_max_bytes };
    let task: Task = reader.json(&options.out.join(run).join(TASK_FILE))?;
    if task.name != run {
        return Err(invalid("session ID differs from its saved task"));
    }
    let plan = resolve_task_mode(&task, options, &options.out, &options.ceilings(), true)?;
    fits_tool_loop(&plan.prompt_template, tool_loop)?;
    Ok((plan, reader))
}

/// The run's checkpoint, under its lease: what the previous process left
/// open closed, the saved problems loaded, and the admission recorded in
/// `resume.jsonl` before anything runs.
fn checkpoint(plan: RunPlan, mut reader: Reader, max_bytes: u64) -> Result<Admitted, Error> {
    let lease = lease(&plan.dir)?;
    close_open(&plan, max_bytes)?;
    let saved = load(&mut reader, &plan)?;
    let events_path = plan.dir.join("resume.jsonl");
    if events_path.exists() {
        let prior: Vec<serde_json::Value> = reader.lines(&events_path)?;
        if prior.iter().any(|r| r["v"] != 1 || r["event"] != "resume_admitted") {
            return Err(invalid("unsupported resume journal format"));
        }
    }
    let mut events = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_path)
        .map_err(|e| invalid(e.to_string()))?;
    append(
        &mut events,
        &serde_json::json!({"v":1,"session":plan.name,
            "event":"resume_admitted","at_ms":unix_ms(),"uncertain_model_calls":saved.uncertain,
            "next_worker_after":saved.next_task,"fresh_authority":true}),
    )
    .map_err(invalid)?;
    events.sync_data().map_err(|e| invalid(e.to_string()))?;
    Ok(Admitted {
        progress: Progress::new(plan),
        saved: saved.problems,
        next_task: saved.next_task,
        lease,
        uncertain_model_calls: saved.uncertain,
    })
}

/// Closes what the run's previous process left open, as a stop would have
/// closed it, so the run resumes like any closed one:
///
/// - A file whose last line has no line feed ends in a write cut short: a
///   whole record gets its line feed; anything else is cut off (it was never
///   durable, and nobody was told it was).
/// - A problem with no node has no checkpoint and starts again from its
///   root: an open journal's files are removed, a closed one (its statement
///   refused, its root check failed) is kept for what it says.
/// - A journal whose last round has no stop reason gets one closing record,
///   `terminated`, numbered after every round and node on record, with the
///   spend of the last round recorded (worker receipts carry the rest).
///
/// A closed journal is not touched. Each file is read within `max_bytes`.
pub(super) fn close_open(plan: &RunPlan, max_bytes: u64) -> Result<(), Error> {
    for name in [EVENTS_FILE, "resume.jsonl"] {
        repair_tail(&plan.dir.join(name), max_bytes)?;
    }
    for problem in &plan.problems {
        let dir = plan.dir.join(problem);
        let (nodes, rounds) = (dir.join(NODES_FILE), dir.join(ROUNDS_FILE));
        repair_tail(&nodes, max_bytes)?;
        repair_tail(&rounds, max_bytes)?;
        if !nodes.exists() && !rounds.exists() {
            continue;
        }
        let Some(node) = last_record::<NodeRecord>(&nodes, max_bytes)? else {
            if last_record::<RoundRecord>(&rounds, max_bytes)?.is_some_and(|r| r.reason.is_some()) {
                continue;
            }
            for path in [&nodes, &rounds] {
                if path.exists() {
                    std::fs::remove_file(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
                }
            }
            continue;
        };
        let last = last_record::<RoundRecord>(&rounds, max_bytes)?;
        if last.as_ref().is_some_and(|r| r.reason.is_some()) {
            continue;
        }
        let closing = RoundRecord {
            v: JOURNAL_VERSION,
            round: last.as_ref().map_or(0, |r| r.round).max(node.round),
            policy: last.as_ref().and_then(|r| r.policy.clone()).or_else(|| Some(plan.policy.record())),
            batch: vec![],
            children: vec![],
            failed: vec![],
            tokens_in: last.as_ref().map_or(0, |r| r.tokens_in),
            tokens_out: last.as_ref().map_or(0, |r| r.tokens_out),
            usd: last.as_ref().and_then(|r| r.usd),
            reason: None,
            policy_error: None,
        }
        .closed(&Stop::Terminated);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&rounds)
            .map_err(|e| invalid(format!("{}: {e}", rounds.display())))?;
        append(&mut file, &closing).map_err(invalid)?;
        file.sync_data().map_err(|e| invalid(e.to_string()))?;
    }
    Ok(())
}

/// The bytes of the regular file at `path`, at most `max_bytes`
/// (`--resume-max-bytes`); `None` when there is no file.
fn read_bounded(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, Error> {
    observe::read_regular(path, max_bytes).map_err(invalid)
}

/// Ends the file at `path` at its last whole record ([`close_open`]).
fn repair_tail(path: &Path, max_bytes: u64) -> Result<(), Error> {
    let Some(bytes) = read_bounded(path, max_bytes)? else { return Ok(()) };
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let cut = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |end| end + 1);
    let file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    let written = if serde_json::from_slice::<serde_json::Value>(&bytes[cut..]).is_ok() {
        (&file).write_all(b"\n")
    } else {
        file.set_len(cut as u64)
    };
    written.and_then(|()| file.sync_data()).map_err(|e| invalid(format!("{}: {e}", path.display())))
}

/// The last record of the JSONL file at `path`, whose tail [`repair_tail`]
/// has made whole; `None` when there is no file or no record.
fn last_record<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<Option<T>, Error> {
    let Some(bytes) = read_bounded(path, max_bytes)? else { return Ok(None) };
    bytes
        .split(|b| *b == b'\n')
        .rfind(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(|e| invalid(format!("{}: {e}", path.display()))))
        .transpose()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Store(PathBuf);
    impl Store {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "nucleus-resume-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn reader(&self, remaining: u64) -> Reader {
            Reader { root: self.0.canonicalize().unwrap(), remaining }
        }
        fn plan(&self) -> RunPlan {
            let prover = self.0.join("prover");
            let options = Options::parse(
                [
                    "--out",
                    self.0.to_str().unwrap(),
                    "--prover-executable",
                    prover.to_str().unwrap(),
                    "--no-usd-ceiling",
                ]
                .map(str::to_owned),
            )
            .unwrap();
            let task: Task = serde_json::from_value(json!({"v":1,"name":"saved","problems":["demo"],"prompt_template":"{{ statement }}","concurrency":1,"limits":{"w":1,"k1":1,"calls_per_continuation":3,"model_calls_per_problem":10,"max_usd_per_problem":"0"}})).unwrap();
            let plan = resolve_task(&task, &options, &options.out).unwrap();
            materialize(&plan, "dynamic.1").unwrap();
            std::fs::create_dir(plan.dir.join("demo")).unwrap();
            // A journal without the optional scoring fields.
            self.line(&plan.dir.join("demo/nodes.jsonl"), json!({"v":1,"id":0,"parent":null,"round":0,"task":null,"file":"statement","check":{"status":"sorries","errors":[],"sorries":[],"elapsed_ms":0},"verified":null,"score":0.5,"scoring":3,"worker":null,"ms_since_start":0}));
            self.line(&plan.dir.join("demo/rounds.jsonl"), json!({"v":1,"round":1,"batch":[0],"children":[],"tokens_in":20,"tokens_out":10,"usd":0.001,"reason":"rounds"}));
            plan
        }
        fn line(&self, path: &Path, value: serde_json::Value) {
            std::fs::write(path, format!("{value}\n")).unwrap();
        }
    }
    impl Drop for Store {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn closed_checkpoint_keeps_spend_and_does_not_mint_authority() {
        let store = Store::new();
        let plan = store.plan();
        let worker = plan.workers_dir.join("demo/t000019");
        std::fs::create_dir_all(&worker).unwrap();
        store.line(&worker.join("result.json"), json!({"attempts":2,"input_tokens":30,"output_tokens":12}));
        std::fs::write(
            worker.join("attempts.jsonl"),
            "{\"kind\":\"model\",\"cost_nano_usd\":1000000}\n{\"kind\":\"model\"}\n",
        )
        .unwrap();
        let saved = load(&mut store.reader(100_000), &plan).unwrap();
        let p = &saved.problems["demo"];
        assert_eq!(
            (p.spend.calls, p.spend.tokens_in, p.spend.tokens_out, p.spend.cost_nano),
            (2, 30, 12, 1_000_000)
        );
        assert_eq!((saved.next_task, saved.uncertain, p.round), (19, 1, 1));
    }

    /// A worker of `plan`'s problem `demo`, its directory created.
    fn worker(plan: &RunPlan, task: &str) -> PathBuf {
        let dir = plan.workers_dir.join("demo").join(task);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What resume charges a worker whose receipt is missing, cut short or
    /// partial: never less than it may have spent, every call without a
    /// reported cost uncertain, and its task id not handed out again.
    /// `Some((calls, uncertain, next_task))`, or `None` when the checkpoint
    /// is refused.
    #[test]
    fn a_worker_without_a_whole_receipt_is_charged_its_whole_reservation() {
        type Setup = fn(&Store, &RunPlan);
        type Charged = Option<(u64, u64, u64)>;
        let cases: [(&str, Setup, Charged); 7] = [
            ("no receipt: the grant", |_, plan| drop(worker(plan, "t000007")), Some((3, 3, 7))),
            (
                "the reservation, collection calls included",
                |store, plan| {
                    store.line(
                        &worker(plan, "t000007").join("reservation.json"),
                        json!({"v":1,"model_calls":8}),
                    )
                },
                Some((8, 8, 7)),
            ),
            (
                "a reservation of another version",
                |store, plan| {
                    store.line(
                        &worker(plan, "t000007").join("reservation.json"),
                        json!({"v":2,"model_calls":8}),
                    )
                },
                None,
            ),
            (
                "killed while writing its records: the cut receipt is not trusted",
                |store, plan| {
                    let dir = worker(plan, "t000019");
                    store.line(&dir.join("reservation.json"), json!({"v":1,"model_calls":9}));
                    std::fs::write(dir.join("result.json"), "{\"attempts\":1,\"inp").unwrap();
                    std::fs::write(dir.join("attempts.jsonl"), "{\"kind\":\"model\"}\n{\"kind\":\"mo")
                        .unwrap();
                },
                Some((9, 9, 19)),
            ),
            (
                "a whole last step without its line feed counts",
                |store, plan| {
                    let dir = worker(plan, "t000019");
                    store.line(&dir.join("reservation.json"), json!({"v":1,"model_calls":9}));
                    std::fs::write(dir.join("result.json"), "{\"attempts\":1,\"inp").unwrap();
                    let steps = "{\"kind\":\"model\"}\n".repeat(10) + "{\"kind\":\"model\"}";
                    std::fs::write(dir.join("attempts.jsonl"), steps).unwrap();
                },
                Some((11, 11, 19)),
            ),
            (
                "killed before its first receipt",
                |store, plan| {
                    let spawned = json!({"problem":"demo","task":"t000099","process":4,"event":"spawned"});
                    store.line(&plan.dir.join(EVENTS_FILE), spawned);
                },
                Some((3, 3, 99)),
            ),
            (
                "the most each worker reported, summed over workers",
                |store, plan| {
                    let nodes = plan.dir.join("demo/nodes.jsonl");
                    let root: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&nodes).unwrap()).unwrap();
                    let mut rows = vec![root.clone()];
                    for (id, task, calls) in [(1, "t000001", 1), (2, "t000001", 3), (3, "t000002", 1)] {
                        let mut node = root.clone();
                        node["id"] = json!(id);
                        node["parent"] = json!(id - 1);
                        node["task"] = json!(task);
                        node["worker"] =
                            json!({"process":id,"model_calls":calls,"input_tokens":0,"output_tokens":0});
                        rows.push(node);
                    }
                    std::fs::write(nodes, rows.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
                    for (task, calls) in [("t000001", 1), ("t000002", 3)] {
                        let receipt = json!({"attempts":calls,"input_tokens":0,"output_tokens":0});
                        store.line(&worker(plan, task).join("result.json"), receipt);
                    }
                },
                Some((6, 6, 2)),
            ),
        ];
        for (case, setup, expected) in cases {
            let store = Store::new();
            let plan = store.plan();
            setup(&store, &plan);
            let saved = load(&mut store.reader(100_000), &plan);
            let got = saved.ok().map(|s| (s.problems["demo"].spend.calls, s.uncertain, s.next_task));
            assert_eq!(got, expected, "{case}");
        }
    }

    #[test]
    fn rejects_future_truncated_active_and_oversized_journals_without_rewriting() {
        let store = Store::new();
        let plan = store.plan();
        let path = plan.dir.join("demo/rounds.jsonl");
        let original = std::fs::read(&path).unwrap();
        let mut row: serde_json::Value = serde_json::from_slice(&original).unwrap();
        row["v"] = json!(999);
        store.line(&path, row.clone());
        assert!(load(&mut store.reader(100_000), &plan).is_err());
        row["v"] = json!(1);
        row["reason"] = json!(null);
        store.line(&path, row);
        assert!(load(&mut store.reader(100_000), &plan).is_err());
        std::fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(load(&mut store.reader(100_000), &plan).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original[..original.len() - 1]);
        std::fs::write(&path, &original).unwrap();
        assert!(load(&mut store.reader(10), &plan).is_err());
        assert!(load(&mut store.reader(100_000), &plan).is_ok());
    }

    #[test]
    fn a_tool_loop_template_needs_the_context_reset_mode() {
        let old = "{{ statement }}";
        assert!(fits_tool_loop(old, true).unwrap_err().message.contains("prompts/prover-agent.j2"));
        fits_tool_loop(old, false).unwrap();
        let current = format!("{old}{{% if mode == \"context_reset\" %}}reset{{% endif %}}");
        fits_tool_loop(&current, true).unwrap();
    }

    /// What a process that was killed or crashed leaves: an open journal, a
    /// problem with no node, and record files cut mid-line. Closed once, the
    /// run loads like one a stop closed, numbered past everything recorded.
    #[test]
    fn what_an_ended_process_left_open_is_closed_as_terminated_and_then_loads() {
        let store = Store::new();
        let mut plan = store.plan();
        plan.problems.extend(["open", "rootless", "refused", "closed"].map(str::to_owned));
        let node = |id: u64, parent: Option<u64>, round: u32| {
            let task = parent.map(|_| format!("t{id:06}"));
            json!({"v":1,"id":id,"parent":parent,"round":round,"task":task,"file":"statement",
                   "check":{"status":"sorries","errors":[],"sorries":[],"elapsed_ms":0},
                   "verified":null,"score":0.5,"scoring":3,"worker":null,"ms_since_start":0})
        };
        let round = |round: u32, reason: Option<&str>| json!({"v":1,"round":round,"batch":[0],"children":[],"tokens_in":7,"tokens_out":3,"reason":reason});
        let lines =
            |values: &[serde_json::Value]| values.iter().map(|v| format!("{v}\n")).collect::<String>();
        for problem in ["open", "rootless", "refused", "closed"] {
            std::fs::create_dir(plan.dir.join(problem)).unwrap();
        }
        // Round 1 recorded, round 2's record held back and lost, a node of
        // round 3 written, and round 3's record cut mid-line.
        std::fs::create_dir_all(plan.workers_dir.join("open/t000001")).unwrap();
        std::fs::write(plan.dir.join("open/nodes.jsonl"), lines(&[node(0, None, 0), node(1, Some(0), 3)]))
            .unwrap();
        std::fs::write(plan.dir.join("open/rounds.jsonl"), lines(&[round(1, None)]) + "{\"v\":1,\"rou")
            .unwrap();
        // Stopped while its root was being evaluated.
        std::fs::write(plan.dir.join("rootless/nodes.jsonl"), "").unwrap();
        std::fs::write(plan.dir.join("rootless/rounds.jsonl"), "").unwrap();
        // Ended before it had a root: Lean refused its statement.
        let refused = lines(&[round(0, Some("statement_rejected"))]);
        std::fs::write(plan.dir.join("refused/nodes.jsonl"), "").unwrap();
        std::fs::write(plan.dir.join("refused/rounds.jsonl"), &refused).unwrap();
        std::fs::write(plan.dir.join("closed/nodes.jsonl"), lines(&[node(0, None, 0)])).unwrap();
        std::fs::write(plan.dir.join("closed/rounds.jsonl"), lines(&[round(4, Some("rounds"))])).unwrap();
        let events = plan.dir.join(EVENTS_FILE);
        std::fs::write(&events, "{\"problem\":\"open\",\"task\":\"t000001\",\"event\":\"spawned\"}").unwrap();
        assert!(load(&mut store.reader(1_000_000), &plan).is_err(), "open: refused as it stands");

        close_open(&plan, 1_000_000).unwrap();
        close_open(&plan, 1_000_000).unwrap();
        let rounds: Vec<RoundRecord> =
            observe::read_jsonl(&plan.dir, &plan.dir.join("open/rounds.jsonl"), false).unwrap();
        let last = rounds.last().unwrap();
        assert_eq!(rounds.len(), 2, "the cut record is gone, one closing record added, once");
        assert_eq!((last.round, last.reason.as_deref(), last.tokens_in), (3, Some("terminated"), 7));
        assert!(
            !plan.dir.join("rootless/nodes.jsonl").exists()
                && !plan.dir.join("rootless/rounds.jsonl").exists()
        );
        assert_eq!(
            std::fs::read_to_string(plan.dir.join("closed/rounds.jsonl")).unwrap(),
            lines(&[round(4, Some("rounds"))])
        );
        assert_eq!(std::fs::read_to_string(plan.dir.join("refused/rounds.jsonl")).unwrap(), refused);
        assert!(std::fs::read_to_string(&events).unwrap().ends_with("}\n"), "a whole last record keeps it");
        let saved = load(&mut store.reader(1_000_000), &plan).unwrap();
        let problems: Vec<&str> = saved.problems.keys().map(String::as_str).collect();
        assert_eq!(problems, ["closed", "demo", "open"], "the rootless ones start again from their roots");
        assert_eq!(saved.problems["open"].round, 3, "the next round is numbered after every node");
        // A run none of whose problems has a root yet (queued, or stopped
        // during its first root check) resumes from its roots.
        let mut rootless = plan.clone();
        rootless.problems = ["rootless", "refused", "never"].map(str::to_owned).to_vec();
        assert!(load(&mut store.reader(1_000_000), &rootless).unwrap().problems.is_empty());
    }

    #[test]
    fn lease_excludes_another_runtime_and_releases_on_close() {
        let store = Store::new();
        let first = lease(&store.0).unwrap();
        assert!(lease(&store.0).is_err());
        drop(first);
        // A test running beside this one may be starting a process: until
        // that child execs, it holds a copy of every descriptor, the lease's
        // too, so the lock can outlive `drop` by that moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lease(&store.0).is_err() {
            assert!(std::time::Instant::now() < deadline, "the lease was not released on close");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn checkpoint_and_lease_symlinks_are_refused() {
        let store = Store::new();
        store.line(&store.0.join("real"), json!({}));
        std::os::unix::fs::symlink(store.0.join("real"), store.0.join("link")).unwrap();
        assert!(store.reader(1000).read(&store.0.join("link")).is_err());
        std::os::unix::fs::symlink(store.0.join("real"), store.0.join(".session.lock")).unwrap();
        assert!(lease(&store.0).is_err());
    }
}
