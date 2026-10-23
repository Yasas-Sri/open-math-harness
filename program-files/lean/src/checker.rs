//! `check`, `analyze` and `complete`: the resident Lean REPL
//! and what the service makes of its replies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nucleus_lean_interface::{
    AnalyzeResponse, CheckResponse, CheckStatus, CompleteResponse, Diagnostic, OpenGoal,
};

use crate::repl::{CommandReply, Repl, ReplConfig, ReplError};
use crate::verify::excerpt;
use crate::{check_forbidden_token, completion, sketch};

// ---- `check`: the partial-proof evaluator on a resident REPL ----

/// Most errors and most open goals reported by one `check`, and the longest
/// text kept for each. Lean can print a term of any size; the kernel's reply
/// payload cannot exceed the protocol's frame limit (8 MiB), so the
/// lists are bounded here and a cut text ends in `…[truncated]`. A candidate
/// that exceeds these bounds is pathological, not a proof in progress.
pub const MAX_CHECK_ITEMS: usize = 32;
pub const MAX_MESSAGE_CHARS: usize = 1_500;
pub const MAX_GOAL_CHARS: usize = 3_000;

/// What `check` decided before any Lean ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// Not a valid attempt at this problem (the reason); no Lean runs.
    Invalid(String),
    /// The text to elaborate in the Mathlib environment.
    Send(String),
}

/// The pre-Lean part of `check`, `analyze` and `complete`: a banned token
/// (`native_decide`, user `axiom`, anything that runs code while elaborating,
/// see [`nucleus_lean_interface::exec_token`]; `sorry` is allowed) or a missing verbatim
/// `statement` (the frozen `theorem … :=`, see [`nucleus_lean_interface::extract_statement`])
/// makes the attempt `Invalid`; otherwise the candidate is prepared for the
/// REPL by [`without_header_imports`].
pub fn prepare_check(statement: &str, candidate: &str) -> Prepared {
    if let Some(tok) = check_forbidden_token(candidate) {
        return Prepared::Invalid(format!("forbidden token `{tok}`"));
    }
    if !candidate.contains(statement) {
        // The reason carries the statement itself: a caller that retyped or
        // reflowed it (one space in `((3) : ℕ )` is enough) cannot see what
        // differs from the words alone, and the frozen text is the answer.
        return Prepared::Invalid(format!(
            "the frozen theorem statement must appear verbatim; it is, byte for byte:\n{statement}"
        ));
    }
    Prepared::Send(without_header_imports(candidate))
}

/// The candidate with its header `import` lines blanked.
///
/// The REPL takes `import` only in a command that names no `env`; with an
/// `env` the line would be elaborated as an ordinary command and fail.
/// `check` runs every candidate in the resident process's `import Mathlib`
/// environment — the one import a registered frozen file may have — so the
/// header is dropped, not honoured: a candidate importing anything else is
/// checked under Mathlib alone. `verify` (a cold, complete `lean` run) runs
/// the frozen file's own preamble in place of the candidate's, and refuses a
/// candidate whose header differs from it (`PreambleMismatch`).
///
/// Each import line is replaced by an empty line rather than removed, so the
/// line numbers in the reply are the candidate's own.
pub fn without_header_imports(candidate: &str) -> String {
    let mut out = String::with_capacity(candidate.len());
    let mut in_header = true;
    for line in candidate.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if in_header && body.trim_start().starts_with("import ") {
            out.push_str(line.get(body.len()..).unwrap_or_default());
        } else {
            if in_header && !body.trim().is_empty() {
                in_header = false;
            }
            out.push_str(line);
        }
    }
    out
}

pub(crate) fn bounded(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push_str(" …[truncated]");
    cut
}

/// Lean's verdict on a checked candidate, from the REPL's reply: messages of
/// severity `error` become [`Diagnostic`]s and every explicit `sorry` becomes
/// an [`OpenGoal`] with its goal text; the status is `Errors` if any error,
/// else `Sorries` if any sorry, else `Complete`. Positions are as the REPL
/// reports them (1-based line, 0-based column) and refer to the candidate's
/// own lines, since [`without_header_imports`] preserves line numbering.
pub fn check_response(reply: &CommandReply, elapsed: Duration) -> CheckResponse {
    let status = status_of(reply);
    let errors: Vec<Diagnostic> = reply
        .messages
        .iter()
        .filter(|m| m.severity == "error")
        .take(MAX_CHECK_ITEMS)
        .map(|m| Diagnostic { line: m.line, column: m.column, message: bounded(&m.data, MAX_MESSAGE_CHARS) })
        .collect();
    let sorries: Vec<OpenGoal> = reply
        .sorries
        .iter()
        .take(MAX_CHECK_ITEMS)
        .map(|s| OpenGoal { line: s.line, column: s.column, goal: bounded(&s.goal, MAX_GOAL_CHARS) })
        .collect();
    CheckResponse {
        status,
        invalid_reason: None,
        errors,
        sorries,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    }
}

/// The status a command reply stands for: `Errors` if any message has
/// severity `error`, else `Sorries` if any `sorry`, else `Complete`.
pub(crate) fn status_of(reply: &CommandReply) -> CheckStatus {
    if reply.has_errors() {
        CheckStatus::Errors
    } else if !reply.sorries.is_empty() {
        CheckStatus::Sorries
    } else {
        CheckStatus::Complete
    }
}

/// Why one attempt to elaborate on the resident REPL failed.
enum Attempt {
    /// The process is gone, or was killed for not answering in time: a fresh
    /// one may succeed.
    Dead(String),
    /// The process answered, but not with a usable reply; a respawn would not
    /// change that.
    Failed(String),
}

