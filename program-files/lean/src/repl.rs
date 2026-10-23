//! Client for the Lean REPL (`leanprover-community/repl`) as a resident child
//! process: the warm backend of `check`, `analyze` and `complete`.
//! One [`Repl`] owns one REPL process. [`crate::checker::ReplChecker`] owns
//! it: it imports Mathlib once per process, retries a request once on a
//! fresh process when the old one died or timed out (never an `analyze` or a
//! `complete`), and replaces the process after `--repl-max-requests`
//! operations. Confining the process is the launcher's
//! job — this module runs the given executable with exactly the given
//! environment and nothing ambient.
//!
//! Wire protocol:
//!
//! - stdin takes one JSON object per request followed by a blank line;
//! - stdout answers one JSON object, pretty-printed over several lines,
//!   terminated by a blank line;
//! - command mode `{"cmd": src, "env"?: id}` answers `{"env": id, "messages"?:
//!   [..], "sorries"?: [..]}` (the two lists are absent when empty); with
//!   `"allTactics": true` it adds `"tactics": [..]`, one entry per tactic run
//!   with its span, the proof state *before* it and that state's goals as one
//!   string;
//! - tactic mode `{"tactic": t, "proofState": id}` answers `{"proofState": id,
//!   "goals": [..], "proofStatus"?: s, "messages"?: [..]}`, or `{"message":
//!   "Lean error: .."}` when the tactic fails.
//!
//! Every request is bounded by [`ReplConfig::request_timeout`] unless the
//! caller passes its own deadline (`*_within`). I/O with the
//! child happens on helper threads (a writer, a stdout reader, a stderr drain),
//! so a REPL that stops answering or floods stderr can never block the caller:
//! the caller waits on a channel with a deadline, and on expiry kills the child.
//! After a timeout, an I/O failure or an exit the [`Repl`] is dead
//! ([`Repl::is_alive`] is `false`, every request fails with
//! [`ReplError::Closed`]) and the owner must spawn a new one; a fresh process
//! is the only way to get back to a known environment/proof-state table.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// Largest single reply accepted from the REPL. A goal listing for a big proof
/// is kilobytes; anything near this bound means the stream is not what we think
/// it is, and the process is killed rather than buffered without limit.
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// How much of the REPL's stderr is retained for diagnostics (the tail).
const STDERR_TAIL_BYTES: usize = 64 * 1024;
/// Longest excerpt of a bad reply quoted in a [`ReplError::Protocol`].
const EXCERPT_CHARS: usize = 400;

/// How to run the REPL. Everything the child sees is listed here; the parent's
/// environment is never inherited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplConfig {
    /// The `repl` executable (e.g. `<checkout>/.lake/build/bin/repl`).
    pub exe: PathBuf,
    /// The child's `PATH`. The REPL forks `lean` through it, so the toolchain
    /// `bin` directory must come first.
    pub path_env: String,
    /// The child's `LEAN_PATH`: the olean search path.
    pub lean_path: String,
    /// The child's `HOME`; must be writable.
    pub home: PathBuf,
    /// The child's working directory.
    pub cwd: PathBuf,
    /// Deadline for one request, from sending it to receiving the whole reply.
    pub request_timeout: Duration,
}

/// Why a REPL operation failed.
#[derive(Debug)]
pub enum ReplError {
    /// The child process or its helper threads could not be started.
    Spawn(std::io::Error),
    /// Reading from or writing to the child failed. The REPL is dead afterwards.
    Io(std::io::Error),
    /// No complete reply arrived within `request_timeout`. The child has been
    /// killed and the REPL is dead afterwards.
    Timeout,
    /// The reply was not what the protocol promises: unparsable JSON, a
    /// command reply without `env`, a REPL-level rejection such as an unknown
    /// `env` id, or an oversized reply (that last one kills the REPL). The
    /// stream stays in sync for the other cases, so the REPL remains usable.
    Protocol(String),
    /// The child has exited or was killed earlier; the owner must respawn.
    Closed,
}

impl std::fmt::Display for ReplError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplError::Spawn(e) => write!(f, "cannot start the Lean REPL: {e}"),
            ReplError::Io(e) => write!(f, "Lean REPL I/O failed: {e}"),
            ReplError::Timeout => write!(f, "the Lean REPL did not answer within the deadline"),
            ReplError::Protocol(why) => write!(f, "Lean REPL protocol violation: {why}"),
            ReplError::Closed => write!(f, "the Lean REPL is not running"),
        }
    }
}

