//! An exploration policy that lives in another process, driven over a line
//! protocol.
//!
//! The policy is an arbitrary program. It holds no capability: this module
//! gives it nothing (no inherited environment, no argument it did not get
//! from its spawner), and how it is confined is the spawner's choice (the
//! discovery harness runs it inside its run process's sandbox). What this
//! module guarantees is that everything the program can do through *this*
//! channel is choose a batch, answer nonsense, or hang — and each of those is
//! a stop of the rollout, never a hang of the driver.
//!
//! # Protocol, version 1
//!
//! Line-oriented JSON over the policy's stdin/stdout: for every decision the
//! driver writes **one line** and reads **one line**. Key order in an object is
//! not significant.
//!
//! **Request** (driver → policy's stdin):
//!
//! ```json
//! {"v":1,"round":2,"w":4,
//!  "revealed":[{"id":0,"score":0.0,"parent":null,"depth":0},
//!              {"id":1,"score":0.5,"parent":0,"depth":1}],
//!  "continuable":[1,0]}
//! ```
//!
//! - `v`: protocol version, `1`. A policy must refuse a version it does not
//!   know (exit, or answer an empty batch) rather than guess.
//! - `round`: 1-based count of decisions asked of this policy process. One
//!   process serves one rollout, so `round` is the rollout's decision round.
//! - `w`: the most continuations this round may hold.
//! - `revealed`: every revealed node exactly once, in the harness's order: its `id`, `score` (a finite JSON
//!   number, larger is better), `parent` (`null` for the root, which is always
//!   node `0`) and `depth` (root `0`, child = parent + 1; computed by the
//!   driver from the parents). A node that has not been revealed does not
//!   appear, so a policy cannot learn an outcome it has not paid for.
//! - `continuable`: the ids the harness lets be continued now (the root and the
//!   leaves), sorted by (score desc, depth asc, id asc). The driver sorts,
//!   whatever order the harness used.
//! - Optional, per revealed node, when the record carries it (additive; a
//!   policy that ignores them keeps working):
//!   `check`: `{"status": "invalid"|"errors"|"sorries"|"complete",
//!   "errors": n, "sorries": n}` — Lean's verdict on the node's file, so a
//!   policy can tell a restated theorem from a decomposed one at equal
//!   score; `tokens`: `{"in": n, "out": n}` — the worker's spend (the root
//!   has none). These are what the harness recorded.
//! - Optional too, per revealed node, under task-scoring protocol v3
//!   (additive, absent means unknown): `sketch`: `{"hollow": true|false|
//!   null, "false_leaf": b, "restates_root": b, "closed": n, "open": n,
//!   "auto_closable": n, "analysis_failed": b}` — the `lean` service's
//!   analysis of a partial proof (`hollow` is `null` when it did not run);
//!   `errors_obs`: `{"repairable": b, "classes": [class, …],
//!   "first_error_fraction": f|null}` for an `errors` node; `delta`:
//!   `{"sorries": ±n, "errors": ±n}` against the parent's verdict;
//!   `verify_rejection`: why a complete candidate was rejected. The
//!   [`super::Observation`] fields they come from are documented there; an
//!   `errors_obs` class is one of `nucleus_discovery_interface::scoring::ERROR_CLASSES`.
//!
//! **Reply** (policy's stdout → driver):
//!
//! ```json
//! {"batch":[1,1,0]}
//! ```
//!
//! - `batch`: ids to CONTINUE this round, in order; a repeated id asks for
//!   several continuations of that node. An empty batch means **stop**.
//! - Ids not in `continuable` are dropped by the driver; the harness would
//!   refuse them anyway (its `admit`), this just keeps that refusal explicit. The remaining order is kept; the driver applies `w`.
//! - Other keys are ignored, so a policy may add its own notes. They are
//!   untrusted text and are not logged here.
//! - Exactly one line per request, UTF-8, at most [`MAX_REPLY_BYTES`], flushed,
//!   within the configured timeout of the request line. A policy must not
//!   write when no request is pending.
//!
//! **Failure is a stop.** A late reply (timeout), a malformed, oversized or
//! stray line, a policy that exits or closes stdout, or a request that cannot
//! be written: the decision is an empty batch, the process is killed and
//! reaped, [`ProcessPolicy::last_error`] says why, and every later decision of
//! this [`ProcessPolicy`] is an empty batch too. A rollout whose policy failed
//! is over; a new tree gets a new process.
//!
//! **Lifetime.** One [`ProcessPolicy`] is one process is one rollout. The
//! process is killed when the policy is dropped. This module kills the process
//! it spawned; anything that process spawned in turn is the spawner's problem.
//! The captured tail of the policy's stderr (at most [`STDERR_TAIL_BYTES`]) is
//! attached to errors for debugging the policy; it too is untrusted text.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use super::{NodeId, Policy, Score, View};