/// Default number of resident operations one REPL process serves before it
/// is replaced (`--repl-max-requests`).
pub const DEFAULT_REPL_MAX_REQUESTS: u32 = 100;

/// The resident Lean REPL behind `check`: one process, `import Mathlib` done
/// once, its environment id reused for every candidate (each check builds a
/// throwaway child environment on it, so candidates never see each other).
///
/// Ownership: the checker owns the child process; dropping it kills the
/// process ([`Repl`]'s drop). Failure: a request on a dead REPL, or one that
/// outlives the request timeout (which kills the process), is retried **once**
/// on a freshly spawned REPL (Mathlib imported again); if that fails too, or
/// the reply is malformed, the caller gets the reason as an error — never a
/// verdict Lean did not give. The checker never confines the process; the
/// launcher does (no network, workspace and toolchain read-only, scratch
/// writable).
///
/// Recycling: the REPL keeps every environment and proof state it has
/// created, so a long-lived process only grows. Every resident operation
/// (`check`, `analyze`, `complete`) counts against one request
/// budget, failed ones included; before the operation that would exceed it,
/// the old process is killed and reaped, a new one imports Mathlib, and every
/// id of the old process (the cached analysis roots) is discarded. An
/// analysis is never interrupted: its internal commands share the ids of the
/// process it started on. The budget bounds what the process retains between
/// operations, not the peak of one operation.
///
/// `analyze` and `complete` are not retried: a REPL that dies or hangs during
/// one ends it with `error` in the reply (the fields obtained so far kept);
/// the checker is left without a process and the next call, of any kind,
/// spawns a fresh one.
pub struct ReplChecker {
    config: ReplConfig,
    repl: Option<Repl>,
    mathlib_env: u64,
    respawns: u32,
    /// Resident operations begun on the current process.
    requests: u32,
    max_requests: u32,
    restarts: u32,
    /// Per frozen file, what the analysis of every candidate shares
    /// (environment ids of the current process; cleared whenever the process
    /// changes).
    roots: HashMap<String, sketch::Root>,
}

impl std::fmt::Debug for ReplChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplChecker")
            .field("repl", &self.repl)
            .field("mathlib_env", &self.mathlib_env)
            .field("respawns", &self.respawns)
            .field("requests", &self.requests)
            .field("max_requests", &self.max_requests)
            .field("restarts", &self.restarts)
            .finish_non_exhaustive()
    }
}

impl ReplChecker {
    /// [`Self::spawn_with_max_requests`] with the default request budget, for
    /// the tests.
    #[cfg(test)]
    pub fn spawn(config: ReplConfig) -> Result<Self, String> {
        Self::spawn_with_max_requests(config, DEFAULT_REPL_MAX_REQUESTS)
    }

    /// Spawn the REPL and run `import Mathlib`, with a request budget of
    /// `max_requests` resident operations per process, which must be
    /// positive. Fails if the process cannot start or the import does not
    /// answer cleanly; the service then refuses to start rather than serve
    /// `check` without a backend.
    pub fn spawn_with_max_requests(config: ReplConfig, max_requests: u32) -> Result<Self, String> {
        if max_requests == 0 {
            return Err("REPL max requests must be positive".to_owned());
        }
        let (repl, mathlib_env) = Self::start(&config)?;
        Ok(Self {
            config,
            repl: Some(repl),
            mathlib_env,
            respawns: 0,
            requests: 0,
            max_requests,
            restarts: 0,
            roots: HashMap::new(),
        })
    }

    fn start(config: &ReplConfig) -> Result<(Repl, u64), String> {
        let mut repl = Repl::spawn(config).map_err(|e| e.to_string())?;
        let imported = repl
            .command("import Mathlib", None)
            .map_err(|e| format!("import Mathlib: {e}; stderr: {}", excerpt(&repl.stderr_tail())))?;
        if imported.has_errors() {
            let errors: Vec<&str> =
                imported.messages.iter().filter(|m| m.severity == "error").map(|m| m.data.as_str()).collect();
            return Err(format!("import Mathlib failed: {}", excerpt(&errors.join("; "))));
        }
        Ok((repl, imported.env))
    }

    /// The environment id `import Mathlib` produced (0 on a fresh REPL).
    #[must_use]
    pub fn mathlib_env(&self) -> u64 {
        self.mathlib_env
    }

    /// How many times the REPL has been respawned after dying or timing out.
    #[cfg(test)]
    #[must_use]
    pub fn respawns(&self) -> u32 {
        self.respawns
    }

    /// How many times the REPL was replaced after its request budget.
    #[cfg(test)]
    #[must_use]
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Resident operations begun on the current process, failed ones
    /// included; the import at startup and an analysis's own commands are
    /// not operations.
    #[cfg(test)]
    #[must_use]
    pub fn requests(&self) -> u32 {
        self.requests
    }

    /// Whether the REPL process is currently running.
    pub fn is_alive(&mut self) -> bool {
        self.repl.as_mut().is_some_and(Repl::is_alive)
    }