/// One diagnostic from command mode. `line` is 1-based and `column` 0-based,
/// as Lean reports them; both are 0 when the REPL gave no position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// `"error"`, `"warning"` or `"info"`.
    pub severity: String,
    pub data: String,
    pub line: u64,
    pub column: u64,
}

/// One `sorry` in a command, with the proof state to continue from in tactic
/// mode and the pretty-printed goal (`hyps ⊢ target`). `end_line`/`end_column`
/// close the token's span (0 when the REPL gave none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sorry {
    pub proof_state: u64,
    pub goal: String,
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

/// One tactic of a command elaborated with `allTactics`: its source text and
/// span, the proof state the tactic ran *on* (before it) and that state's
/// goals as one pretty-printed string (several goals separated by blank
/// lines). `proof_state` is absent when the REPL recorded none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TacticInfo {
    pub tactic: String,
    pub proof_state: Option<u64>,
    pub goals: String,
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

/// The reply to a command-mode request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandReply {
    /// The environment after the command; pass it as `env` to build on it.
    pub env: u64,
    pub messages: Vec<Message>,
    pub sorries: Vec<Sorry>,
    /// Filled only for a command sent with `all_tactics` (see
    /// [`Repl::command_with`]).
    pub tactics: Vec<TacticInfo>,
}

impl CommandReply {
    /// Whether any message has severity `error`.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.messages.iter().any(|m| m.severity == "error")
    }
}

/// The reply to a tactic-mode request. Exactly one of `error` and
/// `proof_state` is set: a failed tactic yields `error` (the REPL's
/// `{"message": "Lean error: .."}`) and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TacticReply {
    pub proof_state: Option<u64>,
    /// Remaining goals; empty when the proof is complete.
    pub goals: Vec<String>,
    /// E.g. `"Completed"` or `"Incomplete: open goals remain"`.
    pub proof_status: Option<String>,
    /// Messages the tactic logged (an `exact?` suggestion is an `info`).
    pub messages: Vec<Message>,
    pub error: Option<String>,
}

// ---- Wire shapes (private): what the REPL actually prints ----

#[derive(Deserialize)]
struct WirePos {
    line: u64,
    column: u64,
}