/// Version of the request/reply line format this module speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// Longest reply line accepted, in bytes; a longer one is a protocol failure.
pub const MAX_REPLY_BYTES: usize = 1 << 20;
/// The limit the reader applies: small in tests, so that an
/// oversized reply costs them kilobytes, not a megabyte.
const REPLY_LIMIT: usize = if cfg!(test) { 4096 } else { MAX_REPLY_BYTES };
/// How much of the policy's stderr is kept for error reports.
pub const STDERR_TAIL_BYTES: usize = 4096;
/// How much of an offending reply an error report quotes.
const QUOTED_REPLY_CHARS: usize = 200;
/// How long, after the process is reaped, to wait for its stderr to drain
/// before an error report is assembled. Bounded so that a descendant holding
/// the pipe cannot hang a stop.
const STDERR_SETTLE: Duration = Duration::from_millis(500);

/// Reply lines from the stdout reader thread; `Err` is a read failure or an
/// oversized line, after which the reader stops.
type Lines = Receiver<io::Result<Vec<u8>>>;
/// A child's stdin, its reply lines, its stderr tail and the tail's
/// "drained" signal, as [`ProcessPolicy::wire`] hands them over.
type Wired = (ChildStdin, Lines, Arc<Mutex<Vec<u8>>>, Receiver<()>);

/// How to start a policy program. The caller decides how it is confined; this
/// module never inherits its own environment into the child (`env_clear`, then
/// exactly `env`). `program` should be an absolute path: with a cleared
/// environment there is no `PATH` to search unless `env` supplies one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyProgram {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Deadline for each reply, measured from the request line.
    pub timeout: Duration,
}

/// Why a policy process stopped serving decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// The shape handed in does not describe the view (length mismatch, a
    /// parent that is not revealed, a cycle).
    BadStructure(String),
    /// Writing the request line failed: the policy closed stdin or exited.
    Write { round: u64, error: String, stderr: String },
    /// No reply within the deadline; the policy was killed.
    Timeout { round: u64, timeout: Duration, stderr: String },
    /// The policy exited or closed stdout before replying.
    Exited { round: u64, status: Option<String>, stderr: String },
    /// A reply that is not one line of `{"batch":[ids]}`: malformed, oversized,
    /// not UTF-8, or a line written while no request was pending.
    Malformed { round: u64, why: String, reply: String, stderr: String },
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::BadStructure(why) => write!(f, "revealed tree's parents are inconsistent: {why}"),
            PolicyError::Write { round, error, stderr } => {
                write!(f, "round {round}: writing the request failed: {error}{}", tail(stderr))
            }
            PolicyError::Timeout { round, timeout, stderr } => {
                write!(f, "round {round}: no reply within {timeout:?}; policy killed{}", tail(stderr))
            }
            PolicyError::Exited { round, status, stderr } => {
                let status = status.as_deref().unwrap_or("unknown");
                write!(f, "round {round}: policy exited before replying (status {status}){}", tail(stderr))
            }
            PolicyError::Malformed { round, why, reply, stderr } => {
                write!(f, "round {round}: malformed reply: {why}: {reply:?}{}", tail(stderr))
            }
        }
    }
}

impl PolicyError {
    /// The same error with the policy's stderr tail attached (process-side
    /// variants only).
    fn with_stderr(self, tail: String) -> Self {
        match self {
            PolicyError::Write { round, error, .. } => PolicyError::Write { round, error, stderr: tail },
            PolicyError::Timeout { round, timeout, .. } => {
                PolicyError::Timeout { round, timeout, stderr: tail }
            }
            PolicyError::Exited { round, status, .. } => PolicyError::Exited { round, status, stderr: tail },
            PolicyError::Malformed { round, why, reply, .. } => {
                PolicyError::Malformed { round, why, reply, stderr: tail }
            }
            other @ PolicyError::BadStructure(_) => other,
        }
    }
}