    /// The REPL's OS process id, if one is held.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.repl.as_ref().map(Repl::pid)
    }

    /// Elaborate `body` (a candidate prepared by [`prepare_check`]) in the
    /// Mathlib environment. Returns the reply and the wall time of the request
    /// that produced it. See the type's documentation for the retry and
    /// recycling rules.
    pub fn check(&mut self, body: &str) -> Result<(CommandReply, Duration), String> {
        self.check_within(body, self.config.request_timeout)
    }

    /// [`Self::check`] with `timeout` for the check's requests; the imports
    /// of a respawn keep the configured deadline.
    fn check_within(&mut self, body: &str, timeout: Duration) -> Result<(CommandReply, Duration), String> {
        self.begin_request()?;
        match self.send(body, timeout) {
            Ok(done) => Ok(done),
            Err(Attempt::Failed(why)) => Err(why),
            Err(Attempt::Dead(why)) => {
                self.respawn().map_err(|e| format!("{why}; respawning the REPL failed: {e}"))?;
                self.begin_request()?;
                self.send(body, timeout).map_err(|again| match again {
                    Attempt::Dead(w) | Attempt::Failed(w) => format!("{why}; after a respawn: {w}"),
                })
            }
        }
    }

    fn send(&mut self, body: &str, timeout: Duration) -> Result<(CommandReply, Duration), Attempt> {
        let Some(mut repl) = self.repl.take() else {
            return Err(Attempt::Dead("the Lean REPL is not running".to_owned()));
        };
        if !repl.is_alive() {
            return Err(Attempt::Dead(format!(
                "the Lean REPL exited; stderr: {}",
                excerpt(&repl.stderr_tail())
            )));
        }
        let started = Instant::now();
        match repl.command_with(body, Some(self.mathlib_env), false, timeout) {
            Ok(reply) => {
                self.repl = Some(repl);
                Ok((reply, started.elapsed()))
            }
            Err(e @ (ReplError::Timeout | ReplError::Io(_) | ReplError::Closed)) => {
                // `repl` is dead (the client killed it) and is dropped here.
                Err(Attempt::Dead(format!("{e}; stderr: {}", excerpt(&repl.stderr_tail()))))
            }
            Err(e @ (ReplError::Protocol(_) | ReplError::Spawn(_))) => {
                // A protocol violation leaves the stream in sync (or has
                // already killed the process, which the next send notices).
                self.repl = Some(repl);
                Err(Attempt::Failed(e.to_string()))
            }
        }
    }

    fn respawn(&mut self) -> Result<(), String> {
        self.repl = None;
        self.roots.clear();
        self.requests = 0;
        let (repl, mathlib_env) = Self::start(&self.config)?;
        self.repl = Some(repl);
        self.mathlib_env = mathlib_env;
        self.respawns += 1;
        Ok(())
    }

    /// Count one resident operation, first replacing the process when its
    /// budget is spent. Only called between operations, so no analysis loses
    /// its ids.
    fn begin_request(&mut self) -> Result<(), String> {
        if self.requests >= self.max_requests {
            let (old, served) = (self.pid(), self.requests);
            self.respawn()?;
            self.respawns -= 1;
            self.restarts += 1;
            eprintln!(
                "nucleus-lean: replaced the resident REPL after {served} requests (pid {old:?} -> {:?})",
                self.pid()
            );
        }
        self.requests += 1;
        Ok(())
    }

    /// Take the process for one analysis, whose commands share its ids: spawn
    /// one if none is running (a death noticed here counts as a respawn),
    /// then count the operation.
    fn take_for_analysis(&mut self) -> Result<Repl, String> {
        if !self.is_alive() {
            self.respawn()?;
        }
        self.begin_request()?;
        self.repl.take().ok_or_else(|| "the Lean REPL is not running".to_owned())
    }

    /// Put the process back after an analysis; a dead one is dropped, with
    /// the per-file preparation that lived in it.
    fn give_back(&mut self, mut repl: Repl) {
        if repl.is_alive() {
            self.repl = Some(repl);
        } else {
            self.roots.clear();
        }
    }

    /// The sketch analysis of `body` (a candidate prepared by
    /// [`prepare_check`]) for `problem`, whose frozen file is `frozen` and
    /// frozen statement `statement`. `Err` only when no REPL could be
    /// spawned; every other failure is in the reply's `error`.
    pub fn analyze(
        &mut self,
        problem: &str,
        frozen: &str,
        statement: &str,
        body: &str,
    ) -> Result<AnalyzeResponse, String> {
        let mut repl = self.take_for_analysis()?;
        let frozen_body = without_header_imports(frozen);
        let input = sketch::Input {
            thm: problem,
            frozen_body: &frozen_body,
            statement,
            body,
            mathlib_env: self.mathlib_env,
        };
        let mut driver = sketch::Driver::new(&mut repl, sketch::ANALYZE_WALL, sketch::ANALYZE_WALL);
        let reply = sketch::analyze(&mut driver, &mut self.roots, &input);
        self.give_back(repl);
        Ok(reply)
    }

    /// The completion procedure on `candidate` (the whole file; it must have
    /// passed [`prepare_check`]). `Err` only when no REPL could be spawned.
    pub fn complete(&mut self, candidate: &str) -> Result<CompleteResponse, String> {
        let mut repl = self.take_for_analysis()?;
        let mut driver =
            sketch::Driver::new(&mut repl, completion::COMPLETE_WALL, completion::COMPLETE_REQUEST_WALL);
        let reply = completion::complete(&mut driver, self.mathlib_env, candidate);
        self.give_back(repl);
        Ok(reply)
    }
}

