//! Reading a run back: the pure half of `discovery problems` and `discovery
//! tree`. Everything here works on a problem's journal records — the
//! [`NodeRecord`]s of `nodes.jsonl` and the [`RoundRecord`]s of
//! `rounds.jsonl` — so a view shows exactly what the journal records; the
//! service (`registry::journal_of`) adds the rounds the run has not written
//! yet for a live problem before calling in, and never anything else.
//!
//! A journal file of a live problem may end in a torn line (a write in
//! progress); that last line is skipped.
//! A closed journal (an ended problem's) is never appended to again, so a
//! torn tail there is a failed write of the past and is reported as an
//! error, as is any other line the service cannot read or one of another
//! journal version — never a silently shorter tree: the journal is this
//! service's own writing.

use std::collections::BTreeMap;
use std::path::Path;

use nucleus_discovery_interface::{
    ProblemRow, ProblemState, TreeCheck, TreeNode, TreePolicy, TreeResult, TreeRound, TreeSketch, TreeWorker,
};
use nucleus_lean_interface::{CheckResponse, CheckStatus};
use serde::de::DeserializeOwned;

use super::{JOURNAL_VERSION, NodeRecord, PolicyKind, PolicyRecord, RoundRecord, SOLVED, excerpt};

/// One problem's journal as read back from its directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProblemJournal {
    pub nodes: Vec<NodeRecord>,
    pub rounds: Vec<RoundRecord>,
}

/// Reads `<dir>/nodes.jsonl` and `<dir>/rounds.jsonl` ([`read_jsonl`]),
/// beneath `base`. A missing directory or file is an empty journal: a
/// problem whose root could not be evaluated has a round record and no node.
/// `live` says the problem's rollout is still running, so a torn last line
/// may be a write in progress.
pub fn read_problem(base: &Path, dir: &Path, live: bool) -> Result<ProblemJournal, String> {
    Ok(ProblemJournal {
        nodes: read_jsonl(base, &dir.join("nodes.jsonl"), live)?,
        rounds: read_jsonl(base, &dir.join("rounds.jsonl"), live)?,
    })
}