#[derive(Deserialize)]
struct WireMessage {
    severity: String,
    data: String,
    pos: Option<WirePos>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSorry {
    proof_state: u64,
    goal: String,
    pos: Option<WirePos>,
    end_pos: Option<WirePos>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTacticInfo {
    tactic: String,
    proof_state: Option<u64>,
    #[serde(default)]
    goals: String,
    pos: Option<WirePos>,
    end_pos: Option<WirePos>,
}

#[derive(Deserialize)]
struct WireCommand {
    env: Option<u64>,
    #[serde(default)]
    messages: Vec<WireMessage>,
    #[serde(default)]
    sorries: Vec<WireSorry>,
    #[serde(default)]
    tactics: Vec<WireTacticInfo>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTactic {
    proof_state: Option<u64>,
    #[serde(default)]
    goals: Vec<String>,
    proof_status: Option<String>,
    #[serde(default)]
    messages: Vec<WireMessage>,
    message: Option<String>,
}

fn position(pos: Option<WirePos>) -> (u64, u64) {
    pos.map_or((0, 0), |p| (p.line, p.column))
}

fn message(m: WireMessage) -> Message {
    let (line, column) = position(m.pos);
    Message { severity: m.severity, data: m.data, line, column }
}

fn excerpt(s: &str) -> String {
    s.chars().take(EXCERPT_CHARS).collect()
}

/// What the helper threads hand to the caller: one item per request while the
/// child behaves, then a terminal item.
enum Inbound {
    /// One complete reply block (the text between blank lines).
    Reply(String),
    /// A read or write on the child's pipes failed.
    Io(std::io::Error),
    /// A reply grew past [`MAX_REPLY_BYTES`]; the reader has given up.
    Oversized,
    /// The child closed its stdout.
    Eof,
}

/// The caller's ends of the helper threads: the request sender, the inbound
/// receiver and the shared stderr tail.
type Wiring = (Sender<Vec<u8>>, Receiver<Inbound>, Arc<Mutex<Vec<u8>>>);

/// A resident Lean REPL process.
pub struct Repl {
    child: Child,
    /// Requests go to the writer thread, which owns the child's stdin. The
    /// thread ends (closing stdin) when this sender is dropped.
    requests: Sender<Vec<u8>>,
    inbound: Receiver<Inbound>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    timeout: Duration,
    dead: bool,
}

impl std::fmt::Debug for Repl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repl")
            .field("pid", &self.child.id())
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

impl Repl {
    /// Start the REPL. Returns as soon as the process and its helper threads
    /// are up; the first request is what proves the binary works.
    pub fn spawn(config: &ReplConfig) -> Result<Self, ReplError> {
        let mut child = Command::new(&config.exe)
            .env_clear()
            .env("PATH", &config.path_env)
            .env("LEAN_PATH", &config.lean_path)
            .env("HOME", &config.home)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ReplError::Spawn)?;
        match Self::start_threads(&mut child) {
            Ok((requests, inbound, stderr_tail)) => Ok(Repl {
                child,
                requests,
                inbound,
                stderr_tail,
                timeout: config.request_timeout,
                dead: false,
            }),
            Err(e) => {
                // Half-started: the process exists but cannot be driven. Reap it.
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Take the child's pipes and start the three helper threads.
    fn start_threads(child: &mut Child) -> Result<Wiring, ReplError> {
        let missing =
            |what: &str| ReplError::Spawn(std::io::Error::other(format!("child has no {what} pipe")));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;

        let (inbound_tx, inbound) = std::sync::mpsc::channel();
        let (requests, request_rx) = std::sync::mpsc::channel();
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));

        let tx = inbound_tx.clone();
        std::thread::Builder::new()
            .name("lean-repl-stdout".to_owned())
            .spawn(move || read_replies(stdout, &tx))
            .map_err(ReplError::Spawn)?;
        std::thread::Builder::new()
            .name("lean-repl-stdin".to_owned())
            .spawn(move || write_requests(stdin, &request_rx, &inbound_tx))
            .map_err(ReplError::Spawn)?;
        let tail = Arc::clone(&stderr_tail);
        std::thread::Builder::new()
            .name("lean-repl-stderr".to_owned())
            .spawn(move || drain_stderr(stderr, &tail))
            .map_err(ReplError::Spawn)?;
        Ok((requests, inbound, stderr_tail))
    }

    /// The child's OS process id (valid while [`Self::is_alive`]).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the process is still running. A REPL that timed out, failed
    /// I/O or exited stays dead; the owner spawns a new one.
    pub fn is_alive(&mut self) -> bool {
        if self.dead {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                self.dead = true;
                false
            }
        }
    }

    /// The last [`STDERR_TAIL_BYTES`] of the child's stderr so far, for the
    /// owner's diagnostics when a request fails. Never contains a reply.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().map(|t| String::from_utf8_lossy(&t).into_owned()).unwrap_or_default()
    }

    /// Command mode: elaborate `source` in environment `env` (a previous
    /// reply's `env`), or in a fresh one when `None` (required for `import`).
    pub fn command(&mut self, source: &str, env: Option<u64>) -> Result<CommandReply, ReplError> {
        let timeout = self.timeout;
        self.command_with(source, env, false, timeout)
    }

    /// [`Self::command`] with the REPL's `allTactics` option (every tactic run
    /// comes back in [`CommandReply::tactics`]) and an explicit deadline for
    /// this one request (in place of the configured one; on expiry the
    /// process is killed, as for any timeout).
    pub fn command_with(
        &mut self,
        source: &str,
        env: Option<u64>,
        all_tactics: bool,
        timeout: Duration,
    ) -> Result<CommandReply, ReplError> {
        let mut request = serde_json::Map::new();
        request.insert("cmd".to_owned(), Value::String(source.to_owned()));
        if let Some(env) = env {
            request.insert("env".to_owned(), Value::from(env));
        }
        if all_tactics {
            request.insert("allTactics".to_owned(), Value::Bool(true));
        }
        let reply = self.request_within(&Value::Object(request), timeout)?;
        let wire: WireCommand = parse(&reply)?;
        let env = match (wire.env, wire.message) {
            (Some(env), _) => env,
            (None, Some(message)) => return Err(ReplError::Protocol(format!("command rejected: {message}"))),
            (None, None) => {
                return Err(ReplError::Protocol(format!(
                    "command reply has no env: {}",
                    excerpt(&reply.to_string())
                )));
            }
        };
        let messages = wire.messages.into_iter().map(message).collect();
        let sorries = wire
            .sorries
            .into_iter()
            .map(|s| {
                let (line, column) = position(s.pos);
                let (end_line, end_column) = position(s.end_pos);
                Sorry { proof_state: s.proof_state, goal: s.goal, line, column, end_line, end_column }
            })
            .collect();
        let tactics = wire
            .tactics
            .into_iter()
            .map(|t| {
                let (line, column) = position(t.pos);
                let (end_line, end_column) = position(t.end_pos);
                TacticInfo {
                    tactic: t.tactic,
                    proof_state: t.proof_state,
                    goals: t.goals,
                    line,
                    column,
                    end_line,
                    end_column,
                }
            })
            .collect();
        Ok(CommandReply { env, messages, sorries, tactics })
    }

    /// Tactic mode with the configured deadline, for the tests.
    #[cfg(test)]
    pub fn tactic(&mut self, tactic: &str, proof_state: u64) -> Result<TacticReply, ReplError> {
        let timeout = self.timeout;
        self.tactic_within(tactic, proof_state, timeout)
    }

    /// Tactic mode: run `tactic` on `proof_state` (from a [`Sorry`] or a
    /// previous [`TacticReply`]) within `timeout`. A failing tactic is a
    /// normal reply with `error` set, not a [`ReplError`].
    pub fn tactic_within(
        &mut self,
        tactic: &str,
        proof_state: u64,
        timeout: Duration,
    ) -> Result<TacticReply, ReplError> {
        let reply = self
            .request_within(&serde_json::json!({ "tactic": tactic, "proofState": proof_state }), timeout)?;
        let wire: WireTactic = parse(&reply)?;
        if let Some(error) = wire.message {
            return Ok(TacticReply {
                proof_state: None,
                goals: vec![],
                proof_status: None,
                messages: vec![],
                error: Some(error),
            });
        }
        if wire.proof_state.is_none() {
            return Err(ReplError::Protocol(format!(
                "tactic reply has neither proofState nor message: {}",
                excerpt(&reply.to_string())
            )));
        }
        Ok(TacticReply {
            proof_state: wire.proof_state,
            goals: wire.goals,
            proof_status: wire.proof_status,
            messages: wire.messages.into_iter().map(message).collect(),
            error: None,
        })
    }

    /// Send one raw request object and return the raw reply object, within
    /// `timeout`. This is what [`Self::command`] and [`Self::tactic_within`]
    /// are built on. One request is in flight at a time by construction.
    pub fn request_within(&mut self, json: &Value, timeout: Duration) -> Result<Value, ReplError> {
        if self.dead {
            return Err(ReplError::Closed);
        }
        let mut bytes = serde_json::to_vec(json)
            .map_err(|e| ReplError::Protocol(format!("cannot encode request: {e}")))?;
        bytes.extend_from_slice(b"\n\n");
        if self.requests.send(bytes).is_err() {
            // The writer thread is gone: stdin broke on an earlier request.
            self.kill();
            return Err(ReplError::Closed);
        }
        match self.inbound.recv_timeout(timeout) {
            Ok(Inbound::Reply(text)) => serde_json::from_str(&text)
                .map_err(|e| ReplError::Protocol(format!("unparsable reply ({e}): {}", excerpt(&text)))),
            Ok(Inbound::Io(e)) => {
                self.kill();
                Err(ReplError::Io(e))
            }
            Ok(Inbound::Oversized) => {
                self.kill();
                Err(ReplError::Protocol(format!("reply exceeds {MAX_REPLY_BYTES} bytes")))
            }
            Ok(Inbound::Eof) | Err(RecvTimeoutError::Disconnected) => {
                self.kill();
                Err(ReplError::Closed)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill();
                Err(ReplError::Timeout)
            }
        }
    }

    /// Kill and reap the child and mark this REPL dead. Idempotent. The helper
    /// threads end on their own once the pipes close; they are never joined,
    /// so a grandchild still holding a pipe cannot block the caller.
    fn kill(&mut self) {
        self.dead = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        self.kill();
    }
}

fn parse<T: for<'de> Deserialize<'de>>(reply: &Value) -> Result<T, ReplError> {
    T::deserialize(reply).map_err(|e| {
        ReplError::Protocol(format!("unexpected reply shape ({e}): {}", excerpt(&reply.to_string())))
    })
}

/// Reader thread: split stdout into blank-line-terminated blocks. A partial
/// block at EOF is still delivered (its JSON decides), then `Eof`.
fn read_replies(stdout: ChildStdout, tx: &Sender<Inbound>) {
    let mut reader = BufReader::new(stdout);
    let mut block: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = tx.send(Inbound::Io(e));
                return;
            }
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if !block.is_empty() && tx.send(Inbound::Reply(take_text(&mut block))).is_err() {
                return;
            }
            continue;
        }
        if block.len() + line.len() > MAX_REPLY_BYTES {
            let _ = tx.send(Inbound::Oversized);
            return;
        }
        block.extend_from_slice(&line);
    }
    if !block.is_empty() {
        let _ = tx.send(Inbound::Reply(take_text(&mut block)));
    }
    let _ = tx.send(Inbound::Eof);
}