/// `check` over a fake REPL: a shell script that speaks the REPL's framing
/// and answers by keyword, so the whole path — preparation, the resident
/// client, respawn on death or timeout, reply mapping — runs without Lean.
#[cfg(test)]
mod check_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::repl;
    use crate::service_tests::{path, workspace_with};
    use crate::{
        ACTION_ANALYZE, ACTION_CHECK, ACTION_COMPLETE, ANALYSIS_SUITE_ID, AnalyzeRequest, CheckRequest,
        CompleteRequest, ErrorCode, LeanService, Payload, RunError, SketchCounts, serve,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    const FROZEN: &str = "import Mathlib\n\ntheorem putnam_chk (n : Nat) : n = n :=\nsorry\n";
    const STATEMENT: &str = "theorem putnam_chk (n : Nat) : n = n :=";

    /// `$HOME` is the test's private directory; marker files there carry state
    /// across respawns (`died`: the one death already happened; `fail_import`:
    /// answer `import Mathlib` with an error; `imports`: one line per import).
    const FAKE_REPL: &str = r#"#!/bin/sh
req=""
while IFS= read -r line; do
  case "$line" in
    "") [ -n "$req" ] || continue ;;
    *) req="$req$line"; continue ;;
  esac
  case "$req" in
    *'"cmd":"import Mathlib"'*)
      printf 'import\n' >> "$HOME/imports"
      if [ -e "$HOME/fail_import" ]; then
        printf '%s\n\n' '{"messages": [{"severity": "error", "pos": {"line": 1, "column": 0}, "data": "unknown module prefix Mathlib"}], "env": 0}'
      else
        printf '{"env": 0}\n\n'
      fi ;;
    *die_once*) if [ -e "$HOME/died" ]; then printf '{"env": 5}\n\n'; else : > "$HOME/died"; exit 1; fi ;;
    *die_always*) exit 1 ;;
    *hang_forever*) : ;;
    *'"tactic"'*) cat <<'EOF'
{"proofState": 7,
 "goals": ["n : Nat\nh : 2 ≤ n\n⊢ 0 < n"],
 "proofStatus": "Incomplete: open goals remain"}

EOF
    ;;
    *no_such_goal*) cat <<'EOF'
{"messages":
 [{"severity": "error",
   "pos": {"line": 1, "column": 8},
   "endPos": {"line": 1, "column": 20},
   "data": "unknown identifier 'Nonsense'"}],
 "env": 14}

EOF
    ;;
    *'by exact?'*) cat <<'EOF'
{"messages":
 [{"severity": "info",
   "pos": {"line": 1, "column": 21},
   "endPos": {"line": 1, "column": 29},
   "data": "Try this: exact Nat.le_refl n"}],
 "env": 12}

EOF
    ;;
    *'by apply?'*) cat <<'EOF'
{"messages":
 [{"severity": "error",
   "pos": {"line": 1, "column": 21},
   "endPos": {"line": 1, "column": 29},
   "data": "`apply?` could not close the goal"}],
 "env": 13}

EOF
    ;;
    *try_this*) cat <<'EOF'
{"messages":
 [{"severity": "info",
   "pos": {"line": 1, "column": 21},
   "endPos": {"line": 1, "column": 29},
   "data": "Try this: exact Nat.le_refl n"}],
 "env": 11}

EOF
    ;;
    *leaf_hole*) cat <<'EOF'
{"messages":
 [{"severity": "warning",
   "pos": {"line": 4, "column": 7},
   "endPos": {"line": 4, "column": 8},
   "data": "unused variable `h`\n\nNote: This linter can be disabled with `set_option linter.unusedVariables false`"}],
 "env": 9}

EOF
    ;;
    *allTactics*) cat <<'EOF'
{"tactics":
 [{"tactic": "have h : n = n := rfl",
   "proofState": 3,
   "pos": {"line": 4, "column": 2},
   "goals": "n : Nat\n⊢ n = n",
   "endPos": {"line": 4, "column": 23}},
  {"tactic": "sorry",
   "proofState": 4,
   "pos": {"line": 5, "column": 2},
   "goals": "n : Nat\nh : n = n\n⊢ n = n",
   "endPos": {"line": 5, "column": 7}}],
 "sorries":
 [{"proofState": 4,
   "pos": {"line": 5, "column": 2},
   "goal": "n : Nat\nh : n = n\n⊢ n = n",
   "endPos": {"line": 5, "column": 7}}],
 "env": 8}

EOF
    ;;
    *'(by rfl)'*) printf '{"env": 12}\n\n' ;;
    *sketch_me*) cat <<'EOF'
{"sorries":
 [{"proofState": 0,
   "pos": {"line": 5, "column": 2},
   "goal": "n : Nat\nh : n = n\n⊢ n = n",
   "endPos": {"line": 5, "column": 7}}],
 "env": 3}

EOF
    ;;
    *bad_tac*) cat <<'EOF'
{"messages":
 [{"severity": "error",
   "pos": {"line": 4, "column": 2},
   "endPos": {"line": 4, "column": 9},
   "data": "unknown tactic"},
  {"severity": "warning",
   "pos": {"line": 3, "column": 8},
   "endPos": {"line": 3, "column": 18},
   "data": "declaration uses 'sorry'"}],
 "sorries":
 [{"proofState": 0,
   "pos": {"line": 5, "column": 2},
   "goal": "n : Nat\n⊢ n = n",
   "endPos": {"line": 5, "column": 7}}],
 "env": 2}

EOF
    ;;
    *sorry*) cat <<'EOF'
{"sorries":
 [{"proofState": 0,
   "pos": {"line": 5, "column": 2},
   "goal": "n : Nat\nh : n = n\n⊢ n = n",
   "endPos": {"line": 5, "column": 7}},
  {"proofState": 1,
   "pos": {"line": 7, "column": 2},
   "goal": "⊢ True",
   "endPos": {"line": 7, "column": 7}}],
 "messages":
 [{"severity": "warning",
   "pos": {"line": 3, "column": 8},
   "endPos": {"line": 3, "column": 18},
   "data": "declaration uses 'sorry'"}],
 "env": 3}

EOF
    ;;
    *) printf '{"env": 1}\n\n' ;;
  esac
  req=""
