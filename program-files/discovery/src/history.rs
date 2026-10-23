//! The runs an earlier instance of the service left under `--out`, each in
//! its project's directory `<out>/<project>/<run>`, listed again when the
//! service starts. The journals on disk are the durable
//! record; the registry in memory is not, so without this a restart hides
//! every earlier run from `list` although `resume` could still continue it.
//!
//! A run is read back as ended: `done` when every problem's last round
//! record carries a stop reason, `stopped` when one of them was terminated,
//! when a problem never started, or when a process went away before its
//! journals were closed (then `resume` closes and continues them). Counts
//! and tokens come from those round records. A run is listed whatever the
//! service's ceilings are now; `resume` checks them.
use super::*;
use crate::registry::{Phase, Registry, Run};

/// The most bytes read of one run's `task.json` or one problem's round record file.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn restore(options: &Options, runs: &mut Registry) {
    let mut found = Vec::new();
    // A directory entry's type is the entry's own, never a link's target.
    let directories = |dir: &Path| -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|entry| entry.file_name().to_str().map(|name| (name.to_owned(), entry.path())))
            .filter(|(name, _)| discovery::valid_name(name))
            .collect()
    };
    for (project, dir) in directories(&options.out) {
        for (_, path) in directories(&dir) {
            match ended(options, &project, &path) {
                Ok(Some(run)) => found.push(run),
                Ok(None) => {}
                Err(why) => eprintln!("nucleus-discovery: not listing {}: {why}", path.display()),
            }
        }
    }
    found.sort_by_key(|(when, _)| *when);
    for (_, run) in found {
        runs.push(run);
    }
}

/// A run's file as [`observe::read_regular_beneath`] reads it beneath the
/// output directory: what any run's process may have put there is not
/// trusted to be a regular file, nor reached without a link.
fn read(options: &Options, path: &Path) -> Result<Vec<u8>, String> {
    observe::read_regular_beneath(&options.out, path, MAX_FILE_BYTES)?
        .ok_or_else(|| format!("{}: not found", path.display()))
}

/// The run of `project` in `dir`, ended; `None` when `dir` holds no task.
fn ended(options: &Options, project: &str, dir: &Path) -> Result<Option<(SystemTime, Run)>, String> {
    let task_path = dir.join(TASK_FILE);
    let Some(bytes) = observe::read_regular_beneath(&options.out, &task_path, MAX_FILE_BYTES)? else {
        return Ok(None);
    };
    let when = std::fs::symlink_metadata(&task_path).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
    let task: Task = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if dir.file_name().and_then(|n| n.to_str()) != Some(task.name.as_str()) {
        return Err("the directory is not named after its task".to_owned());
    }
    let any = Ceilings {
        max_concurrency: usize::MAX,
        max_usd_micro_per_problem: 0,
        max_problems_per_task: usize::MAX,
        max_width: usize::MAX,
    };
    let out = options.out.join(project);
    let plan = resolve_task_mode(&task, options, &out, &any, true).map_err(|e| e.message)?;
    let (mut finished, mut solved, mut spend) = (0u64, 0u64, Spend::default());
    let (mut open, mut terminated, mut started) = (false, false, 0u64);
    for (i, problem) in plan.problems.iter().enumerate() {
        let path = plan.dir.join(problem).join(ROUNDS_FILE);
        if !path.is_file() {
            continue;
        }
        // Problems start in task order: this one and every one before it did.
        started = i as u64 + 1;
        let bytes = read(options, &path)?;
        let last: Option<RoundRecord> = match bytes.split(|b| *b == b'\n').rev().find(|l| !l.is_empty()) {
            // A last line that does not read is a write cut short: the
            // journal is open.
            Some(line) => serde_json::from_slice(line).ok(),
            None => None,
        };
        let Some(last) = last else {
            open = true;
            continue;
        };
        // Cumulative through the round: the last record carries the rollout's total.
        spend.tokens_in = spend.tokens_in.saturating_add(last.tokens_in);
        spend.tokens_out = spend.tokens_out.saturating_add(last.tokens_out);
        match last.reason.as_deref() {
            None => open = true,
            Some(reason) => {
                finished += 1;
                solved += u64::from(reason == "solved");
                terminated |= reason == "terminated";
            }
        }
    }
    let unstarted = started < plan.problems.len() as u64;
    let (state, error) = match (open, terminated || unstarted) {
        (true, _) => (
            RunState::Stopped,
            Some("the run's process ended before the run did; `resume` continues it".to_owned()),
        ),
        (false, true) => (RunState::Stopped, None),
        (false, false) => (RunState::Done, None),
    };
    let status = RunStatus {
        run: plan.name.clone(),
        state,
        problems: plan.problems.len() as u64,
        finished,
        solved,
        active: Vec::new(),
        tokens_in: spend.tokens_in,
        tokens_out: spend.tokens_out,
        usd: None,
        error,
        task: TaskShape::of(&plan.document),
        failed_continuations: 0,
        last_failure: None,
        started_at_ms: None,
        failure_reasons: Vec::new(),
        other_failures: 0,
    };
    let mut run = Run::new(project, plan, status, Phase::Ended { process: None }, open);
    run.started = started;
    Ok(Some((when, run)))
}