fn take_text(block: &mut Vec<u8>) -> String {
    String::from_utf8_lossy(&std::mem::take(block)).into_owned()
}

/// Writer thread: owns stdin so a write that blocks (a hung REPL and a full
/// pipe) blocks this thread, not the caller. Ends when the `Repl` drops its
/// sender, closing stdin, or when a write fails.
fn write_requests(mut stdin: ChildStdin, requests: &Receiver<Vec<u8>>, tx: &Sender<Inbound>) {
    while let Ok(bytes) = requests.recv() {
        if let Err(e) = stdin.write_all(&bytes).and_then(|()| stdin.flush()) {
            let _ = tx.send(Inbound::Io(e));
            return;
        }
    }
}

/// Stderr thread: keep the pipe drained (so the child never blocks on it) and
/// retain the tail for diagnostics.
fn drain_stderr(mut stderr: ChildStderr, tail: &Mutex<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let cut = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..cut);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for the REPL that speaks its framing: one request per
    /// blank-line-terminated block, one pretty-printed reply per block. Which
    /// canned reply it gives depends on a keyword in the request.
    const FAKE_REPL: &str = r#"#!/bin/sh
env=0
req=""
while IFS= read -r line; do
  case "$line" in
    "") [ -n "$req" ] || continue ;;
    *) req="$req$line"; continue ;;
  esac
  case "$req" in
    *hang*) : ;;
    *quit*) exit 0 ;;
    *noisy*) head -c 1000000 /dev/zero | tr '\0' 'e' >&2; printf '{"env": 99}\n\n' ;;
    *garbage*) printf 'this is not json\n\n' ;;
    *bogus*) printf '%s\n\n' '{"message": "Lean error:\n<input>:1:1: unknown tactic"}' ;;
    *'"tactic"'*) cat <<'EOF'
{"proofState": 7,
 "goals": ["n : Nat\nh : 2 ≤ n\n⊢ 0 < n"],
 "proofStatus": "Incomplete: open goals remain"}