done
"#;

    /// Written once per test process, before any spawn (see the note on
    /// `ETXTBSY` in `repl::tests`).
    fn fake_exe() -> PathBuf {
        static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        EXE.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("nucleus_lean_check_fake_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("repl.sh");
            std::fs::write(&exe, FAKE_REPL).unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            exe
        })
        .clone()
    }

    fn fake_config(name: &str, timeout: Duration) -> (ReplConfig, PathBuf) {
        let home = std::env::temp_dir().join(format!("nucleus_lean_check_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = ReplConfig {
            exe: fake_exe(),
            path_env: "/usr/bin:/bin".to_owned(),
            lean_path: String::new(),
            home: home.clone(),
            cwd: home.clone(),
            request_timeout: timeout,
        };
        (config, home)
    }

    /// A checker over the fake, retrying the spawn while another test's fork
    /// still holds the script open (`ETXTBSY`, os error 26).
    fn fake_checker(config: &ReplConfig) -> ReplChecker {
        fake_checker_with(config, DEFAULT_REPL_MAX_REQUESTS)
    }

    /// [`fake_checker`] with a request budget.
    fn fake_checker_with(config: &ReplConfig, max_requests: u32) -> ReplChecker {
        let mut tries = 0;
        loop {
            match ReplChecker::spawn_with_max_requests(config.clone(), max_requests) {
                Ok(checker) => return checker,
                Err(e) if e.contains("os error 26") && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("fake REPL: {e}"),
            }
        }
    }

    /// A cold runner that must never be reached by `check`.
    type Cold = fn(&str) -> Result<String, RunError>;

    /// A service with the fake REPL behind `check`; the cold runner must never run.
    fn service(name: &str, timeout: Duration) -> (LeanService<Cold>, PathBuf, PathBuf) {
        let ws = workspace_with(&format!("chk_{name}"), FROZEN);
        std::fs::rename(ws.join(format!("chk_{name}.lean")), ws.join("putnam_chk.lean")).unwrap();
        let (config, home) = fake_config(name, timeout);
        let checker = fake_checker(&config);
        assert_eq!(checker.mathlib_env(), 0);
        let cold: Cold = |_| panic!("check must not run cold lean");
        (LeanService::new(&ws, cold).with_checker(checker), ws, home)
    }

    fn candidate(proof: &str) -> CheckRequest {
        CheckRequest { candidate: format!("import Mathlib\n\n{STATEMENT} by\n{proof}\n") }
    }

    fn cleanup(ws: &Path, home: &Path) {
        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(home);
    }

    // ---- the pure parts ----

    #[test]
    fn prepare_allows_sorry_and_bans_native_decide_axioms_and_a_changed_statement() {
        let partial = format!("import Mathlib\n\n{STATEMENT} by\n  sorry\n");
        assert_eq!(
            prepare_check(STATEMENT, &partial),
            Prepared::Send(format!("\n\n{STATEMENT} by\n  sorry\n"))
        );
        let nd = format!("import Mathlib\n\n{STATEMENT} by native_decide\n");
        assert_eq!(
            prepare_check(STATEMENT, &nd),
            Prepared::Invalid("forbidden token `native_decide`".into())
        );
        let ax = format!("import Mathlib\n\naxiom cheat : False\n\n{STATEMENT} by exact cheat.elim\n");
        assert_eq!(prepare_check(STATEMENT, &ax), Prepared::Invalid("forbidden token `axiom`".into()));
        let rewritten = "import Mathlib\n\ntheorem putnam_chk (n : Nat) : n = n ∨ True := by simp\n";
        assert_eq!(
            prepare_check(STATEMENT, rewritten),
            Prepared::Invalid(format!(
                "the frozen theorem statement must appear verbatim; it is, byte for byte:\n{STATEMENT}"
            ))
        );
        assert_eq!(check_forbidden_token("by sorry"), None, "sorry is allowed in a partial proof");
    }

    #[test]
    fn header_imports_are_blanked_and_line_numbers_kept() {
        let src = "import Mathlib\nimport Mathlib.Tactic\n\nopen Nat\n\n-- import mentioned later stays\ntheorem t : True := by\n  sorry\n";
        let sent = without_header_imports(src);
        assert_eq!(
            sent,
            "\n\n\nopen Nat\n\n-- import mentioned later stays\ntheorem t : True := by\n  sorry\n"
        );
        assert_eq!(sent.lines().count(), src.lines().count());
        assert!(!sent.contains("import Mathlib"));
        assert_eq!(without_header_imports("theorem t : True := trivial"), "theorem t : True := trivial");
        assert_eq!(
            without_header_imports("import Mathlib\r\ntheorem t : True := trivial\r\n"),
            "\r\ntheorem t : True := trivial\r\n"
        );
    }

    #[test]
    fn a_reply_maps_to_errors_goals_and_a_status() {
        use repl::{Message, Sorry};
        let warning = Message {
            severity: "warning".into(),
            data: "declaration uses 'sorry'".into(),
            line: 3,
            column: 8,
        };
        let error = Message { severity: "error".into(), data: "unknown tactic".into(), line: 4, column: 2 };
        let sorry = Sorry {
            proof_state: 0,
            goal: "n : Nat\n⊢ n = n".into(),
            line: 5,
            column: 2,
            end_line: 5,
            end_column: 7,
        };
        let clean = CommandReply { env: 1, messages: vec![], sorries: vec![], tactics: vec![] };
        let got = check_response(&clean, Duration::from_millis(80));
        assert_eq!(
            got,
            CheckResponse {
                status: CheckStatus::Complete,
                invalid_reason: None,
                errors: vec![],
                sorries: vec![],
                elapsed_ms: 80
            }
        );
        let partial = CommandReply {
            env: 2,
            messages: vec![warning.clone()],
            sorries: vec![sorry.clone()],
            tactics: vec![],
        };
        let got = check_response(&partial, Duration::from_millis(1));
        assert_eq!(got.status, CheckStatus::Sorries);
        assert!(got.errors.is_empty(), "a warning is not an error");
        assert_eq!(got.sorries, vec![OpenGoal { line: 5, column: 2, goal: "n : Nat\n⊢ n = n".into() }]);
        let broken =
            CommandReply { env: 3, messages: vec![warning, error], sorries: vec![sorry], tactics: vec![] };
        let got = check_response(&broken, Duration::from_millis(1));
        assert_eq!(got.status, CheckStatus::Errors, "an error wins over a remaining sorry");
        assert_eq!(got.errors, vec![Diagnostic { line: 4, column: 2, message: "unknown tactic".into() }]);
        assert_eq!(got.sorries.len(), 1);
    }

    #[test]
    fn replies_are_bounded_so_they_fit_the_kernel_payload() {
        use repl::{Message, Sorry};
        let error = |i: u64| Message {
            severity: "error".into(),
            data: "e".repeat(MAX_MESSAGE_CHARS + 10),
            line: i,
            column: 0,
        };
        let sorry = |i: u64| Sorry {
            proof_state: i,
            goal: "⊢ ".repeat(MAX_GOAL_CHARS),
            line: i,
            column: 0,
            end_line: i,
            end_column: 5,
        };
        let reply = CommandReply {
            env: 1,
            messages: (0..(MAX_CHECK_ITEMS as u64 + 5)).map(error).collect(),
            sorries: (0..(MAX_CHECK_ITEMS as u64 + 5)).map(sorry).collect(),
            tactics: vec![],
        };
        let got = check_response(&reply, Duration::ZERO);
        assert_eq!(got.errors.len(), MAX_CHECK_ITEMS);
        assert_eq!(got.sorries.len(), MAX_CHECK_ITEMS);
        assert!(got.errors[0].message.ends_with("…[truncated]"));
        assert_eq!(
            got.errors[0].message.chars().count(),
            MAX_MESSAGE_CHARS + " …[truncated]".chars().count()
        );
        assert!(got.sorries[0].goal.ends_with("…[truncated]"));
        assert_eq!(got.sorries[0].goal.chars().count(), MAX_GOAL_CHARS + " …[truncated]".chars().count());
        let encoded = serde_json::to_vec(&got).unwrap();
        assert!(encoded.len() < 256 * 1024, "{} bytes", encoded.len());
    }

    #[test]
    fn a_clean_candidate_is_complete_and_dispatches_through_serve() {
        let (mut service, ws, home) = service("clean", Duration::from_secs(10));
        let payload = Payload::encode(&candidate("  rfl")).unwrap();
        let reply: CheckResponse =
            serve(&mut service, ACTION_CHECK, &path(&["putnam_chk"]), &payload).unwrap().decode().unwrap();
        assert_eq!(reply.status, CheckStatus::Complete);
        assert_eq!(reply.invalid_reason, None);
        assert!(reply.errors.is_empty() && reply.sorries.is_empty(), "{reply:?}");
        assert_eq!(service.checker().unwrap().respawns(), 0);
        // Terminating the service (dropping it) kills the REPL it owns.
        let pid = service.checker().unwrap().pid().unwrap();
        assert!(crate::test_process_exists(pid));
        drop(service);
        assert!(!crate::test_process_exists(pid), "the REPL must not outlive its owner");
        cleanup(&ws, &home);
    }

    #[test]
    fn remaining_sorries_come_back_as_goals_with_positions() {
        let (mut service, ws, home) = service("sorries", Duration::from_secs(10));
        let reply =
            service.check(&path(&["putnam_chk"]), &candidate("  have h : n = n := rfl\n  sorry")).unwrap();
        assert_eq!(reply.status, CheckStatus::Sorries);
        assert!(reply.errors.is_empty(), "the 'uses sorry' warning is not an error: {reply:?}");
        assert_eq!(
            reply.sorries,
            vec![
                OpenGoal { line: 5, column: 2, goal: "n : Nat\nh : n = n\n⊢ n = n".into() },
                OpenGoal { line: 7, column: 2, goal: "⊢ True".into() },
            ]
        );
        cleanup(&ws, &home);
    }

    #[test]
    fn lean_errors_are_mapped_and_outrank_sorries() {
        let (mut service, ws, home) = service("errors", Duration::from_secs(10));
        let reply = service.check(&path(&["putnam_chk"]), &candidate("  bad_tac\n  sorry")).unwrap();
        assert_eq!(reply.status, CheckStatus::Errors);
        assert_eq!(reply.errors, vec![Diagnostic { line: 4, column: 2, message: "unknown tactic".into() }]);
        assert_eq!(reply.sorries.len(), 1, "the sorry is still reported: {reply:?}");
        cleanup(&ws, &home);
    }

    #[test]
    fn an_invalid_attempt_never_reaches_the_repl() {
        let (mut service, ws, home) = service("invalid", Duration::from_secs(10));
        // Both candidates carry the keyword that would kill the fake for good.
        let nd = service.check(&path(&["putnam_chk"]), &candidate("  die_always native_decide")).unwrap();
        assert_eq!(nd.status, CheckStatus::Invalid);
        assert_eq!(nd.invalid_reason.as_deref(), Some("forbidden token `native_decide`"));
        assert_eq!(nd.elapsed_ms, 0);
        let rewritten = CheckRequest {
            candidate: "import Mathlib\n\ntheorem putnam_chk (n : Nat) : n = n ∨ True := by\n  die_always\n"
                .into(),
        };
        let mismatch = service.check(&path(&["putnam_chk"]), &rewritten).unwrap();
        assert_eq!(mismatch.status, CheckStatus::Invalid);
        assert!(mismatch.invalid_reason.as_deref().unwrap().contains("verbatim"), "{mismatch:?}");
        // Sorry alone is fine, and the REPL was never asked anything lethal.
        let ok = service.check(&path(&["putnam_chk"]), &candidate("  sorry")).unwrap();
        assert_eq!(ok.status, CheckStatus::Sorries);
        assert_eq!(service.checker().unwrap().respawns(), 0);
        cleanup(&ws, &home);
    }

    #[test]
    fn a_repl_that_dies_is_respawned_once_and_the_check_retried() {
        let (mut service, ws, home) = service("die_once", Duration::from_secs(10));
        let first_pid = service.checker().unwrap().pid().unwrap();
        // The fake exits without answering the first time it sees this.
        let reply = service.check(&path(&["putnam_chk"]), &candidate("  die_once")).unwrap();
        assert_eq!(reply.status, CheckStatus::Complete, "{reply:?}");
        let checker = service.checker().unwrap();
        assert_eq!(checker.respawns(), 1);
        assert_ne!(checker.pid().unwrap(), first_pid, "a fresh process, Mathlib imported again");
        assert_eq!(checker.mathlib_env(), 0);
        // The replacement keeps serving.
        assert_eq!(
            service.check(&path(&["putnam_chk"]), &candidate("  rfl")).unwrap().status,
            CheckStatus::Complete
        );
        assert_eq!(service.checker().unwrap().respawns(), 1);
        cleanup(&ws, &home);
    }

    #[test]
    fn dying_again_after_the_respawn_is_an_error_not_a_verdict() {
        let (mut service, ws, home) = service("die_always", Duration::from_secs(10));
        let err = service.check(&path(&["putnam_chk"]), &candidate("  die_always")).unwrap_err();
        assert_eq!(err.code, ErrorCode::ServiceError);
        assert!(err.message.contains("after a respawn"), "{}", err.message);
        assert_eq!(service.checker().unwrap().respawns(), 1, "exactly one respawn per check");
        // The next check starts from no process: respawn, then a verdict.
        let reply = service.check(&path(&["putnam_chk"]), &candidate("  rfl")).unwrap();
        assert_eq!(reply.status, CheckStatus::Complete);
        assert_eq!(service.checker().unwrap().respawns(), 2);
        cleanup(&ws, &home);
    }

    #[test]
    fn a_timed_out_request_is_retried_once_on_a_fresh_repl_then_reported() {
        // Only the hanging check and its retry get the short deadline; the
        // fake's imports keep the long one.
        let (config, home) = fake_config("hang", Duration::from_secs(10));
        let mut checker = fake_checker(&config);
        let started = Instant::now();
        let err = checker
            .check_within("theorem t : True := by hang_forever", Duration::from_millis(100))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "bounded by two deadlines, not forever");
        assert!(err.contains("did not answer within the deadline"), "{err}");
        assert!(err.contains("after a respawn"), "{err}");
        assert_eq!(checker.respawns(), 1);
        assert!(checker.check("theorem t : True := by rfl").unwrap().0.messages.is_empty());
        drop(checker);
        let _ = std::fs::remove_dir_all(home);
    }

    /// `analyze` and `complete` through the dispatcher over the fake: the
    /// wiring and the reply shapes, and the REPL replaced after its request
    /// budget, which every resident operation shares.
    #[test]
    fn resident_operations_dispatch_and_recycle_at_the_budget() {
        const BUDGET: u32 = 5;
        let (mut service, ws, home) = service("analyses", Duration::from_secs(10));
        service.checker.as_mut().unwrap().max_requests = BUDGET;
        let request = AnalyzeRequest {
            candidate: format!(
                "import Mathlib\n\n{STATEMENT} by\n  have h : n = n := rfl\n  sorry -- sketch_me\n"
            ),
        };
        let payload = Payload::encode(&request).unwrap();
        let reply: AnalyzeResponse =
            serve(&mut service, ACTION_ANALYZE, &path(&["putnam_chk"]), &payload).unwrap().decode().unwrap();
        assert_eq!(reply.error, None, "{reply:?}");
        assert_eq!(reply.status, CheckStatus::Sorries);
        assert_eq!(reply.suite, ANALYSIS_SUITE_ID);
        // The fake cannot restate the step (its tactic reply has one goal) and
        // lints `h` as unused; its leaf is untestable (the fake's revert
        // leaves locals). The counts follow.
        assert_eq!((reply.steps.len(), reply.leaves.len()), (1, 1), "{reply:?}");
        assert_eq!((reply.steps[0].unused, reply.steps[0].counted), (true, false));
        assert_eq!(reply.leaves[0].verdict, nucleus_lean_interface::LeafVerdict::Untestable);
        assert_eq!(
            reply.counts,
            SketchCounts { closed: 0, closed_with_bullets: 0, closed_leaf_used: 0, open: 1, untestable: 1 }
        );
        let complete = CompleteRequest { candidate: request.candidate.clone() };
        let payload = Payload::encode(&complete).unwrap();
        let reply: CompleteResponse =
            serve(&mut service, ACTION_COMPLETE, &path(&["putnam_chk"]), &payload).unwrap().decode().unwrap();
        assert_eq!(reply.error, None, "{reply:?}");
        assert!(reply.all_closed, "{reply:?}");
        assert_eq!(reply.closers[0].tactic.as_deref(), Some("rfl"));
        assert_eq!(reply.closers[0].position, nucleus_lean_interface::SorryPosition::Term);
        assert!(reply.spliced.as_deref().unwrap().contains("(by rfl) -- sketch_me"), "{reply:?}");
        // `check` shares the budget with the two analyses above.
        service.check(&path(&["putnam_chk"]), &candidate("  rfl")).unwrap();
        let checker = service.checker().unwrap();
        assert_eq!((checker.requests(), checker.restarts(), checker.respawns()), (3, 0, 0));
        let first_pid = checker.pid().unwrap();
        for _ in 3..BUDGET {
            service.analyze(&path(&["putnam_chk"]), &request).unwrap();
        }
        let checker = service.checker().unwrap();
        assert_eq!((checker.requests(), checker.pid().unwrap()), (BUDGET, first_pid), "not yet");
        service.analyze(&path(&["putnam_chk"]), &request).unwrap();
        let checker = service.checker().unwrap();
        assert_eq!((checker.requests(), checker.restarts(), checker.respawns()), (1, 1, 0));
        assert_ne!(checker.pid().unwrap(), first_pid, "a fresh process after the request budget");
        assert!(!crate::test_process_exists(first_pid), "the old one is gone");
        cleanup(&ws, &home);
    }

    #[test]
    fn mixed_resident_operations_recycle_before_dispatch_and_discard_cached_roots() {
        let (mut service, ws, home) = service("mixed_recycling", Duration::from_secs(10));
        service.checker.as_mut().unwrap().max_requests = 2;
        let original = service.checker().unwrap().pid().unwrap();
        let analyze = AnalyzeRequest {
            candidate: format!(
                "import Mathlib\n\n{STATEMENT} by\n  have h : n = n := rfl\n  sorry -- sketch_me\n"
            ),
        };
        assert_eq!(service.analyze(&path(&["putnam_chk"]), &analyze).unwrap().error, None);
        assert!(!service.checker.as_ref().unwrap().roots.is_empty());
        let bad = service.check(&path(&["putnam_chk"]), &candidate("  bad_tac")).unwrap();
        assert_eq!(bad.status, CheckStatus::Errors, "elaboration failures consume the budget");
        assert_eq!(
            (service.checker().unwrap().requests(), service.checker().unwrap().pid()),
            (2, Some(original))
        );
        service.check(&path(&["putnam_chk"]), &candidate("  bad_tac")).unwrap();
        let second = service.checker().unwrap().pid().unwrap();
        assert_ne!(second, original);
        assert!(!crate::test_process_exists(original), "recycling reaps the previous process");
        assert!(
            service.checker.as_ref().unwrap().roots.is_empty(),
            "old environment ids cannot survive recycling"
        );
        assert_eq!(service.checker().unwrap().requests(), 1, "the check enters the new process's budget");
        let reply = service
            .complete(&path(&["putnam_chk"]), &CompleteRequest { candidate: analyze.candidate.clone() })
            .unwrap();
        assert_eq!(reply.error, None, "internal completion requests retain their generation");
        assert_eq!(
            (service.checker().unwrap().requests(), service.checker().unwrap().pid()),
            (2, Some(second))
        );
        assert_eq!(service.analyze(&path(&["putnam_chk"]), &analyze).unwrap().error, None);
        let checker = service.checker().unwrap();
        assert_ne!(checker.pid(), Some(second));
        assert_eq!((checker.requests(), checker.restarts(), checker.respawns()), (1, 2, 0));
        assert!(!checker.roots.is_empty(), "analysis rebuilt its root on the new generation");
        assert_eq!(std::fs::read_to_string(home.join("imports")).unwrap().lines().count(), 3);
        drop(service);
        cleanup(&ws, &home);
    }

    #[test]
    fn recycling_import_failure_reaps_old_process_and_returns_no_verdict() {
        let (config, home) = fake_config("recycle_import_failure", Duration::from_secs(5));
        let mut checker = fake_checker_with(&config, 1);
        checker.check("example : True := by trivial").unwrap();
        let old = checker.pid().unwrap();
        std::fs::write(home.join("fail_import"), "").unwrap();
        assert!(checker.check("example : True := by trivial").unwrap_err().contains("import Mathlib failed"));
        assert!(!crate::test_process_exists(old));
        assert_eq!(checker.pid(), None, "a failed replacement cannot keep serving");
        std::fs::remove_file(home.join("fail_import")).unwrap();
        checker.check("example : True := by trivial").unwrap();
        assert_eq!(checker.requests(), 1);
        assert_ne!(checker.pid(), Some(old));
        drop(checker);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_checker_refuses_to_start_without_a_working_repl() {
        let (mut config, home) = fake_config("missing", Duration::from_secs(5));
        config.exe = home.join("no_such_repl");
        let err = ReplChecker::spawn(config).unwrap_err();
        assert!(err.contains("cannot start the Lean REPL"), "{err}");
        let (config, home2) = fake_config("bad_import", Duration::from_secs(5));
        std::fs::write(home2.join("fail_import"), "").unwrap();
        let err = fake_checker_err(&config);
        assert!(err.contains("import Mathlib failed") && err.contains("unknown module"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&home2);
    }

    fn fake_checker_err(config: &ReplConfig) -> String {
        let mut tries = 0;
        loop {
            match ReplChecker::spawn(config.clone()) {
                Ok(_) => panic!("the checker must not start"),
                Err(e) if e.contains("os error 26") && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return e,
            }
        }
    }
}