fn tail(stderr: &str) -> String {
    if stderr.is_empty() { String::new() } else { format!("; stderr tail: {stderr:?}") }
}

/// Depth of every revealed node from its parent: root 0, child = parent + 1.
/// `parents[i]` belongs to `revealed[i]`. Fails on a length mismatch, a parent
/// that is not itself revealed, or a cycle.
pub fn depths(revealed: &[(NodeId, Score)], parents: &[Option<NodeId>]) -> Result<Vec<usize>, PolicyError> {
    if revealed.len() != parents.len() {
        return Err(PolicyError::BadStructure(format!(
            "{} revealed nodes but {} parents",
            revealed.len(),
            parents.len()
        )));
    }
    let index: HashMap<NodeId, usize> = revealed.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    let mut depth: Vec<Option<usize>> = vec![None; revealed.len()];
    for start in 0..revealed.len() {
        // Walk up to the first node whose depth is known (or the root), then
        // assign depths back down the chain.
        let mut chain = Vec::new();
        let mut cur = start;
        let base = loop {
            if let Some(d) = depth[cur] {
                break d;
            }
            let Some(parent) = parents[cur] else {
                depth[cur] = Some(0);
                break 0;
            };
            let Some(&up) = index.get(&parent) else {
                return Err(PolicyError::BadStructure(format!(
                    "node {} has parent {parent}, which is not revealed",
                    revealed[cur].0
                )));
            };
            chain.push(cur);
            if chain.len() > revealed.len() {
                return Err(PolicyError::BadStructure(format!("cycle through node {}", revealed[cur].0)));
            }
            cur = up;
        };
        for (k, &i) in chain.iter().rev().enumerate() {
            depth[i] = Some(base + 1 + k);
        }
    }
    Ok(depth.into_iter().map(|d| d.unwrap_or(0)).collect())
}

/// One request line, version 1. `round` is the decision counter.
pub fn encode_request(
    view: &View,
    parents: &[Option<NodeId>],
    round: u64,
    w: usize,
) -> Result<String, PolicyError> {
    let depth = depths(&view.revealed, parents)?;
    let by_id: HashMap<NodeId, (Score, usize)> =
        view.revealed.iter().zip(&depth).map(|((id, s), d)| (*id, (*s, *d))).collect();
    let observations = view.observations.iter().map(Some).chain(std::iter::repeat(None));
    let revealed: Vec<serde_json::Value> = view
        .revealed
        .iter()
        .zip(parents)
        .zip(&depth)
        .zip(observations)
        .map(|((((id, score), parent), depth), observation)| {
            let mut node = serde_json::json!({ "id": id, "score": score, "parent": parent, "depth": depth });
            // Optional, additive (protocol v1): what the harness observed, when recorded.
            if let Some(Some(o)) = observation {
                node["check"] =
                    serde_json::json!({ "status": o.status, "errors": o.errors, "sorries": o.sorries });
                node["tokens"] = serde_json::json!({ "in": o.tokens_in, "out": o.tokens_out });
                // Task-scoring protocol v3 observations, additive too: a policy
                // treats a missing field as unknown.
                if let Some(s) = &o.sketch {
                    node["sketch"] = serde_json::json!({
                        "hollow": s.hollow,
                        "false_leaf": s.false_leaf,
                        "restates_root": s.restates_root,
                        "closed": s.closed,
                        "open": s.open,
                        "auto_closable": s.auto_closable,
                        "analysis_failed": s.analysis_failed,
                    });
                }
                if let Some(e) = &o.errors_obs {
                    node["errors_obs"] = serde_json::json!({
                        "repairable": e.repairable,
                        "classes": e.classes,
                        "first_error_fraction": e.first_error_fraction,
                    });
                }
                if let Some((sorries, errors)) = o.delta_vs_parent {
                    node["delta"] = serde_json::json!({ "sorries": sorries, "errors": errors });
                }
                if let Some(why) = &o.verify_rejection {
                    node["verify_rejection"] = serde_json::json!(why);
                }
            }
            node
        })
        .collect();
    let mut continuable = view.continuable.clone();
    continuable.sort_by(|a, b| {
        let (sa, da) = by_id.get(a).copied().unwrap_or((Score::MIN, usize::MAX));
        let (sb, db) = by_id.get(b).copied().unwrap_or((Score::MIN, usize::MAX));
        sb.total_cmp(&sa).then(da.cmp(&db)).then(a.cmp(b))
    });
    let request = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "round": round,
        "w": w,
        "revealed": revealed,
        "continuable": continuable,
    });
    serde_json::to_string(&request)
        .map_err(|e| PolicyError::BadStructure(format!("request not serialisable: {e}")))
}