EOF
    ;;
    *allTactics*) cat <<'EOF'
{"tactics":
 [{"usedConstants": ["Nat", "Eq", "rfl"],
   "tactic": "have h : n = n := rfl",
   "proofState": 3,
   "pos": {"line": 2, "column": 2},
   "goals": "n : ℕ\n⊢ n + 0 = n",
   "endPos": {"line": 2, "column": 23}},
  {"tactic": "sorry",
   "proofState": 4,
   "pos": {"line": 3, "column": 2},
   "goals": "n : ℕ\nh : n = n\n⊢ n + 0 = n",
   "endPos": {"line": 3, "column": 7}}],
 "sorries":
 [{"proofState": 4,
   "pos": {"line": 3, "column": 2},
   "goal": "n : ℕ\nh : n = n\n⊢ n + 0 = n",
   "endPos": {"line": 3, "column": 7}}],
 "messages":
 [{"severity": "warning",
   "pos": {"line": 1, "column": 8},
   "endPos": {"line": 1, "column": 9},
   "data": "declaration uses 'sorry'"}],
 "env": 8}

EOF
    ;;
    *sorry*) cat <<'EOF'
{"sorries":
 [{"proofState": 0,
   "pos": {"line": 1, "column": 34},
   "goal": "n : Nat\n⊢ n + 0 = n",
   "endPos": {"line": 1, "column": 39}}],
 "messages":
 [{"severity": "warning",
   "pos": {"line": 1, "column": 4},
   "endPos": {"line": 1, "column": 5},
   "data": "declaration uses 'sorry'"}],
 "env": 0}