/// The most bytes read of one journal file.
pub const MAX_JOURNAL_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The bytes of the regular file at `path`, at most `max_bytes`; `None` when
/// there is none. Every run's process can write under its project's output
/// directory, and the service reads there on its only thread, so what is
/// opened is checked, not the name: a symlink is refused by the open itself,
/// which does not wait for a FIFO's writer either, and anything but a regular
/// file or anything larger than `max_bytes` is refused once open.
pub fn read_regular(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, String> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => std::fs::File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let meta = file.metadata().map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > max_bytes {
        return Err(format!("{} is larger than {max_bytes} bytes", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{} is larger than {max_bytes} bytes", path.display()));
    }
    Ok(Some(bytes))
}

/// [`read_regular`] of `path`, which must lie beneath `base`, resolved from
/// `base` with no link followed on the way: a run's directory is written by
/// that run's process, and a link in it must not have the service read
/// another project's journal as this one's.
pub fn read_regular_beneath(base: &Path, path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, String> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    use rustix::io::Errno;
    use std::io::Read;
    let relative =
        path.strip_prefix(base).map_err(|_| format!("{} is not under {}", path.display(), base.display()))?;
    let root = rustix::fs::open(base, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("{}: {e}", base.display()))?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    let file = match rustix::fs::openat2(&root, relative, flags, Mode::empty(), resolve) {
        Ok(fd) => std::fs::File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::XDEV) => return Err(format!("{} crosses a link", path.display())),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let meta = file.metadata().map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > max_bytes {
        return Err(format!("{} is larger than {max_bytes} bytes", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{} is larger than {max_bytes} bytes", path.display()));
    }
    Ok(Some(bytes))
}

/// The records of one journal file, in file order. A missing file is empty.
/// Blank lines are skipped. Lines are split on the byte `\n`, so a line torn
/// inside a multi-byte character (Lean text is full of `⊢`, `→`, `≥`) is
/// just a line that does not parse, never an error for the whole file. With
/// `live`, the last non-blank line may be torn — a rollout's write in
/// progress — and is skipped when it does not parse; a closed journal is
/// never appended to again, so without `live` a torn last line is the error
/// it is. An unreadable line anywhere else, a line without `"v": 1`, or a
/// record that is not a `T` is an error naming the line.
pub fn read_jsonl<T: DeserializeOwned>(base: &Path, path: &Path, live: bool) -> Result<Vec<T>, String> {
    let Some(bytes) = read_regular_beneath(base, path, MAX_JOURNAL_FILE_BYTES)? else {
        return Ok(Vec::new());
    };
    let lines: Vec<(usize, &[u8])> = bytes
        .split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.iter().all(u8::is_ascii_whitespace))
        .collect();
    let last = lines.len().checked_sub(1);
    let mut records = Vec::with_capacity(lines.len());
    for (i, (n, line)) in lines.into_iter().enumerate() {
        let at = |what: &str| format!("{}: line {}: {what}", path.display(), n + 1);
        let value: serde_json::Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            // The torn tail of a file still being appended to.
            Err(_) if live && Some(i) == last => break,
            Err(e) if Some(i) == last => {
                return Err(at(&format!("{e} (a torn last line of a closed journal: a failed write)")));
            }
            Err(e) => return Err(at(&e.to_string())),
        };
        let version = value.get("v").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(JOURNAL_VERSION)) {
            return Err(at(&format!(
                "journal version {} is not {JOURNAL_VERSION}",
                version.map_or_else(|| "missing".to_owned(), |v| v.to_string())
            )));
        }
        records.push(serde_json::from_value(value).map_err(|e| at(&e.to_string()))?);
    }
    Ok(records)
}

/// The best score on record, if any node is.
fn best_of(nodes: &[NodeRecord]) -> Option<f64> {
    nodes.iter().map(|n| n.score).reduce(f64::max)
}

/// One row of `problems` from a problem's records: `best` over the nodes,
/// `rounds` the highest round number on record (an open round counts),
/// `nodes` the non-root ones, the tokens and the stop reason from the last
/// round record (the spend is cumulative), `solved` when a node reached
/// [`SOLVED`].
#[must_use]
pub fn summarize(problem: &str, state: ProblemState, journal: &ProblemJournal) -> ProblemRow {
    let best = best_of(&journal.nodes);
    let last = journal.rounds.last();
    ProblemRow {
        problem: problem.to_owned(),
        state,
        best,
        rounds: journal.rounds.iter().map(|r| u64::from(r.round)).max().unwrap_or(0),
        nodes: journal.nodes.iter().filter(|n| n.parent.is_some()).count() as u64,
        tokens_in: last.map_or(0, |r| r.tokens_in),
        tokens_out: last.map_or(0, |r| r.tokens_out),
        stop: last.and_then(|r| r.reason.clone()),
        solved: best.is_some_and(|b| b >= SOLVED),
    }
}

/// The `lean check` status as its wire name.
#[must_use]
pub fn status_name(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Invalid => "invalid",
        CheckStatus::Errors => "errors",
        CheckStatus::Sorries => "sorries",
        CheckStatus::Complete => "complete",
    }
}

fn check_of(check: &CheckResponse) -> TreeCheck {
    TreeCheck {
        status: status_name(check.status).to_owned(),
        errors: check.errors.len() as u64,
        sorries: check.sorries.len() as u64,
    }
}

/// A policy identity as the tree reports it.
#[must_use]
pub fn policy_of(policy: &PolicyRecord) -> TreePolicy {
    let kind = match policy.kind {
        PolicyKind::Builtin => "builtin",
        PolicyKind::Process => "process",
    };
    TreePolicy { kind: kind.to_owned(), program: policy.program.clone() }
}

/// A node's row: the record without its file, and an excerpt of the file
/// from the first line that differs from its parent's ([`excerpt`]; the
/// root's is the head of the frozen statement). A recorded sketch is
/// summarized: `hollow`, `closed` and `open` as the record has them.
#[must_use]
pub fn node_of(record: &NodeRecord, parent_file: &str) -> TreeNode {
    TreeNode {
        id: record.id,
        parent: record.parent,
        round: record.round,
        score: record.score,
        verified: record.verified,
        check: Some(check_of(&record.check)),
        sketch: record.sketch.as_ref().map(|s| TreeSketch {
            hollow: s.hollow,
            closed: s.counts.closed,
            open: s.counts.open,
        }),
        worker: record.worker.as_ref().map(|w| TreeWorker {
            task: record.task.clone().unwrap_or_default(),
            process: w.process,
            model_calls: u64::from(w.model_calls),
            input_tokens: w.input_tokens,
            output_tokens: w.output_tokens,
        }),
        excerpt: excerpt(parent_file, &record.file),
        ms_since_start: record.ms_since_start,
    }
}