/// The batch of one reply line, unfiltered. `Err` carries why it is malformed.
pub fn decode_reply(line: &str) -> Result<Vec<NodeId>, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let object = value.as_object().ok_or("not a JSON object")?;
    let batch = object.get("batch").ok_or("no \"batch\" key")?;
    let items = batch.as_array().ok_or("\"batch\" is not an array")?;
    items
        .iter()
        .map(|item| {
            let id = item.as_u64().ok_or_else(|| format!("batch entry {item} is not a node id"))?;
            usize::try_from(id).map_err(|_| format!("batch entry {id} does not fit a node id"))
        })
        .collect()
}

/// The live process and its channels.
struct Running {
    child: Child,
    stdin: ChildStdin,
    lines: Lines,
    /// A line already taken off `lines` while no request was pending; only
    /// tests take one early (`ProcessPolicy::await_line`).
    early: Option<io::Result<Vec<u8>>>,
    /// The last [`STDERR_TAIL_BYTES`] of the policy's stderr, kept by a reader
    /// thread; `stderr_done` disconnects when that thread has drained the pipe.
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_done: Receiver<()>,
}

/// A [`Policy`] served by an external program over the protocol above.
pub struct ProcessPolicy {
    running: Option<Running>,
    timeout: Duration,
    round: u64,
    last_error: Option<PolicyError>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProcessPolicy {
    /// Start the policy program. Fails if it cannot be spawned (a missing
    /// interpreter is a configuration error, reported here, not a stop).
    pub fn spawn(program: &PolicyProgram) -> io::Result<Self> {
        let mut command = Command::new(&program.program);
        command
            .args(&program.args)
            .env_clear()
            .envs(program.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &program.cwd {
            command.current_dir(cwd);
        }
        let running = Self::attach(command.spawn()?)?;
        Ok(ProcessPolicy { running: Some(running), timeout: program.timeout, round: 0, last_error: None })
    }

    /// Take the child's pipes and start the two reader threads; on failure
    /// the child is killed and reaped before the error is returned.
    fn attach(mut child: Child) -> io::Result<Running> {
        match Self::wire(&mut child) {
            Ok((stdin, lines, stderr, stderr_done)) => {
                Ok(Running { child, stdin, lines, early: None, stderr, stderr_done })
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    fn wire(child: &mut Child) -> io::Result<Wired> {
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("policy stdin not piped"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("policy stdout not piped"))?;
        let mut stderr = child.stderr.take().ok_or_else(|| io::Error::other("policy stderr not piped"))?;

        let (tx, lines) = mpsc::channel();
        thread::Builder::new().name("policy-stdout".into()).spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut buf = Vec::new();
                let limit = REPLY_LIMIT as u64 + 1;
                match (&mut reader).take(limit).read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
                if buf.len() > REPLY_LIMIT {
                    let why = format!("reply longer than {REPLY_LIMIT} bytes");
                    let _ = tx.send(Err(io::Error::new(io::ErrorKind::InvalidData, why)));
                    break;
                }
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if tx.send(Ok(buf)).is_err() {
                    break;
                }
            }
        })?;

        let tail = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&tail);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::Builder::new().name("policy-stderr".into()).spawn(move || {
            let _disconnects_when_done = done_tx;
            let mut buf = [0u8; 1024];
            loop {
                let n = match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut kept = lock(&sink);
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > STDERR_TAIL_BYTES {
                    let cut = kept.len() - STDERR_TAIL_BYTES;
                    kept.drain(..cut);
                }
            }
        })?;

        Ok((stdin, lines, tail, done_rx))
    }

    /// The error that stopped this policy, if any. Text inside it that came
    /// from the policy (its stderr, its reply) is untrusted.
    #[must_use]
    pub fn last_error(&self) -> Option<&PolicyError> {
        self.last_error.as_ref()
    }

    /// Decisions asked so far (the `round` of the last request).
    #[cfg(test)]
    #[must_use]
    pub fn rounds(&self) -> u64 {
        self.round
    }

    /// The OS process id while the policy process is alive; `None` once it
    /// has been killed and reaped.
    #[cfg(test)]
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.running.as_ref().map(|r| r.child.id())
    }