EOF
    ;;
    *'"env":3'*) printf '{"env": 4}\n\n' ;;
    *) env=$((env+1)); printf '{"env": %d}\n\n' "$env" ;;
  esac
  req=""
done
"#;

    /// The fake REPL script, written exactly once per test process. Tests run
    /// in parallel threads, and a `fork` that races another thread's open
    /// write of the script inherits that descriptor until its `exec`, which
    /// then fails with `ETXTBSY`; writing once, before any spawn, avoids it.
    fn fake_exe() -> PathBuf {
        static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        EXE.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("nucleus_repl_fake_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("repl.sh");
            std::fs::write(&exe, FAKE_REPL).unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            exe
        })
        .clone()
    }

    /// Spawns the fake REPL, retrying `ETXTBSY`.
    ///
    /// This test binary writes an executable and spawns processes at the same
    /// time, in parallel threads. Between another thread's `fork` and its
    /// `exec`, that child holds every fd the parent had open -- including the
    /// one writing `repl.sh` -- and Linux refuses to exec a file that anyone
    /// holds open for writing. It is a race the test fixture makes, not one in
    /// the REPL.
    fn spawn_fake(config: &ReplConfig) -> Repl {
        for _ in 0..100 {
            match Repl::spawn(config) {
                Err(ReplError::Spawn(e)) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other.expect("the fake REPL starts"),
            }
        }
        panic!("the fake REPL script stayed busy for two seconds")
    }

    /// A config for the fake REPL with a private scratch dir; returns the config and the dir.
    fn fake(name: &str, timeout: Duration) -> (ReplConfig, PathBuf) {
        let exe = fake_exe();
        let dir = std::env::temp_dir().join(format!("nucleus_repl_fake_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = ReplConfig {
            exe,
            path_env: "/usr/bin:/bin".to_owned(),
            lean_path: String::new(),
            home: dir.clone(),
            cwd: dir.clone(),
            request_timeout: timeout,
        };
        (config, dir)
    }

    #[test]
    fn framing_and_typed_parsing_over_a_fake_repl() {
        let (config, dir) = fake("framing", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        assert!(repl.is_alive());

        // Single-line replies, env counter, no messages/sorries keys at all.
        let first = repl.command("def f := 1", None).unwrap();
        assert_eq!(first, CommandReply { env: 1, messages: vec![], sorries: vec![], tactics: vec![] });
        // The env we pass is on the wire: the fake answers 4 only to "env":3.
        assert_eq!(repl.command("def g := 2", Some(3)).unwrap().env, 4);

        // A multi-line reply with unicode and escaped newlines in strings.
        let with_sorry = repl.command("theorem t (n : Nat) : n + 0 = n := by sorry", Some(0)).unwrap();
        assert_eq!(with_sorry.env, 0);
        assert_eq!(
            with_sorry.sorries,
            vec![Sorry {
                proof_state: 0,
                goal: "n : Nat\n⊢ n + 0 = n".into(),
                line: 1,
                column: 34,
                end_line: 1,
                end_column: 39
            }]
        );
        assert_eq!(
            with_sorry.messages,
            vec![Message {
                severity: "warning".into(),
                data: "declaration uses 'sorry'".into(),
                line: 1,
                column: 4
            }]
        );
        assert!(!with_sorry.has_errors());

        // Tactic mode: goals, a new proof state, the status.
        let step = repl.tactic("intro h", 0).unwrap();
        assert_eq!(
            step,
            TacticReply {
                proof_state: Some(7),
                goals: vec!["n : Nat\nh : 2 ≤ n\n⊢ 0 < n".into()],
                proof_status: Some("Incomplete: open goals remain".into()),
                messages: vec![],
                error: None,
            }
        );
        // A failing tactic is `{"message": ..}`, mapped to `error`.
        let bad = repl.tactic("bogus_tac", 0).unwrap();
        assert_eq!(bad.proof_state, None);
        assert!(bad.goals.is_empty());
        assert!(bad.error.as_deref().unwrap().contains("unknown tactic"), "{bad:?}");

        assert!(repl.is_alive());
        drop(repl);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_tactics_and_end_positions_are_parsed() {
        let (config, dir) = fake("all_tactics", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        // The option is on the wire (the fake answers this shape only to it),
        // the deadline is the caller's, and the reply's tactics carry their
        // span, the state they ran on and that state's goals as one string.
        let reply = repl
            .command_with(
                "theorem t (n : ℕ) : n + 0 = n := by\n  have h : n = n := rfl\n  sorry",
                Some(0),
                true,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(reply.env, 8);
        assert_eq!(
            reply.tactics[0],
            TacticInfo {
                tactic: "have h : n = n := rfl".into(),
                proof_state: Some(3),
                goals: "n : ℕ\n⊢ n + 0 = n".into(),
                line: 2,
                column: 2,
                end_line: 2,
                end_column: 23,
            }
        );
        assert_eq!(reply.tactics.len(), 2);
        assert_eq!((reply.sorries[0].end_line, reply.sorries[0].end_column), (3, 7));
        // Without the option the same request has no tactics.
        assert!(repl.command("theorem t2 : True := trivial", Some(0)).unwrap().tactics.is_empty());
        drop(repl);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hung_repl_is_killed_on_timeout_and_stays_dead() {
        // The configured timeout is generous and the short deadline goes only
        // to the one call under test: on a loaded machine a freshly spawned
        // fake can take longer than that deadline to answer the setup call.
        let (config, dir) = fake("hang", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        assert_eq!(repl.command("def ok := 1", None).unwrap().env, 1);
        let pid = repl.pid();
        let deadline = Duration::from_millis(300);
        let started = std::time::Instant::now();
        assert!(matches!(repl.command_with("hang", None, false, deadline), Err(ReplError::Timeout)));
        let waited = started.elapsed();
        assert!(waited >= deadline, "it waited for the deadline it was given, not less");
        // An upper bound relative to the deadline rather than an absolute one:
        // what is being checked is that the wait is bounded at all, and any
        // wall-clock number tight enough to be interesting is also tight
        // enough to fail on a loaded machine.
        assert!(waited < 100 * deadline, "and the deadline bounds it: waited {waited:?}");
        assert!(!repl.is_alive());
        assert!(!crate::test_process_exists(pid), "the child was killed and reaped");
        // Dead means dead: no request goes through, and nothing is respawned here.
        assert!(matches!(repl.command("def later := 2", None), Err(ReplError::Closed)));
        assert!(matches!(repl.tactic("intro h", 0), Err(ReplError::Closed)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_exiting_repl_reports_closed() {
        let (config, dir) = fake("quit", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        assert!(matches!(repl.command("quit", None), Err(ReplError::Closed)));
        assert!(!repl.is_alive());
        assert!(matches!(repl.command("def x := 1", None), Err(ReplError::Closed)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stderr_is_drained_without_blocking_and_its_tail_is_kept() {
        let (config, dir) = fake("noisy", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        // The fake writes 1 MB to stderr (far more than a pipe buffer holds)
        // before answering; an undrained pipe would stall it until the deadline.
        assert_eq!(repl.command("noisy", None).unwrap().env, 99);
        assert!(repl.is_alive());
        // The drain thread may still be copying the last bytes; give it a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while repl.stderr_tail().len() < STDERR_TAIL_BYTES && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let tail = repl.stderr_tail();
        assert_eq!(tail.len(), STDERR_TAIL_BYTES);
        assert!(tail.bytes().all(|b| b == b'e'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_reply_is_a_protocol_error_and_the_repl_survives() {
        let (config, dir) = fake("garbage", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        let err = repl.command("garbage", None).unwrap_err();
        assert!(matches!(&err, ReplError::Protocol(why) if why.contains("this is not json")), "{err}");
        // The block was consumed whole, so the stream is still in sync.
        assert!(repl.is_alive());
        assert_eq!(repl.command("def next := 1", None).unwrap().env, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_kills_the_child() {
        let (config, dir) = fake("drop", Duration::from_secs(10));
        let mut repl = spawn_fake(&config);
        assert_eq!(repl.command("def x := 1", None).unwrap().env, 1);
        let pid = repl.pid();
        assert!(crate::test_process_exists(pid));
        drop(repl);
        assert!(!crate::test_process_exists(pid));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_executable_is_a_spawn_error() {
        let (mut config, dir) = fake("missing", Duration::from_secs(1));
        config.exe = dir.join("no_such_repl");
        assert!(matches!(Repl::spawn(&config), Err(ReplError::Spawn(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `lake env <cmd>` in the PutnamBench directory, trimmed stdout.
    fn lake_env(pb_lean: &str, args: &[&str]) -> String {
        let out = Command::new("lake").arg("env").args(args).current_dir(pb_lean).output().unwrap();
        assert!(out.status.success(), "lake env {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    // The real thing. Opt-in: needs the PutnamBench Lean environment and the
    // built REPL binary. Run with
    // `PB_LEAN=~/putnambench/lean4 NUCLEUS_REPL=~/lean-repl/.lake/build/bin/repl \
    //    cargo test -p nucleus-lean -- --ignored repl`
    // (`lake` must be on PATH, i.e. `$HOME/.elan/bin`).
    #[test]
    #[ignore = "needs PB_LEAN and the REPL binary (NUCLEUS_REPL)"]
    fn real_repl_imports_mathlib_and_steps_a_proof() {
        let (pb_lean, exe) = (
            std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"),
            std::env::var("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL"),
        );
        let lean_path = lake_env(&pb_lean, &["printenv", "LEAN_PATH"]);
        let lean = std::fs::canonicalize(lake_env(&pb_lean, &["which", "lean"])).unwrap();
        let toolchain_bin = lean.parent().unwrap().to_str().unwrap().to_owned();
        let home = std::env::temp_dir().join(format!("nucleus_repl_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = ReplConfig {
            exe: PathBuf::from(exe),
            path_env: format!("{toolchain_bin}:/usr/bin:/bin"),
            lean_path,
            home: home.clone(),
            cwd: home.clone(),
            request_timeout: Duration::from_secs(180),
        };
        let mut repl = spawn_fake(&config);

        let t = std::time::Instant::now();
        let imported = repl.command("import Mathlib", None).unwrap();
        eprintln!("import Mathlib: {:?} -> env {}", t.elapsed(), imported.env);
        assert!(!imported.has_errors(), "{imported:?}");
        let base = imported.env;

        let t = std::time::Instant::now();
        let stated = repl.command("theorem t (n : Nat) : 2 ≤ n → 0 < n := by sorry", Some(base)).unwrap();
        eprintln!("sorry theorem: {:?} -> {stated:?}", t.elapsed());
        assert!(!stated.has_errors(), "{stated:?}");
        assert_eq!(stated.sorries.len(), 1, "{stated:?}");
        let sorry = &stated.sorries[0];
        assert!(sorry.goal.contains("⊢ 2 ≤ n → 0 < n"), "{}", sorry.goal);
        assert!(sorry.line >= 1);

        let t = std::time::Instant::now();
        let step = repl.tactic("intro h", sorry.proof_state).unwrap();
        eprintln!("intro h: {:?} -> {step:?}", t.elapsed());
        assert_eq!(step.error, None);
        assert!(step.proof_state.is_some());
        assert_eq!(step.goals.len(), 1);
        assert!(step.goals[0].contains("h : 2 ≤ n") && step.goals[0].contains("⊢ 0 < n"), "{step:?}");

        let t = std::time::Instant::now();
        let bad = repl.tactic("bogus_tac", sorry.proof_state).unwrap();
        eprintln!("bogus tactic: {:?} -> {bad:?}", t.elapsed());
        assert!(bad.error.is_some(), "{bad:?}");
        assert_eq!(bad.proof_state, None);

        let t = std::time::Instant::now();
        let done = repl.tactic("omega", step.proof_state.unwrap()).unwrap();
        eprintln!("omega: {:?} -> {done:?}", t.elapsed());
        assert_eq!(done.error, None);
        assert!(done.goals.is_empty(), "{done:?}");

        // Env reuse: a second declaration on the same base, a full proof, no sorries.
        let t = std::time::Instant::now();
        let proved = repl.command("theorem t2 (n : Nat) : n + 0 = n := by simp", Some(base)).unwrap();
        eprintln!("full proof: {:?} -> {proved:?}", t.elapsed());
        assert!(!proved.has_errors() && proved.sorries.is_empty(), "{proved:?}");
        assert!(proved.env > base);

        assert!(repl.is_alive());
        drop(repl);
        let _ = std::fs::remove_dir_all(&home);
    }
}