/// A round's row: the record without its policy.
#[must_use]
pub fn round_of(record: &RoundRecord) -> TreeRound {
    TreeRound {
        round: record.round,
        batch: record.batch.clone(),
        children: record.children.clone(),
        failed: record.failed.clone(),
        reason: record.reason.clone(),
        policy_error: record.policy_error.clone(),
        tokens_in: record.tokens_in,
        tokens_out: record.tokens_out,
        usd: record.usd,
    }
}

/// The `tree` of a started problem from its records, bounded
/// ([`TreeResult::bound`]). `policy` is the run's, when the caller knows it;
/// otherwise the first round record's.
#[must_use]
pub fn tree_of(
    run: &str,
    problem: &str,
    state: ProblemState,
    journal: &ProblemJournal,
    policy: Option<&PolicyRecord>,
) -> TreeResult {
    let files: BTreeMap<u64, &str> = journal.nodes.iter().map(|n| (n.id, n.file.as_str())).collect();
    let nodes = journal
        .nodes
        .iter()
        .map(|n| node_of(n, n.parent.and_then(|p| files.get(&p)).copied().unwrap_or("")))
        .collect();
    let recorded = journal.rounds.iter().find_map(|r| r.policy.as_ref());
    let mut tree = TreeResult {
        run: run.to_owned(),
        problem: problem.to_owned(),
        state,
        nodes,
        rounds: journal.rounds.iter().map(round_of).collect(),
        policy: policy.or(recorded).map(policy_of),
        truncated: false,
    };
    tree.bound();
    tree
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{PolicySource, WorkerRecord};
    use nucleus_discovery_interface::{Failed, MAX_TREE_BYTES, TREE_EXCERPT_CHARS, scoring};
    use nucleus_lean_interface::OpenGoal;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nucleus_observe_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn check(status: CheckStatus, sorries: usize) -> CheckResponse {
        CheckResponse {
            status,
            invalid_reason: None,
            errors: Vec::new(),
            sorries: (0..sorries)
                .map(|_| OpenGoal { line: 4, column: 0, goal: "⊢ n + 0 = n".into() })
                .collect(),
            elapsed_ms: 80,
        }
    }

    fn node(id: u64, parent: Option<u64>, round: u32, file: &str, score: f64) -> NodeRecord {
        let complete = score >= SOLVED;
        NodeRecord {
            v: JOURNAL_VERSION,
            id,
            parent,
            round,
            task: parent.map(|_| format!("t{id:06}")),
            file: file.to_owned(),
            check: if complete { check(CheckStatus::Complete, 0) } else { check(CheckStatus::Sorries, 1) },
            verified: complete.then_some(true),
            verify_rejection: None,
            score,
            scoring: scoring::VERSION,
            sketch: None,
            completion: None,
            errors_obs: None,
            analysis_failed: None,
            worker: parent.map(|_| WorkerRecord {
                process: 40 + id,
                model_calls: 2,
                input_tokens: 18,
                output_tokens: 8,
            }),
            ms_since_start: id * 1000,
        }
    }

    fn round(n: u32, batch: &[u64], children: &[u64], tokens_in: u64, reason: Option<&str>) -> RoundRecord {
        RoundRecord {
            v: JOURNAL_VERSION,
            round: n,
            policy: Some(PolicySource::Builtin.record()),
            batch: batch.to_vec(),
            children: children.to_vec(),
            failed: Vec::new(),
            tokens_in,
            tokens_out: tokens_in / 2,
            usd: Some(tokens_in as f64 * 1e-6),
            reason: reason.map(str::to_owned),
            policy_error: None,
        }
    }

    const FROZEN: &str = "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n :=\nsorry\n";
    const PARTIAL: &str = "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n := by\n  have h : n + 0 = n := by simp\n  sorry";
    const PROOF: &str = "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n :=\n by simp";

    /// The scripted e2e's demo_a: two rounds, solved in the second.
    fn solved_journal() -> ProblemJournal {
        ProblemJournal {
            nodes: vec![
                node(0, None, 0, FROZEN, 0.5),
                node(1, Some(0), 1, PARTIAL, 0.5),
                node(2, Some(0), 1, PARTIAL, 0.5),
                node(3, Some(0), 2, PROOF, 1.0),
                node(4, Some(1), 2, PARTIAL, 0.5),
            ],
            rounds: vec![
                round(1, &[0, 0], &[1, 2], 36, None),
                round(2, &[0, 1], &[3, 4], 63, Some("solved")),
            ],
        }
    }

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn a_journal_is_read_back_and_a_torn_last_line_is_skipped() {
        let dir = scratch("torn");
        let journal = solved_journal();
        let mut nodes = String::new();
        for n in &journal.nodes {
            nodes.push_str(&serde_json::to_string(n).unwrap());
            nodes.push('\n');
        }
        let mut rounds = String::new();
        for r in &journal.rounds {
            rounds.push_str(&serde_json::to_string(r).unwrap());
            rounds.push('\n');
        }
        write(&dir, "nodes.jsonl", &nodes);
        write(&dir, "rounds.jsonl", &rounds);
        for live in [true, false] {
            assert_eq!(
                read_problem(&dir, &dir, live).unwrap(),
                journal,
                "a complete journal reads back whole"
            );
        }

        // A write in progress: the last line is cut mid-record, with and
        // without a newline before it, with blank lines in between.
        let torn = format!("{nodes}\n{{\"v\":1,\"id\":5,\"par");
        write(&dir, "nodes.jsonl", &torn);
        assert_eq!(read_problem(&dir, &dir, true).unwrap().nodes, journal.nodes, "the torn tail is skipped");
        let torn_round = format!("{rounds}{}", &serde_json::to_string(&journal.rounds[1]).unwrap()[..20]);
        write(&dir, "rounds.jsonl", &torn_round);
        assert_eq!(read_problem(&dir, &dir, true).unwrap().rounds, journal.rounds);
        // The same tail on a closed journal is a failed write: an error, not a shorter tree.
        let err = read_problem(&dir, &dir, false).unwrap_err();
        assert!(err.contains("nodes.jsonl: line 7:") && err.contains("closed journal"), "{err}");
        write(&dir, "nodes.jsonl", &nodes);
        let err = read_problem(&dir, &dir, false).unwrap_err();
        assert!(err.contains("rounds.jsonl: line 3:"), "{err}");
        write(&dir, "rounds.jsonl", &rounds);

        // Torn inside a multi-byte character (the goal's `⊢` is three bytes):
        // still just a torn line, skipped live and reported closed.
        let last = serde_json::to_string(&journal.nodes[4]).unwrap();
        let cut = last.find('⊢').unwrap() + 1;
        let torn_utf8 = format!("{nodes}{}", &last.as_bytes()[..cut].escape_ascii());
        let mut file = nodes.clone().into_bytes();
        file.extend_from_slice(&last.as_bytes()[..cut]);
        assert!(String::from_utf8(file.clone()).is_err(), "the fixture is not UTF-8: {torn_utf8}");
        std::fs::write(dir.join("nodes.jsonl"), &file).unwrap();
        assert_eq!(
            read_problem(&dir, &dir, true).unwrap().nodes,
            journal.nodes,
            "skipped like any torn tail"
        );
        let err = read_problem(&dir, &dir, false).unwrap_err();
        assert!(err.contains("nodes.jsonl: line 6:"), "{err}");

        // A torn line elsewhere is corruption, named by its line, live or not.
        let broken = format!("{}\n{{\"v\":1,\"id\":\n{}\n", nodes.trim_end(), nodes.lines().next().unwrap());
        write(&dir, "nodes.jsonl", &broken);
        for live in [true, false] {
            let err = read_problem(&dir, &dir, live).unwrap_err();
            assert!(err.contains("nodes.jsonl: line 6:"), "{err}");
        }
        // Another journal version is refused, never reinterpreted; so is a
        // record that is not a node.
        write(&dir, "nodes.jsonl", "{\"v\":2,\"id\":0}\n");
        let err = read_problem(&dir, &dir, true).unwrap_err();
        assert!(err.contains("journal version 2 is not 1"), "{err}");
        write(&dir, "nodes.jsonl", "{\"id\":0}\n");
        assert!(read_problem(&dir, &dir, true).unwrap_err().contains("version missing"));
        write(&dir, "nodes.jsonl", "{\"v\":1,\"id\":0}\n{\"v\":1,\"id\":0}\n");
        let err = read_problem(&dir, &dir, true).unwrap_err();
        assert!(err.contains("line 1:") && err.contains("missing field"), "{err}");

        // What a run's process may leave in place of a journal file is refused
        // at once: a symlink is not followed, a FIFO nobody writes to is not
        // waited on, and a file above the bound is not read.
        let nodes_file = dir.join("nodes.jsonl");
        std::fs::write(dir.join("elsewhere"), "").unwrap();
        std::fs::remove_file(&nodes_file).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &nodes_file).unwrap();
        assert!(read_problem(&dir, &dir, true).is_err(), "a symlink is refused, not followed");
        std::fs::remove_file(&nodes_file).unwrap();
        let (fifo, mode) = (rustix::fs::FileType::Fifo, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR);
        rustix::fs::mknodat(rustix::fs::CWD, &nodes_file, fifo, mode, 0).unwrap();
        assert!(read_problem(&dir, &dir, true).unwrap_err().contains("not a regular file"));
        std::fs::remove_file(&nodes_file).unwrap();
        write(&dir, "nodes.jsonl", "{}\n");
        assert!(read_regular(&nodes_file, 2).unwrap_err().contains("larger than 2 bytes"));
        // Nor is a link on the way to one: a run's directory is written by its
        // project's run processes, which must not have the service read
        // another project's journal as theirs.
        std::os::unix::fs::symlink(&dir, dir.join("linked")).unwrap();
        assert!(read_problem(&dir, &dir.join("linked"), true).unwrap_err().contains("crosses a link"));
        std::fs::remove_file(dir.join("linked")).unwrap();

        // Missing files and directories are empty journals.
        std::fs::remove_file(dir.join("nodes.jsonl")).unwrap();
        assert_eq!(read_problem(&dir, &dir, false).unwrap().nodes, []);
        assert_eq!(read_problem(&dir, &dir.join("never"), false).unwrap(), ProblemJournal::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_problem_row_summarizes_its_records() {
        let journal = solved_journal();
        let row = summarize("demo_a", ProblemState::Done, &journal);
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            json!({"problem": "demo_a", "state": "done", "best": 1.0, "rounds": 2, "nodes": 4,
                   "tokens_in": 63, "tokens_out": 31, "stop": "solved", "solved": true})
        );
        // Live: the open round has no reason yet; its cumulative spend is the row's.
        let live = ProblemJournal {
            nodes: journal.nodes[..3].to_vec(),
            rounds: vec![journal.rounds[0].clone(), round(2, &[0, 1], &[], 45, None)],
        };
        let row = summarize("demo_a", ProblemState::Active, &live);
        assert_eq!(
            (row.best, row.rounds, row.nodes, row.tokens_in, row.stop, row.solved),
            (Some(0.5), 2, 2, 45, None, false)
        );
        // A root whose evaluation failed: one round-0 record, no node.
        let failed = ProblemJournal {
            nodes: vec![],
            rounds: vec![round(0, &[], &[], 0, Some("failed: statement: not_found: demo_zzz"))],
        };
        let row = summarize("demo_zzz", ProblemState::Done, &failed);
        assert_eq!((row.best, row.rounds, row.nodes), (None, 0, 0));
        assert_eq!(row.stop.as_deref(), Some("failed: statement: not_found: demo_zzz"));
        assert!(!row.solved);
        // Nothing on record yet (a rollout that has just started).
        let row = summarize("demo_a", ProblemState::Active, &ProblemJournal::default());
        assert_eq!(row, ProblemRow { state: ProblemState::Active, ..ProblemRow::queued("demo_a") });
    }

    #[test]
    fn a_tree_carries_the_nodes_with_excerpts_the_rounds_and_the_policy() {
        let journal = solved_journal();
        let tree = tree_of("demo", "demo_a", ProblemState::Done, &journal, None);
        assert_eq!(
            (tree.run.as_str(), tree.problem.as_str(), tree.state),
            ("demo", "demo_a", ProblemState::Done)
        );
        assert_eq!((tree.nodes.len(), tree.rounds.len(), tree.truncated), (5, 2, false));
        let root = &tree.nodes[0];
        assert_eq!((root.id, root.parent, root.round, root.score, root.verified), (0, None, 0, 0.5, None));
        assert!(root.worker.is_none());
        assert_eq!(root.check, Some(TreeCheck { status: "sorries".into(), errors: 0, sorries: 1 }));
        assert_eq!(root.excerpt, FROZEN.trim_end(), "the root's excerpt is the head of the statement");
        let child = &tree.nodes[1];
        assert_eq!(
            child.excerpt,
            "theorem demo (n : Nat) : n + 0 = n := by\n  have h : n + 0 = n := by simp\n  sorry",
            "from the first line that differs from the parent"
        );
        assert_eq!(
            child.worker,
            Some(TreeWorker {
                task: "t000001".into(),
                process: 41,
                model_calls: 2,
                input_tokens: 18,
                output_tokens: 8
            })
        );
        let solved = &tree.nodes[3];
        assert_eq!(
            (solved.parent, solved.round, solved.score, solved.verified),
            (Some(0), 2, 1.0, Some(true))
        );
        assert_eq!(solved.check.as_ref().unwrap().status, "complete");
        assert_eq!(solved.excerpt, " by simp", "the theorem line is the parent's; only the proof differs");
        assert_eq!(tree.nodes[4].excerpt, PARTIAL, "identical to its parent (node 1): the head of the file");
        assert_eq!(tree.nodes[4].ms_since_start, 4000);
        let r2 = &tree.rounds[1];
        assert_eq!(
            (r2.round, &r2.batch, &r2.children, r2.reason.as_deref()),
            (2, &vec![0, 1], &vec![3, 4], Some("solved"))
        );
        assert_eq!((r2.tokens_in, r2.tokens_out, r2.usd), (63, 31, Some(63e-6)));
        assert!(tree.rounds[0].reason.is_none() && tree.rounds[0].policy_error.is_none());
        assert_eq!(
            tree.policy,
            Some(TreePolicy { kind: "builtin".into(), program: None }),
            "from the records"
        );
        let process = PolicyRecord { kind: PolicyKind::Process, program: Some("/policies/pi0.py".into()) };
        let tree = tree_of("demo", "demo_a", ProblemState::Done, &journal, Some(&process));
        assert_eq!(
            tree.policy,
            Some(TreePolicy { kind: "process".into(), program: Some("/policies/pi0.py".into()) }),
            "the run's, when known"
        );
        assert_eq!(
            serde_json::to_value(&tree).unwrap()["policy"],
            json!({"kind": "process", "program": "/policies/pi0.py"})
        );
        // The wire names are serde's.
        for status in [CheckStatus::Invalid, CheckStatus::Errors, CheckStatus::Sorries, CheckStatus::Complete]
        {
            assert_eq!(json!(status_name(status)), serde_json::to_value(status).unwrap());
        }
        for kind in [PolicyKind::Builtin, PolicyKind::Process] {
            let policy = PolicyRecord { kind, program: None };
            assert_eq!(json!(policy_of(&policy).kind), serde_json::to_value(kind).unwrap());
        }
        // Failed continuations and a policy error travel as recorded.
        let mut failed = round(1, &[0], &[], 0, Some("policy_failed"));
        failed.failed.push(Failed { task: "t000009".into(), why: "timed_out".into() });
        failed.policy_error = Some("exited with 3".into());
        let row = round_of(&failed);
        assert_eq!(row.failed, [Failed { task: "t000009".into(), why: "timed_out".into() }]);
        assert_eq!(row.policy_error.as_deref(), Some("exited with 3"));
    }

    #[test]
    fn a_tree_is_bounded_in_bytes_and_excerpt_length() {
        // 60 rounds of 20 children with long files: far more than the reply
        // bound; the reply is the first rounds whole, within the bound, and
        // says it was cut. (A tree this size needs a task whose call limit
        // allows 1200 continuations.)
        let long = format!("{FROZEN}{}", "x".repeat(1000));
        let mut nodes = vec![node(0, None, 0, FROZEN, 0.5)];
        nodes.extend((1..=1200_u64).map(|id| node(id, Some(0), ((id - 1) / 20) as u32 + 1, &long, 0.5)));
        let rounds = (1..=60_u32).map(|r| round(r, &[0], &[1], 0, None)).collect();
        let tree = tree_of("demo", "big", ProblemState::Active, &ProblemJournal { nodes, rounds }, None);
        let encoded = serde_json::to_vec(&tree).unwrap().len();
        assert!(tree.truncated && encoded <= MAX_TREE_BYTES, "{encoded} bytes");
        assert!(tree.nodes.len() > 100 && tree.nodes.len() < 1201, "{} nodes", tree.nodes.len());
        assert!(!tree.rounds.is_empty() && tree.rounds.len() < 60, "{} rounds", tree.rounds.len());
        let ids: Vec<u64> = tree.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, (0..ids.len() as u64).collect::<Vec<_>>(), "the first ones, in creation order");
        let last_round = tree.rounds.last().unwrap().round;
        assert!(
            tree.nodes.iter().filter(|n| n.round == last_round).count() == 20,
            "its children are all there"
        );
        assert!(tree.nodes.iter().all(|n| n.excerpt.chars().count() <= TREE_EXCERPT_CHARS));
        assert_eq!(tree.nodes[1].excerpt.chars().count(), TREE_EXCERPT_CHARS);
    }
}