    /// Kill and reap the process, attach what it wrote on stderr to `error`,
    /// and record it. A later failure does not overwrite the first recorded.
    fn fail(&mut self, error: PolicyError) {
        let tail = self.kill().unwrap_or_default();
        if self.last_error.is_none() {
            self.last_error = Some(error.with_stderr(tail));
        }
    }

    /// Kill and reap the process if it is still held; returns its stderr tail.
    fn kill(&mut self) -> Option<String> {
        let mut running = self.running.take()?;
        drop(running.stdin); // EOF first, so a well-behaved policy exits on its own
        let _ = running.child.kill();
        let _ = running.child.wait();
        // The pipe closed with the process; give the reader a bounded moment to drain it.
        let _ = running.stderr_done.recv_timeout(STDERR_SETTLE);
        Some(String::from_utf8_lossy(&lock(&running.stderr)).into_owned())
    }

    /// Wait until the policy's next line has reached this driver, so that a
    /// test knows a line written while no request was pending is there to be
    /// found by the next decision.
    #[cfg(test)]
    fn await_line(&mut self, timeout: Duration) {
        if let Some(running) = self.running.as_mut() {
            running.early = running.lines.recv_timeout(timeout).ok();
        }
    }

    fn exit_status(&mut self) -> Option<String> {
        self.running.as_mut().and_then(|r| r.child.try_wait().ok().flatten()).map(|s| s.to_string())
    }

    /// One round of the protocol: write the request, read and check the reply.
    /// Errors come back without a stderr tail; [`ProcessPolicy::fail`] adds it.
    fn exchange(&mut self, line: &str) -> Result<Vec<NodeId>, PolicyError> {
        let round = self.round;
        let stderr = String::new();
        let malformed =
            |why: String, reply: String| PolicyError::Malformed { round, why, reply, stderr: String::new() };
        let Some(running) = self.running.as_mut() else {
            return Err(PolicyError::Exited { round, status: None, stderr });
        };
        // A line written while no request was pending: the policy is not
        // following the protocol and later replies could be misattributed.
        let pending = match running.early.take() {
            Some(line) => Ok(line),
            None => running.lines.try_recv(),
        };
        match pending {
            Ok(Ok(stray)) => {
                return Err(malformed(
                    "a line written while no request was pending".to_owned(),
                    quote(&stray),
                ));
            }
            Ok(Err(e)) => return Err(malformed(e.to_string(), String::new())),
            Err(TryRecvError::Disconnected) => {
                let status = self.exit_status();
                return Err(PolicyError::Exited { round, status, stderr });
            }
            Err(TryRecvError::Empty) => {}
        }
        if let Err(e) = running
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| running.stdin.write_all(b"\n"))
            .and_then(|()| running.stdin.flush())
        {
            return Err(PolicyError::Write { round, error: e.to_string(), stderr });
        }
        match running.lines.recv_timeout(self.timeout) {
            Ok(Ok(reply)) => {
                let text = String::from_utf8(reply)
                    .map_err(|e| malformed("not UTF-8".to_owned(), quote(e.as_bytes())))?;
                decode_reply(&text).map_err(|why| malformed(why, quote(text.as_bytes())))
            }
            Ok(Err(e)) => Err(malformed(e.to_string(), String::new())),
            Err(RecvTimeoutError::Timeout) => {
                Err(PolicyError::Timeout { round, timeout: self.timeout, stderr })
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = self.exit_status();
                Err(PolicyError::Exited { round, status, stderr })
            }
        }
    }
}

fn quote(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().take(QUOTED_REPLY_CHARS).collect()
}

impl Policy for ProcessPolicy {
    fn decide_with_parents(&mut self, view: &View, parents: &[Option<NodeId>], w: usize) -> Vec<NodeId> {
        if self.running.is_none() {
            return Vec::new(); // failed earlier; the first error stands
        }
        self.round += 1;
        let line = match encode_request(view, parents, self.round, w) {
            Ok(line) => line,
            Err(e) => {
                self.fail(e);
                return Vec::new();
            }
        };
        match self.exchange(&line) {
            Ok(batch) => batch.into_iter().filter(|id| view.continuable.contains(id)).collect(),
            Err(e) => {
                self.fail(e);
                Vec::new()
            }
        }
    }
}

impl Drop for ProcessPolicy {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    /// `python3` from the test process's own `PATH`. These tests run the
    /// protocol against a real process, so without one they fail.
    fn python3() -> PathBuf {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
            .expect("these tests need python3 on PATH")
    }

    /// A program running `script` under `python` with a cleared environment.
    fn python_program(python: &Path, script: &Path, timeout: Duration) -> PolicyProgram {
        PolicyProgram {
            program: python.to_path_buf(),
            args: vec![script.to_string_lossy().into_owned()],
            cwd: None,
            env: Vec::new(),
            timeout,
        }
    }

    /// Write a fake policy script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn view(revealed: &[(NodeId, Score)], continuable: &[NodeId]) -> View {
        View {
            revealed: revealed.to_vec(),
            continuable: continuable.to_vec(),
            observations: vec![None; revealed.len()],
        }
    }

    // --- pure: shape, encoding, decoding -------------------------------------

    #[test]
    fn depths_follow_parents_in_any_order() {
        let revealed = [(0, 0.0), (1, 0.5), (2, 0.3), (5, 0.9), (7, 0.1)];
        let parents = [None, Some(0), Some(0), Some(1), Some(5)];
        assert_eq!(depths(&revealed, &parents).unwrap(), [0, 1, 1, 2, 3]);
        // The same tree listed children-first still resolves.
        let revealed = [(7, 0.1), (5, 0.9), (1, 0.5), (0, 0.0)];
        let parents = [Some(5), Some(1), Some(0), None];
        assert_eq!(depths(&revealed, &parents).unwrap(), [3, 2, 1, 0]);
    }

    #[test]
    fn an_inconsistent_shape_is_refused() {
        assert!(matches!(depths(&[(0, 0.0)], &[]), Err(PolicyError::BadStructure(_))));
        let unrevealed_parent = depths(&[(0, 0.0), (3, 0.1)], &[None, Some(2)]);
        assert!(matches!(unrevealed_parent, Err(PolicyError::BadStructure(w)) if w.contains("not revealed")));
        let cycle = depths(&[(1, 0.0), (2, 0.1)], &[Some(2), Some(1)]);
        assert!(matches!(cycle, Err(PolicyError::BadStructure(w)) if w.contains("cycle")));
    }

    #[test]
    fn a_request_carries_the_documented_fields_and_a_canonical_continuable_order() {
        // Two nodes tie on score at different depths; a deeper one has the lower id.
        let v = view(&[(0, 0.0), (1, 0.5), (2, 0.5), (3, 0.5)], &[0, 1, 2, 3]);
        let parents = [None, Some(0), Some(1), Some(0)];
        let line = encode_request(&v, &parents, 2, 4).unwrap();
        assert!(!line.contains('\n'), "one line");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["round"], 2);
        assert_eq!(value["w"], 4);
        assert_eq!(
            value["revealed"],
            serde_json::json!([
                {"id": 0, "score": 0.0, "parent": null, "depth": 0},
                {"id": 1, "score": 0.5, "parent": 0, "depth": 1},
                {"id": 2, "score": 0.5, "parent": 1, "depth": 2},
                {"id": 3, "score": 0.5, "parent": 0, "depth": 1},
            ])
        );
        // score desc, then depth asc (1 and 3 before 2), then id asc, root last.
        assert_eq!(value["continuable"], serde_json::json!([1, 3, 2, 0]));
    }

    #[test]
    fn a_request_carries_the_recorded_observations_when_there_are_any() {
        let mut v = view(&[(0, 0.5), (1, 0.66), (2, 0.05)], &[1, 2, 0]);
        let obs = |status: &str, errors: u64, sorries: u64, tin: u64, tout: u64| {
            Some(crate::policy::Observation {
                status: status.into(),
                errors,
                sorries,
                tokens_in: tin,
                tokens_out: tout,
                ..crate::policy::Observation::default()
            })
        };
        v.observations = vec![obs("sorries", 0, 1, 0, 0), obs("sorries", 0, 2, 1200, 300), None];
        let line = encode_request(&v, &[None, Some(0), Some(0)], 1, 2).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value["revealed"][0]["check"],
            serde_json::json!({"status": "sorries", "errors": 0, "sorries": 1})
        );
        assert_eq!(value["revealed"][1]["tokens"], serde_json::json!({"in": 1200, "out": 300}));
        assert!(value["revealed"][2].get("check").is_none(), "a record without an observation carries none");
        assert_eq!(value["v"], 1, "additive: the protocol version does not change");
    }

    #[test]
    fn replies_decode_or_say_why_not() {
        assert_eq!(decode_reply(r#"{"batch":[0,0,3]}"#).unwrap(), vec![0, 0, 3]);
        assert_eq!(decode_reply(r#"{"batch":[]}"#).unwrap(), Vec::<NodeId>::new());
        assert_eq!(decode_reply(r#"{"batch":[2],"note":"ignored"}"#).unwrap(), vec![2]);
        assert!(decode_reply("nope").unwrap_err().starts_with("not JSON"));
        assert_eq!(decode_reply("[1]").unwrap_err(), "not a JSON object");
        assert_eq!(decode_reply("{}").unwrap_err(), "no \"batch\" key");
        assert_eq!(decode_reply(r#"{"batch":1}"#).unwrap_err(), "\"batch\" is not an array");
        assert!(decode_reply(r#"{"batch":[-1]}"#).unwrap_err().contains("not a node id"));
        assert!(decode_reply(r#"{"batch":[1.5]}"#).unwrap_err().contains("not a node id"));
    }

    // --- with a real child process ------------------------------------------

    const ECHO_REVERSED: &str = r#"
import json, sys
for line in iter(sys.stdin.readline, ""):
    req = json.loads(line)
    assert req["v"] == 1 and isinstance(req["round"], int) and isinstance(req["w"], int)
    for node in req["revealed"]:
        assert set(node) == {"id", "score", "parent", "depth"}, node
    batch = list(reversed(req["continuable"])) + [999999]
    sys.stdout.write(json.dumps({"batch": batch, "note": "extra keys are fine"}) + "\n")
    sys.stdout.flush()
"#;

    #[test]
    fn a_reply_is_filtered_to_continuable_ids_and_rounds_count_up() {
        let python = python3();
        let dir = tempfile::tempdir().unwrap();
        let program =
            python_program(&python, &script(dir.path(), "echo.py", ECHO_REVERSED), Duration::from_secs(10));
        let mut policy = ProcessPolicy::spawn(&program).unwrap();
        let v = view(&[(0, 0.0), (1, 0.5), (2, 0.2)], &[1, 2, 0]);
        let parents = [None, Some(0), Some(0)];
        assert_eq!(policy.decide_with_parents(&v, &parents, 3), vec![0, 2, 1]);
        assert_eq!(policy.decide_with_parents(&v, &parents, 3), vec![0, 2, 1]);
        assert_eq!(policy.rounds(), 2);
        assert_eq!(policy.last_error(), None);
        assert!(policy.id().is_some(), "still running");
    }

    fn alive(pid: u32) -> bool {
        // After kill + wait the process is reaped, so /proc/<pid> is gone.
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn a_late_reply_is_a_stop_and_the_policy_is_killed() {
        let python = python3();
        let dir = tempfile::tempdir().unwrap();
        let sleepy =
            script(dir.path(), "sleepy.py", "import sys, time\nsys.stdin.readline()\ntime.sleep(60)\n");
        let timeout = Duration::from_millis(300);
        let mut policy = ProcessPolicy::spawn(&python_program(&python, &sleepy, timeout)).unwrap();
        let pid = policy.id().unwrap();
        let v = view(&[(0, 0.0)], &[0]);
        let started = Instant::now();
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        assert!(started.elapsed() < Duration::from_secs(10), "did not wait for the sleep");
        assert!(
            matches!(policy.last_error(), Some(PolicyError::Timeout { round: 1, .. })),
            "{:?}",
            policy.last_error()
        );
        assert_eq!(policy.id(), None);
        assert!(!alive(pid), "policy {pid} was killed and reaped");
        // Later decisions are immediate empty batches; the first error stands.
        let again = Instant::now();
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        assert!(again.elapsed() < timeout);
        assert!(matches!(policy.last_error(), Some(PolicyError::Timeout { .. })));
    }

    #[test]
    fn a_malformed_reply_is_a_stop_with_the_stderr_tail_attached() {
        let python = python3();
        let dir = tempfile::tempdir().unwrap();
        let body = "import sys\nsys.stdin.readline()\nsys.stderr.write('pi: oops\\n')\nsys.stderr.flush()\nprint('not json')\nsys.stdout.flush()\nsys.stdin.readline()\n";
        let garbage = script(dir.path(), "garbage.py", body);
        let mut policy =
            ProcessPolicy::spawn(&python_program(&python, &garbage, Duration::from_secs(10))).unwrap();
        let v = view(&[(0, 0.0)], &[0]);
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        match policy.last_error() {
            Some(PolicyError::Malformed { round: 1, why, reply, stderr }) => {
                assert!(why.starts_with("not JSON"), "{why}");
                assert_eq!(reply, "not json");
                assert!(stderr.contains("pi: oops"), "the stderr tail is attached: {stderr:?}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        assert_eq!(policy.id(), None);
    }

    #[test]
    fn a_policy_that_exits_or_chatters_is_a_stop() {
        let python = python3();
        let dir = tempfile::tempdir().unwrap();
        let v = view(&[(0, 0.0)], &[0]);

        let quitter = script(dir.path(), "quitter.py", "import sys\nsys.stdin.readline()\nsys.exit(3)\n");
        let mut policy =
            ProcessPolicy::spawn(&python_program(&python, &quitter, Duration::from_secs(10))).unwrap();
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        assert!(
            matches!(policy.last_error(), Some(PolicyError::Exited { round: 1, .. })),
            "{:?}",
            policy.last_error()
        );

        let chatty = script(
            dir.path(),
            "chatty.py",
            "import sys\nsys.stdin.readline()\nprint('{\"batch\":[0]}')\nprint('{\"batch\":[0]}')\nsys.stdout.flush()\nsys.stdin.readline()\n",
        );
        let mut policy =
            ProcessPolicy::spawn(&python_program(&python, &chatty, Duration::from_secs(10))).unwrap();
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), vec![0], "the first line is a fine reply");
        policy.await_line(Duration::from_secs(10));
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        assert!(
            matches!(policy.last_error(), Some(PolicyError::Malformed { round: 2, why, .. }) if why.contains("no request was pending")),
            "{:?}",
            policy.last_error()
        );

        let huge = script(
            dir.path(),
            "huge.py",
            "import sys\nsys.stdin.readline()\nsys.stdout.write('{\"batch\":[' + '0,' * 3000 + '0]}\\n')\nsys.stdout.flush()\nsys.stdin.readline()\n",
        );
        let mut policy =
            ProcessPolicy::spawn(&python_program(&python, &huge, Duration::from_secs(30))).unwrap();
        assert_eq!(policy.decide_with_parents(&v, &[None], 2), Vec::<NodeId>::new());
        assert!(
            matches!(policy.last_error(), Some(PolicyError::Malformed { why, .. }) if why.contains("longer than")),
            "{:?}",
            policy.last_error()
        );
    }

    #[test]
    fn dropping_the_policy_kills_the_process() {
        let python = python3();
        let dir = tempfile::tempdir().unwrap();
        let sleepy = script(dir.path(), "sleepy.py", "import time\ntime.sleep(60)\n");
        let policy = ProcessPolicy::spawn(&python_program(&python, &sleepy, Duration::from_secs(1))).unwrap();
        let pid = policy.id().unwrap();
        drop(policy);
        assert!(!alive(pid), "policy {pid} outlived its owner");
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_not_a_stop() {
        let program = PolicyProgram {
            program: PathBuf::from("/nonexistent/interpreter"),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout: Duration::from_secs(1),
        };
        assert!(ProcessPolicy::spawn(&program).is_err());
    }
}
