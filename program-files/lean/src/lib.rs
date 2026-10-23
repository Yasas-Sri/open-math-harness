//! PutnamBench Lean verifier core.
//!
//! Decides whether a candidate proof counts as a solve under the PutnamBench
//! protocol, the five checks below. The proof-checking effect — running
//! Lean — is injected as a closure, so all decision logic is pure and unit
//! testable without Lean; the service's closure ([`bounded_lean_runner`])
//! runs the toolchain `lean` in the workspace under a deadline.
//!
//! The frozen file a call is judged against comes only from the service's
//! side: its own read-only `<problem>.lean`, or the text the discovery
//! service registered for `[run, problem]` ([`LeanService::register`], kept
//! in memory by [`registry`]). No request carries it.
//!
//! Checks implemented:
//! 1. no forbidden token: `sorry`, `native_decide`, a user `axiom`, or anything
//!    that runs code while Lean elaborates ([`nucleus_lean_interface::exec_token`]);
//! 2. the frozen `theorem … :=` statement appears verbatim in the candidate,
//!    and the candidate begins with the frozen file's preamble
//!    ([`nucleus_lean_interface::preamble_objection`]); Lean then elaborates the frozen preamble
//!    itself, a separator command, and the candidate's text after its copy
//!    ([`verify::checked_source`]), so every definition the statement uses is
//!    the frozen one and nothing can continue its last declaration;
//! 3. that file elaborates cleanly;
//! 4. `#print axioms` shows no `sorryAx` and nothing outside the trusted three;
//!    checks 4 and 5 read only what follows a per-run marker the probe prints
//!    before the harness's own commands ([`verify::harness_report`]);
//! 5. the candidate theorem's fully-elaborated type (`set_option pp.all true in
//!    #check @thm`) is character-identical to the pristine statement's, computed
//!    once per frozen file by [`pristine_type`] from the file check 2 builds
//!    for the frozen file itself. This defeats a rewrite that keeps the
//!    surface text but elaborates it differently (a shadowed `Nat`, another
//!    instance, …). It compares the names of constants, not their bodies, so a
//!    redefined `def` or answer `abbrev` is check 2's to refuse.
//!
//! Beside `verify` the service provides `check`, the evaluator of a *partial*
//! proof for the Dream-RSI discovery tree: `sorry` allowed, every remaining
//! `sorry` reported with its goal, every error with its position.
//! It runs on a **resident Lean REPL** with `import Mathlib` done once (see
//! [`ReplChecker`]); without one (`--repl` absent) it is refused, never served
//! by a cold `lean` run pretending to be warm.
//!
//! `analyze` and `complete` (task-scoring protocol v3) run on the same
//! resident REPL with the same pre-checks as `check`; the procedures are in
//! [`sketch`] and [`completion`]. A candidate whose re-check is not
//! `sorries` comes back with that status and no sketch or closers.

// Test process lifetime through the host process table.
#[cfg(test)]
#[allow(clippy::expect_used)]
fn test_process_exists(pid: u32) -> bool {
    let result = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .expect("query the process table");
    assert!(matches!(result.status.code(), Some(0 | 1)), "ps failed: {result:?}");
    result.status.success()
}

// The protocol's textual rules live in the interface: the discovery harness
// applies the same ones before it accepts a submission, and two copies of a
// rule is one too many.
use nucleus_lean_interface::{
    check_forbidden_token, extract_statement, forbidden_token, frozen_source_objection,
};

// ---- The service: options, a pristine-type cache, the loop ----

pub mod checker;
pub mod completion;
#[cfg(test)]
mod real_lean;
pub mod registry;
pub mod repl;
pub mod sketch;
pub mod verify;

use checker::{
    DEFAULT_REPL_MAX_REQUESTS, Prepared, ReplChecker, check_response, prepare_check, without_header_imports,
};
pub(crate) use checker::{bounded, status_of};
use verify::{Invoke, RunError, bounded_lean_runner, pristine_type, verify_text};
pub(crate) use verify::{describe, excerpt, separator_position, starts_with_type_head};

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nucleus_lean_interface::{
    ACTION_ANALYZE, ACTION_CHECK, ACTION_COMPLETE, ACTION_FORGET, ACTION_REGISTER, ACTION_STATEMENT,
    ACTION_VERIFY, ANALYSIS_SUITE_ID, AnalyzeRequest, AnalyzeResponse, COMPLETION_SUITE_ID, CheckRequest,
    CheckResponse, CheckStatus, CompleteRequest, CompleteResponse, ENDPOINT, MAX_REGISTER_OUTPUT_BYTES,
    RegisterRequest, RegisterResponse, RegisterStage, SketchCounts, StatementRef, StatementResponse,
    VerifyRequest, VerifyResponse, header_objection, interface, not_registered, register_hint,
    separator_hint, statement_ref,
};
use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Dispatch, Event};
use registry::Registry;
use repl::ReplConfig;

/// Default wall-clock limit for one Lean run; a proof that elaborates past it
/// is a failed attempt.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Largest candidate accepted, so a runaway generation cannot fill the disk.
const MAX_CANDIDATE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The lake workspace Lean runs in, with Mathlib built (the PutnamBench
    /// `lean4` directory, or a Mathlib checkout). Bound read-only in the
    /// compositions.
    pub workspace: PathBuf,
    /// Writable scratch directory for probe files.
    pub scratch: PathBuf,
    /// Directory of frozen statements, `<problem>.lean`. Defaults to
    /// `workspace/src`.
    pub problems: PathBuf,
    pub endpoint: EndpointName,
    pub timeout: Duration,
    /// The toolchain `lean` binary, run with `LEAN_PATH` set to `lean_path`:
    /// it needs only the olean search path, not lake's manifest resolution.
    pub lean: PathBuf,
    /// The `LEAN_PATH` value (the olean directories, `:`-joined).
    pub lean_path: String,
    /// The Lean REPL executable (`leanprover-community/repl`, e.g. bound
    /// read-only at `/opt/nucleus/repl` in production). When set, one REPL is spawned at
    /// startup with `import Mathlib` done once, and it serves `check`; the REPL
    /// forks the toolchain `lean` through `PATH` and gets the same search path.
    /// Startup is not complete until the import has answered, so a launcher
    /// config that passes `--repl` must give the service a `ready_timeout_ms`
    /// that covers it. Without `--repl`, `check` is refused with a service
    /// error.
    pub repl: Option<PathBuf>,
    /// The resident REPL is replaced by a fresh one before the operation
    /// after this many (`--repl-max-requests`, positive; [`ReplChecker`]).
    pub repl_max_requests: u32,
}

impl Options {
    /// Parses `--workspace <dir> --scratch <dir> --lean <exe> --lean-path <dirs> [--problems <dir>]
    /// [--endpoint <name>] [--timeout-secs <n>] [--repl <exe>] [--repl-max-requests <positive u32>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut workspace, mut scratch, mut problems) = (None, None, None);
        let (mut lean, mut lean_path, mut repl) = (None, None, None);
        let mut endpoint = ENDPOINT.to_owned();
        let mut timeout = DEFAULT_TIMEOUT_SECS;
        let mut repl_max_requests = DEFAULT_REPL_MAX_REQUESTS;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--workspace" => workspace = Some(PathBuf::from(value)),
                "--scratch" => scratch = Some(PathBuf::from(value)),
                "--problems" => problems = Some(PathBuf::from(value)),
                "--lean" => lean = Some(PathBuf::from(value)),
                "--lean-path" => lean_path = Some(value),
                "--repl" => repl = Some(PathBuf::from(value)),
                "--repl-max-requests" => {
                    repl_max_requests = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--repl-max-requests must be a positive u32")?;
                }
                "--endpoint" => endpoint = value,
                "--timeout-secs" => timeout = value.parse().map_err(|_| "--timeout-secs must be a number")?,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let workspace: PathBuf = workspace.ok_or("--workspace is required")?;
        let options = Self {
            problems: problems.unwrap_or_else(|| workspace.join("src")),
            workspace,
            scratch: scratch.ok_or("--scratch is required")?,
            endpoint: EndpointName::new(&endpoint).map_err(|e| e.to_string())?,
            timeout: Duration::from_secs(timeout.max(1)),
            lean: lean.ok_or("--lean is required")?,
            lean_path: lean_path.ok_or("--lean-path is required")?,
            repl,
            repl_max_requests,
        };
        // Refuse an unusable REPL here, at startup, not at the first `check`.
        repl_config(&options)?;
        Ok(options)
    }
}

/// The resident REPL's configuration derived from the options, `None` when
/// `--repl` is absent. The REPL forks `lean` through `PATH`, so the
/// toolchain's `bin` directory (the directory of `--lean`, which must
/// therefore be a path) heads the child's
/// `PATH`, followed by `/usr/bin:/bin`; `LEAN_PATH` is `--lean-path`; `HOME`
/// and the working directory are the scratch directory; each request gets the
/// service timeout.
pub fn repl_config(options: &Options) -> Result<Option<ReplConfig>, String> {
    let Some(exe) = &options.repl else { return Ok(None) };
    let bin = options
        .lean
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("--repl needs --lean to be a path (its directory heads the REPL's PATH)")?;
    Ok(Some(ReplConfig {
        exe: exe.clone(),
        path_env: format!("{}:/usr/bin:/bin", bin.display()),
        lean_path: options.lean_path.clone(),
        home: options.scratch.clone(),
        cwd: options.scratch.clone(),
        request_timeout: options.timeout,
    }))
}

/// Most pristine types [`LeanService`] caches before it starts over.
const MAX_PRISTINE_TYPES: usize = 4096;

/// The provider: frozen files come from the service's own `<problem>.lean`
/// files or from its [`Registry`] (never from a request), pristine types are
/// computed once per frozen file, candidates are verified with the injected
/// runner, and partial proofs are checked on the resident REPL when one was
/// configured.
pub struct LeanService<R: Fn(&str) -> Result<String, RunError>> {
    src: PathBuf,
    run: R,
    /// Pristine types by the exact frozen text, for the service's own files
    /// (a registration holds its own).
    pristine: HashMap<String, String>,
    registry: Registry,
    checker: Option<ReplChecker>,
}

/// The frozen file a call names, resolved on the service's side.
struct Frozen {
    /// The problem, which is also the theorem's name.
    problem: String,
    text: String,
    /// Its pristine type, when a registration holds it.
    pristine: Option<String>,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, why.into())
}

fn service_error(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::ServiceError, why.into())
}

/// What `register` answers when Lean refused a frozen file: Lean's `output`,
/// at most [`MAX_REGISTER_OUTPUT_BYTES`], then a hint for each place where
/// an error stands in text the service added to `frozen`: at the separator
/// `verify` puts at the end of the preamble ([`separator_hint`]), or on a
/// line after the file, where only the report commands stand
/// ([`register_hint`]). The file Lean ran has `frozen`'s lines, so their
/// numbers are the same.
fn lean_refusal(frozen: &str, problem: &str, output: &str) -> String {
    let mut cut = MAX_REGISTER_OUTPUT_BYTES.min(output.len());
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = output[..cut].to_owned();
    let errors: Vec<(usize, usize)> = error_positions(output).collect();
    if let Some((line, column)) = separator_position(frozen)
        && errors.iter().any(|&(l, c)| l == line && c >= column)
    {
        text.push('\n');
        text.push_str(&separator_hint(line));
    }
    let last_frozen_line = frozen.matches('\n').count() + 1;
    if errors.iter().any(|&(line, _)| line > last_frozen_line) {
        text.push('\n');
        text.push_str(&register_hint(problem));
    }
    text
}

/// The line and column of every error Lean reported in `output`
/// (`<file>:<line>:<column>: error…`).
fn error_positions(output: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    output.lines().filter_map(|l| {
        let (position, _) = l.split_once(": error")?;
        let mut parts = position.rsplitn(3, ':');
        let (column, line) = (parts.next()?, parts.next()?);
        Some((line.parse().ok()?, column.parse().ok()?))
    })
}

impl<R: Fn(&str) -> Result<String, RunError>> LeanService<R> {
    /// `problems` is the directory of frozen statements (`<problem>.lean`).
    /// Without [`Self::with_checker`], `check` is refused.
    pub fn new(problems: &Path, run: R) -> Self {
        Self {
            src: problems.to_path_buf(),
            run,
            pristine: HashMap::new(),
            registry: Registry::default(),
            checker: None,
        }
    }

    /// Serve `check` on this resident REPL.
    #[must_use]
    pub fn with_checker(mut self, checker: ReplChecker) -> Self {
        self.checker = Some(checker);
        self
    }

    /// The resident REPL, if one was configured; for the tests.
    #[cfg(test)]
    #[must_use]
    pub fn checker(&self) -> Option<&ReplChecker> {
        self.checker.as_ref()
    }

    fn checker_for(&mut self, what: &str) -> Result<&mut ReplChecker, Error> {
        self.checker.as_mut().ok_or_else(|| service_error(format!("{what} needs a resident REPL (--repl)")))
    }

    /// What a call's resource names ([`statement_ref`]); the benchmark ids
    /// are the service's own `<id>.lean` files.
    fn reference<'a>(&self, resource: &'a ResourcePath) -> Result<StatementRef<'a>, Error> {
        statement_ref(resource, |id| self.src.join(format!("{id}.lean")).is_file())
    }

    /// The service's own `<id>.lean`, `None` when it has none.
    fn own_file(&self, id: &str) -> Result<Option<String>, Error> {
        match std::fs::read_to_string(self.src.join(format!("{id}.lean"))) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(service_error(format!("no frozen statement for {id}: {e}"))),
        }
    }

    /// The frozen file `resource` names: the registration for `[run,
    /// problem, …]`, else the service's own `<problem>.lean`. A run with
    /// neither is [`not_registered`]; nothing a caller sends is ever used.
    fn frozen(&mut self, resource: &ResourcePath) -> Result<Frozen, Error> {
        let (run, problem) = match self.reference(resource)? {
            StatementRef::Run { run, problem } => (Some(run), problem),
            StatementRef::Benchmark { problem } => (None, problem),
        };
        if let Some((text, pristine)) = run.and_then(|run| self.registry.get(run, problem)) {
            let (text, pristine) = (text.to_owned(), Some(pristine.to_owned()));
            return Ok(Frozen { problem: problem.to_owned(), text, pristine });
        }
        match (self.own_file(problem)?, run) {
            (Some(text), _) => Ok(Frozen { problem: problem.to_owned(), text, pristine: None }),
            (None, Some(run)) => Err(not_registered(run, problem)),
            (None, None) => Err(service_error(format!("no frozen statement for {problem}"))),
        }
    }

    /// The pristine type of `frozen`, the theorem `problem`: one Lean run
    /// ([`pristine_type`]), cached by the exact text.
    fn pristine_of(&mut self, problem: &str, frozen: &str) -> Result<String, RunError> {
        if let Some(ty) = self.pristine.get(frozen) {
            return Ok(ty.clone());
        }
        let ty = pristine_type(frozen, problem, &self.run)?;
        if self.pristine.len() >= MAX_PRISTINE_TYPES {
            self.pristine.clear();
        }
        self.pristine.insert(frozen.to_owned(), ty.clone());
        Ok(ty)
    }

    /// Check `request.frozen` the way `verify` will use it and keep it for
    /// `[run, problem]` (the interface's module doc):
    /// [`frozen_source_objection`], or for an id the service has a file for,
    /// that file's exact text; [`header_objection`]; then one Lean run of the
    /// file as `verify` builds it, with `verify`'s report commands appended
    /// ([`pristine_type`]), whose type `verify` then compares against. A
    /// refusal carries Lean's output ([`lean_refusal`]); a run that gave no
    /// verdict ([`RunError::NoVerdict`]) is `ServiceError` and registers
    /// nothing. The same text again is accepted and changes nothing; another
    /// text for a registered `[run, problem]` is `InvalidInput`, so a run's
    /// standard never changes under it.
    pub fn register(
        &mut self,
        resource: &ResourcePath,
        request: &RegisterRequest,
    ) -> Result<RegisterResponse, Error> {
        let (run, problem) = match (self.reference(resource)?, resource.segments().len()) {
            (StatementRef::Run { run, problem }, 2) => (run.to_owned(), problem.to_owned()),
            _ => {
                return Err(invalid(
                    "register takes exactly [run, problem]: a discovery run name and the problem id",
                ));
            }
        };
        let frozen = &request.frozen;
        let accepted = RegisterResponse { accepted: true, stage: None, lean_output: None };
        if let Some((registered, _)) = self.registry.get(&run, &problem) {
            if registered == frozen {
                return Ok(accepted);
            }
            return Err(invalid(format!(
                "{run}/{problem} is registered with another text, and a run's statement never changes"
            )));
        }
        let refused = |stage: RegisterStage, output: String| {
            Ok(RegisterResponse { accepted: false, stage: Some(stage), lean_output: Some(output) })
        };
        let source = match self.own_file(&problem)? {
            Some(own) if own != *frozen => Some(format!(
                "{problem} is a benchmark problem: register its own file's text, byte for byte, or give the \
                 statement another id"
            )),
            _ => frozen_source_objection(&problem, frozen),
        };
        if let Some(why) = source {
            return refused(RegisterStage::Source, why);
        }
        if let Some(why) = header_objection(frozen) {
            return refused(RegisterStage::Header, why);
        }
        let pristine = match self.pristine_of(&problem, frozen) {
            Ok(ty) => ty,
            Err(RunError::Refused(output)) => {
                return refused(RegisterStage::Lean, lean_refusal(frozen, &problem, &output));
            }
            Err(RunError::NoVerdict(why)) => {
                return Err(service_error(format!(
                    "Lean gave no verdict on {run}/{problem}, so nothing is registered; register it again: {}",
                    excerpt(&why)
                )));
            }
        };
        self.registry.insert(run, problem, frozen.clone(), pristine);
        Ok(accepted)
    }

    /// Check `request.candidate`, a partial proof of the frozen file the
    /// resource names, on the resident REPL. `Err(ServiceError)` when no
    /// REPL was configured, when the frozen file has no statement, or when
    /// the REPL could not give a verdict even after one respawn; an
    /// `Invalid` status costs no Lean run.
    pub fn check(&mut self, resource: &ResourcePath, request: &CheckRequest) -> Result<CheckResponse, Error> {
        let (frozen, _, body) = self.prepared("check", resource, &request.candidate)?;
        let body = match body {
            Err(reason) => {
                return Ok(CheckResponse {
                    status: CheckStatus::Invalid,
                    invalid_reason: Some(reason),
                    errors: vec![],
                    sorries: vec![],
                    elapsed_ms: 0,
                });
            }
            Ok(body) => body,
        };
        let problem = frozen.problem;
        let (reply, elapsed) = self
            .checker_for("check")?
            .check(&body)
            .map_err(|why| service_error(format!("lean check of {problem}: {why}")))?;
        Ok(check_response(&reply, elapsed))
    }

    /// The pre-Lean part shared by `check`, `analyze` and `complete`: the
    /// resource, the size limit, the presence of a REPL, the frozen file and
    /// its statement, then [`prepare_check`]. `Err(reason)` in the third
    /// place is an `Invalid` attempt that cost no Lean run.
    #[allow(clippy::type_complexity)]
    fn prepared(
        &mut self,
        what: &str,
        resource: &ResourcePath,
        candidate: &str,
    ) -> Result<(Frozen, String, Result<String, String>), Error> {
        self.reference(resource)?;
        if candidate.len() > MAX_CANDIDATE_BYTES {
            return Err(invalid("candidate exceeds the size limit"));
        }
        self.checker_for(what)?;
        let frozen = self.frozen(resource)?;
        let Some(statement) = extract_statement(&frozen.text) else {
            return Err(service_error(format!(
                "the frozen file for {} has no theorem statement",
                frozen.problem
            )));
        };
        let statement = statement.to_owned();
        let body = match prepare_check(&statement, candidate) {
            Prepared::Invalid(reason) => Err(reason),
            Prepared::Send(body) => Ok(body),
        };
        Ok((frozen, statement, body))
    }

    /// Analyse `request.candidate`, a partial proof, as a sketch on the
    /// resident REPL (see [`sketch`]). `Err(ServiceError)` when no REPL was
    /// configured or none could be spawned; an `Invalid` status costs no Lean
    /// run; a failure during the analysis is the reply's `error`.
    pub fn analyze(
        &mut self,
        resource: &ResourcePath,
        request: &AnalyzeRequest,
    ) -> Result<AnalyzeResponse, Error> {
        let (frozen, statement, body) = self.prepared("analyze", resource, &request.candidate)?;
        let body = match body {
            Err(reason) => {
                return Ok(AnalyzeResponse {
                    status: CheckStatus::Invalid,
                    leaves: vec![],
                    steps: vec![],
                    counts: SketchCounts::default(),
                    hollow: false,
                    false_leaf: false,
                    restates_root: false,
                    statement_type_identical: None,
                    truncated: false,
                    suite: ANALYSIS_SUITE_ID.to_owned(),
                    error: Some(reason),
                    elapsed_ms: 0,
                });
            }
            Ok(body) => body,
        };
        let problem = frozen.problem;
        self.checker_for("analyze")?
            .analyze(&problem, &frozen.text, &statement, &body)
            .map_err(|why| service_error(format!("lean analyze of {problem}: {why}")))
    }

    /// Try to close every remaining `sorry` of `request.candidate` with the
    /// completion suite on the resident REPL (see [`completion`]). Never
    /// calls `verify`; the caller does, with `spliced`.
    pub fn complete(
        &mut self,
        resource: &ResourcePath,
        request: &CompleteRequest,
    ) -> Result<CompleteResponse, Error> {
        let (frozen, _, body) = self.prepared("complete", resource, &request.candidate)?;
        if let Err(reason) = body {
            return Ok(CompleteResponse {
                status: CheckStatus::Invalid,
                closers: vec![],
                all_closed: false,
                spliced: None,
                auto_closable: 0,
                truncated: false,
                suite: COMPLETION_SUITE_ID.to_owned(),
                error: Some(reason),
                elapsed_ms: 0,
            });
        }
        let problem = frozen.problem;
        self.checker_for("complete")?
            .complete(&request.candidate)
            .map_err(|why| service_error(format!("lean complete of {problem}: {why}")))
    }

    /// The frozen file the resource names.
    pub fn statement(&mut self, resource: &ResourcePath) -> Result<StatementResponse, Error> {
        let frozen = self.frozen(resource)?;
        Ok(StatementResponse { problem: frozen.problem, statement: frozen.text })
    }

    /// Verify `request.candidate` against the frozen file the resource names.
    pub fn verify(
        &mut self,
        resource: &ResourcePath,
        request: &VerifyRequest,
    ) -> Result<VerifyResponse, Error> {
        self.reference(resource)?;
        if request.candidate.len() > MAX_CANDIDATE_BYTES {
            return Err(invalid("candidate exceeds the size limit"));
        }
        let frozen = self.frozen(resource)?;
        let pristine = match frozen.pristine {
            Some(ty) => ty,
            None => self.pristine_of(&frozen.problem, &frozen.text).map_err(|e| {
                service_error(format!("pristine type for {}: {}", frozen.problem, excerpt(e.text())))
            })?,
        };
        Ok(match verify_text(&frozen.text, &pristine, &request.candidate, &frozen.problem, &self.run) {
            Ok(v) => VerifyResponse { accepted: true, rejection: None, detail: None, axioms: v.axioms },
            Err(r) => {
                let (kind, detail) = describe(&r);
                VerifyResponse {
                    accepted: false,
                    rejection: Some(kind.to_owned()),
                    detail: Some(detail),
                    axioms: vec![],
                }
            }
        })
    }
}

/// Serves one dispatch. Public so the loop is testable with a stubbed runner.
/// Every request is the interface's own type, which refuses any field it
/// does not list, `frozen` included.
pub fn serve<R: Fn(&str) -> Result<String, RunError>>(
    service: &mut LeanService<R>,
    action: &str,
    resource: &ResourcePath,
    payload: &Payload,
) -> Result<Payload, Error> {
    match action {
        ACTION_STATEMENT => Payload::encode(&service.statement(resource)?),
        ACTION_CHECK => Payload::encode(&service.check(resource, &payload.decode()?)?),
        ACTION_ANALYZE => Payload::encode(&service.analyze(resource, &payload.decode()?)?),
        ACTION_COMPLETE => Payload::encode(&service.complete(resource, &payload.decode()?)?),
        ACTION_VERIFY => Payload::encode(&service.verify(resource, &payload.decode()?)?),
        ACTION_REGISTER => Payload::encode(&service.register(resource, &payload.decode()?)?),
        ACTION_FORGET => {
            payload.decode::<nucleus_lean_interface::ForgetRequest>()?;
            let [run] = resource.segments() else {
                return Err(invalid("forget takes exactly [run]: a discovery run's key"));
            };
            service.registry.forget(run);
            Payload::encode(&serde_json::json!({}))
        }
        _ => Err(invalid("unknown action")),
    }
}

/// Provides the endpoint and serves verifications until told to terminate.
/// With `--repl`, the resident REPL is spawned and Mathlib imported *before*
/// the endpoint is provided, so a service that says it is ready can check;
/// a REPL that cannot start is a startup failure. Terminating the service
/// drops the checker, which kills the REPL process.
pub fn run(options: &Options) -> Result<(), String> {
    std::fs::create_dir_all(&options.scratch).map_err(|e| format!("scratch: {e}"))?;
    let invoke = Invoke { lean: options.lean.clone(), lean_path: options.lean_path.clone() };
    let runner =
        bounded_lean_runner(options.workspace.clone(), options.scratch.clone(), options.timeout, invoke);
    let mut service = LeanService::new(&options.problems, runner);
    if let Some(config) = repl_config(options)? {
        let checker = ReplChecker::spawn_with_max_requests(config, options.repl_max_requests)
            .map_err(|e| format!("resident REPL: {e}"))?;
        eprintln!("nucleus-lean: resident REPL up, Mathlib env {}", checker.mathlib_env());
        service = service.with_checker(checker);
    }
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    serve_dispatches(&mut service, &mut kernel)
}

/// How long the loop waits for each further event after it served a
/// dispatch. The SDK hands frames over one at a time, and the one after the
/// frame it holds takes a moment to arrive: without the wait the loop would
/// stop there and run a dispatch whose cancellation comes right behind it.
/// The caller just served already has its reply.
const DRAIN_WAIT: Duration = Duration::from_millis(20);

/// Serves the kernel's dispatches one at a time, oldest first, until told to
/// terminate. The kernel sends a dispatch as soon as it admits it, so while
/// Lean runs (up to `--timeout-secs`) later ones wait in the channel, with
/// the kernel's cancellations of those whose callers stopped, exited or ran
/// out of time behind them. After each dispatch the loop takes every event
/// sent meanwhile into its queue ([`DRAIN_WAIT`]): a queued dispatch the
/// kernel cancelled is answered `cancelled`, and one whose deadline has
/// passed by its turn `timeout`, both without running.
fn serve_dispatches<R: Read, W: Write, Run: Fn(&str) -> Result<String, RunError>>(
    service: &mut LeanService<Run>,
    kernel: &mut Connection<R, W>,
) -> Result<(), String> {
    let mut queue = VecDeque::new();
    loop {
        let Some((dispatch, deadline)) = queue.pop_front() else {
            let event = kernel.next_event().map_err(|e| e.to_string())?;
            if !take_event(kernel, &mut queue, event)? {
                return Ok(());
            }
            continue;
        };
        if Instant::now() >= deadline {
            let late = Error::new(ErrorCode::Timeout, "the deadline passed before the call was served");
            kernel.reply(dispatch.id, Err(late)).map_err(|e| e.to_string())?;
            continue;
        }
        let result = serve(service, dispatch.action.as_str(), &dispatch.resource, &dispatch.payload);
        kernel.reply(dispatch.id, result).map_err(|e| e.to_string())?;
        while let Some(event) = kernel.next_event_within(DRAIN_WAIT).map_err(|e| e.to_string())? {
            if !take_event(kernel, &mut queue, event)? {
                return Ok(());
            }
        }
    }
}

/// Takes one kernel event into `queue`: a dispatch, with its deadline; a
/// cancellation of a queued dispatch, answered at once. `false` when the
/// service is to terminate. The deadline counts from when the SDK hands the
/// dispatch over, after any time it waited in the channel, so it is never
/// earlier than the caller's own; the kernel cancels a dispatch whose
/// deadline passed while it waited there, and that cancellation is taken
/// with it.
fn take_event<R: Read, W: Write>(
    kernel: &mut Connection<R, W>,
    queue: &mut VecDeque<(Dispatch, Instant)>,
    event: Event,
) -> Result<bool, String> {
    match event {
        Event::Dispatch(dispatch) => {
            let deadline = Instant::now() + Duration::from_millis(dispatch.remaining_ms);
            queue.push_back((dispatch, deadline));
        }
        Event::CancelDispatch(id) => {
            if let Some(at) = queue.iter().position(|(dispatch, _)| dispatch.id == id) {
                queue.remove(at);
                let cancelled = Error::new(ErrorCode::Cancelled, "cancelled before it was served");
                kernel.reply(id, Err(cancelled)).map_err(|e| e.to_string())?;
            }
        }
        Event::ChildExited(_) | Event::ChildReaped { .. } => {}
        Event::Terminate => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod service_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::verify::stub_output;
    use nucleus_lean_interface::MAX_FROZEN_BYTES;

    const TYPE: &str = "putnam_demo : ∀ (n : Nat), @Eq.{1} Nat n n";

    pub(super) fn workspace_with(problem: &str, frozen: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nucleus_lean_ws_{}_{problem}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{problem}.lean")), frozen).unwrap();
        dir
    }

    pub(super) fn path(segments: &[&str]) -> ResourcePath {
        ResourcePath::new(segments.iter().copied()).unwrap()
    }

    #[test]
    fn options_need_workspace_scratch_and_lean() {
        let base = ["--workspace", "/w", "--scratch", "/s", "--lean", "/l", "--lean-path", "/a:/b"];
        let parse = |extra: &[&str]| Options::parse(base.iter().chain(extra).map(|s| (*s).to_owned()));
        for missing in [0, 2, 4, 6] {
            let partial: Vec<String> = base
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != missing && *i != missing + 1)
                .map(|(_, s)| (*s).to_owned())
                .collect();
            assert!(Options::parse(partial).is_err(), "{} is required", base[missing]);
        }
        let ok = parse(&["--timeout-secs", "7"]).unwrap();
        assert_eq!(ok.timeout, Duration::from_secs(7));
        assert_eq!(ok.problems, PathBuf::from("/w/src"), "defaults to workspace/src");
        assert_eq!(ok.repl, None);
        assert_eq!(ok.lean_path, "/a:/b");
        let custom = parse(&["--problems", "/fixed"]).unwrap();
        assert_eq!(custom.problems, PathBuf::from("/fixed"));
        // The REPL's request budget: positive, the default when absent.
        assert_eq!(ok.repl_max_requests, DEFAULT_REPL_MAX_REQUESTS);
        let budget = |value: &str| parse(&["--repl-max-requests", value]);
        assert_eq!(budget("2").unwrap().repl_max_requests, 2);
        assert_eq!(budget("4294967295").unwrap().repl_max_requests, u32::MAX);
        for value in ["0", "-1", "1.5", "many", "4294967296"] {
            assert!(budget(value).unwrap_err().contains("positive u32"), "{value}");
        }
    }

    #[test]
    fn a_repl_gets_the_toolchain_bin_first_on_its_path() {
        let parse = |a: &[&str]| Options::parse(a.iter().map(|s| (*s).to_owned()));
        // A bare `lean` name has no directory to head the REPL's PATH.
        let bare = parse(&[
            "--workspace",
            "/w",
            "--scratch",
            "/s",
            "--lean",
            "lean",
            "--lean-path",
            "/a",
            "--repl",
            "/repl",
        ])
        .unwrap_err();
        assert!(bare.contains("--lean to be a path"), "{bare}");
        let ok = parse(&[
            "--workspace",
            "/w",
            "--scratch",
            "/s",
            "--timeout-secs",
            "9",
            "--lean",
            "/elan/tc/bin/lean",
            "--lean-path",
            "/a:/b",
            "--repl",
            "/repl",
        ])
        .unwrap();
        assert_eq!(ok.repl, Some(PathBuf::from("/repl")));
        let config = repl_config(&ok).unwrap().unwrap();
        assert_eq!(
            config,
            ReplConfig {
                exe: PathBuf::from("/repl"),
                path_env: "/elan/tc/bin:/usr/bin:/bin".to_owned(),
                lean_path: "/a:/b".to_owned(),
                home: PathBuf::from("/s"),
                cwd: PathBuf::from("/s"),
                request_timeout: Duration::from_secs(9),
            }
        );
        let none =
            parse(&["--workspace", "/w", "--scratch", "/s", "--lean", "/l", "--lean-path", "/a"]).unwrap();
        assert_eq!(repl_config(&none).unwrap(), None);
    }

    #[test]
    fn check_is_refused_without_a_resident_repl_and_never_served_cold() {
        let ws =
            workspace_with("putnam_norepl", "import Mathlib\n\ntheorem putnam_norepl : True :=\nsorry\n");
        let run = |_: &str| -> Result<String, RunError> { panic!("the cold runner must not serve check") };
        let mut service = LeanService::new(&ws, run);
        let request =
            CheckRequest { candidate: "import Mathlib\n\ntheorem putnam_norepl : True :=\nsorry\n".into() };
        let err = service.check(&path(&["putnam_norepl"]), &request).unwrap_err();
        assert_eq!(err.code, ErrorCode::ServiceError);
        assert!(err.message.contains("--repl"), "{}", err.message);
        // Through the dispatcher too, so the action is wired.
        let payload = Payload::encode(&request).unwrap();
        let err = serve(&mut service, ACTION_CHECK, &path(&["putnam_norepl"]), &payload).unwrap_err();
        assert_eq!(err.code, ErrorCode::ServiceError);
        // The v3 procedures likewise: same pre-checks, same refusal.
        for action in [ACTION_ANALYZE, ACTION_COMPLETE] {
            let err = serve(&mut service, action, &path(&["putnam_norepl"]), &payload).unwrap_err();
            assert_eq!(err.code, ErrorCode::ServiceError, "{action}");
            assert!(err.message.contains("--repl"), "{action}: {}", err.message);
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_service_caches_the_pristine_type_and_serves_verdicts() {
        let frozen = "import Mathlib\n\ntheorem putnam_demo (n : Nat) : n = n :=\nsorry\n";
        let ws = workspace_with("putnam_demo", frozen);
        let calls = std::cell::Cell::new(0usize);
        let run = |probe: &str| {
            calls.set(calls.get() + 1);
            Ok(stub_output(probe, &format!("'putnam_demo' does not depend on any axioms\n{TYPE}\n")))
        };
        let mut service = LeanService::new(&ws, run);
        let good = VerifyRequest {
            candidate: "import Mathlib\n\ntheorem putnam_demo (n : Nat) : n = n :=\n rfl\n".into(),
        };
        let r1 = service.verify(&path(&["putnam_demo"]), &good).unwrap();
        assert!(r1.accepted, "{r1:?}");
        let r2 = service.verify(&path(&["putnam_demo"]), &good).unwrap();
        assert!(r2.accepted);
        // Pristine type computed once (1) + two candidate runs (2) = 3 Lean runs, not 4.
        assert_eq!(calls.get(), 3);
        let bad = VerifyRequest { candidate: frozen.to_owned() };
        let r3 = service.verify(&path(&["putnam_demo"]), &bad).unwrap();
        assert_eq!((r3.accepted, r3.rejection.as_deref()), (false, Some("forbidden")));
        let unknown = service.verify(&path(&["putnam_nope"]), &good);
        assert_eq!(unknown.map_err(|e| e.code), Err(ErrorCode::ServiceError));
        let _ = std::fs::remove_dir_all(&ws);
    }

    const RUN: &str = "gen2-heldout";
    const OWN: &str = "import Mathlib\n\ndef shift (n : Nat) : Nat := n + 0\n\n/-- The statement. -/\ntheorem kmp_demo (n : Nat) : shift n = n := by\n  sorry\n";

    fn register<R: Fn(&str) -> Result<String, RunError>>(
        service: &mut LeanService<R>,
        segments: &[&str],
        frozen: &str,
    ) -> Result<RegisterResponse, Error> {
        let payload = Payload::encode(&RegisterRequest { frozen: frozen.to_owned() }).unwrap();
        serve(service, ACTION_REGISTER, &path(segments), &payload)?.decode()
    }

    fn verify_at<R: Fn(&str) -> Result<String, RunError>>(
        service: &mut LeanService<R>,
        segments: &[&str],
        candidate: &str,
    ) -> Result<VerifyResponse, Error> {
        let payload = Payload::encode(&VerifyRequest { candidate: candidate.to_owned() }).unwrap();
        serve(service, ACTION_VERIFY, &path(segments), &payload)?.decode()
    }

    /// A stub Lean that answers every probe with a clean report for the
    /// theorem its last `#check @<name>` names; a probe of `namespace Kmp`
    /// fails the way Lean does when the appended `#print axioms` cannot see
    /// the theorem, one with `bad_tactic` fails on a line of the file, and
    /// one with a command right after `@[simp]` fails where Lean's parser
    /// does, right after the attribute.
    fn stub_lean(probes: &std::cell::RefCell<Vec<String>>) -> impl Fn(&str) -> Result<String, RunError> + '_ {
        |probe: &str| {
            probes.borrow_mut().push(probe.to_owned());
            if probe.contains("namespace Kmp") {
                let line = probe.lines().position(|l| l.starts_with("#print axioms")).unwrap() + 1;
                return Err(RunError::Refused(format!(
                    "lean exited with exit status: 1:\n/scratch/p.lean:{line}:0: error(lean.unknownConstant): \
                     Unknown constant `kmp_ns`"
                )));
            }
            if probe.contains("bad_tactic") {
                return Err(RunError::Refused(
                    "lean exited with exit status: 1:\n/scratch/p.lean:6:2: error: unknown tactic".into(),
                ));
            }
            if let Some(at) = probe.find("\n@[simp] #") {
                let line = probe[..at].matches('\n').count() + 2;
                return Err(RunError::Refused(format!(
                    "lean exited with exit status: 1:\n/scratch/p.lean:{line}:7: error: unexpected token '#print'; \
                     expected 'theorem'"
                )));
            }
            let name = probe.rsplit("#check @").next().and_then(|n| n.split_whitespace().next()).unwrap();
            let ty = TYPE.replace("putnam_demo", name);
            let axioms = if probe.contains("sorry") {
                "depends on axioms: [sorryAx]"
            } else {
                "does not depend on any axioms"
            };
            Ok(stub_output(probe, &format!("'{name}' {axioms}\n{ty}\n")))
        }
    }

    /// `register` refuses a frozen file at the first stage that fails, with
    /// the reason or Lean's own output, registers nothing then, and keeps an
    /// accepted file, and its pristine type, for exactly `[run, problem]`.
    #[test]
    fn register_checks_a_frozen_file_as_verify_uses_it_and_never_changes_it() {
        let benchmark = "import Mathlib\n\ntheorem putnam_bench (n : Nat) : n = n :=\nsorry\n";
        let ws = workspace_with("putnam_bench", benchmark);
        let probes = std::cell::RefCell::new(vec![]);
        let mut service = LeanService::new(&ws, stub_lean(&probes));
        let named = |id: &str| OWN.replace("kmp_demo", id);
        let rows: &[(&str, String, Option<RegisterStage>, &str)] = &[
            (
                "kmp_eval",
                named("kmp_eval").replace("def shift", "#eval 1\ndef shift"),
                Some(RegisterStage::Source),
                "forbidden token",
            ),
            ("kmp_other", OWN.to_owned(), Some(RegisterStage::Source), "named `kmp_other`"),
            (
                "kmp_rfl",
                named("kmp_rfl").replace("  sorry", "  rfl"),
                Some(RegisterStage::Source),
                "proved by `sorry`",
            ),
            (
                "kmp_in",
                named("kmp_in").replace("\n\n/--", "\nopen Nat in\n/--"),
                Some(RegisterStage::Source),
                "ends in `in`",
            ),
            (
                "kmp_big",
                format!("{}{}", named("kmp_big"), "-- pad\n".repeat(MAX_FROZEN_BYTES / 4)),
                Some(RegisterStage::Source),
                "bytes",
            ),
            (
                "putnam_bench",
                benchmark.replace("n = n", "n + 0 = n"),
                Some(RegisterStage::Source),
                "benchmark",
            ),
            (
                "kmp_hdr",
                named("kmp_hdr").replace("import Mathlib", "import Mathlib.Tactic"),
                Some(RegisterStage::Header),
                "import Mathlib",
            ),
            (
                "kmp_ns",
                named("kmp_ns").replace("/--", "namespace Kmp\n/--") + "end Kmp\n",
                Some(RegisterStage::Lean),
                "Unknown constant `kmp_ns`",
            ),
            (
                // verify's separator follows the attribute, so Lean refuses
                // the file register runs, as it would refuse every proof.
                "kmp_attr",
                named("kmp_attr").replace("-/\ntheorem", "-/\n@[simp]\ntheorem"),
                Some(RegisterStage::Lean),
                "unexpected token '#print'",
            ),
            (
                "kmp_bad",
                named("kmp_bad").replace("\n\n/--", "\n\nlemma helper : True := by\n  bad_tactic\n\n/--"),
                Some(RegisterStage::Lean),
                "unknown tactic",
            ),
            ("putnam_bench", benchmark.to_owned(), None, ""),
            ("kmp_demo", OWN.to_owned(), None, ""),
        ];
        for (id, frozen, stage, why) in rows {
            let reply = register(&mut service, &[RUN, id], frozen).unwrap();
            assert_eq!((reply.accepted, reply.stage), (stage.is_none(), *stage), "{id}: {reply:?}");
            let output = reply.lean_output.unwrap_or_default();
            assert!(output.contains(why), "{id}: {output}");
            // A hint names text the service added, so it comes only with an error there.
            assert_eq!(output.contains(&register_hint(id)), *id == "kmp_ns", "{id}: {output}");
            assert_eq!(output.contains(&separator_hint(6)), *id == "kmp_attr", "{id}: {output}");
            if stage.is_some() && *id != "putnam_bench" {
                let err = verify_at(&mut service, &[RUN, id, "t000001"], "x").unwrap_err();
                assert!(nucleus_lean_interface::is_not_registered(&err), "{id}: nothing registered: {err:?}");
            }
        }
        assert!(probes.borrow().iter().all(|p| !p.contains("#eval")), "a source refusal runs no Lean");
        // An accepted file is judged by `verify`, which runs Lean once: the
        // pristine type came from `register`.
        let runs = probes.borrow().len();
        let proof = OWN.replace("  sorry", "  rfl");
        assert!(verify_at(&mut service, &[RUN, "kmp_demo", "t000001"], &proof).unwrap().accepted);
        assert_eq!(probes.borrow().len(), runs + 1);
        // The same text again changes nothing and runs no Lean; another text
        // never replaces it.
        let runs = probes.borrow().len();
        assert!(register(&mut service, &[RUN, "kmp_demo"], OWN).unwrap().accepted);
        assert_eq!(probes.borrow().len(), runs);
        let other = register(&mut service, &[RUN, "kmp_demo"], &OWN.replace("n + 0", "0 + n")).unwrap_err();
        assert_eq!(other.code, ErrorCode::InvalidInput, "{other:?}");
        // Unless the run is forgotten (its project was deleted): a run of the
        // same key then registers its own text.
        let forget = |service: &mut LeanService<_>, segments: &[&str]| {
            serve(service, ACTION_FORGET, &path(segments), &Payload::encode(&serde_json::json!({})).unwrap())
        };
        assert!(forget(&mut service, &[RUN]).is_ok());
        assert!(register(&mut service, &[RUN, "kmp_demo"], &OWN.replace("n + 0", "0 + n")).unwrap().accepted);
        assert!(forget(&mut service, &[RUN, "kmp_demo"]).is_err(), "exactly [run]");
        assert!(forget(&mut service, &["other-run"]).is_ok(), "nothing to forget is done");
        // Exactly `[run, problem]`, and a run is never a benchmark id.
        for segments in [&["kmp_demo"][..], &[RUN, "kmp_demo", "t000001"], &["putnam_bench", "kmp_demo"]] {
            let err = register(&mut service, segments, OWN).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "{segments:?}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Every call on `[run, problem, …]` is judged against the run's
    /// registration: the frozen preamble itself is what Lean elaborates, the
    /// candidate's copy of it only has to match, and nothing a candidate
    /// writes after it can extend it.
    #[test]
    fn a_registered_frozen_file_is_what_verify_checks_against() {
        let benchmark = "import Mathlib\n\ntheorem putnam_bench2 (n : Nat) : n = n :=\nsorry\n";
        let ws = workspace_with("putnam_bench2", benchmark);
        let probes = std::cell::RefCell::new(vec![]);
        let mut service = LeanService::new(&ws, stub_lean(&probes));
        assert!(register(&mut service, &[RUN, "kmp_demo"], OWN).unwrap().accepted);
        // The pristine type comes from the file verify builds, run on the
        // frozen text itself.
        let (checked, _) = verify::checked_source(OWN, OWN).unwrap();
        assert!(probes.borrow()[0].starts_with(&checked), "{}", probes.borrow()[0]);
        let task = [RUN, "kmp_demo", "t000001"];
        let proof = OWN.replace("  sorry", "  rfl");
        let reply = verify_at(&mut service, &task, &proof).unwrap();
        assert!(reply.accepted, "{reply:?}");
        let restated = proof.replace("shift n = n", "True");
        assert_eq!(
            verify_at(&mut service, &task, &restated).unwrap().rejection.as_deref(),
            Some("statement_mismatch")
        );
        // The `pp.all` type names `shift` without its body, so a new body, a
        // line that extends the old one, or the old one shown only to the
        // checker (Lean reads `/--/` as the start of a doc comment) must be
        // refused before Lean runs. Helpers after the preamble, comments and
        // blank lines are fine.
        let hidden = proof.replacen("def shift", "/--/\ndef shift", 1).replacen(
            "\n/-- The",
            "\nlemma x -/ def shift (n : Nat) : Nat := n\n/-- The",
            1,
        );
        let helper = proof.replace("\n/--", "\nopen Nat -- a helper\n\nlemma h : True := trivial\n/--");
        let commented = format!("-- a comment\n{}", helper.replacen("\n\n", "\n", 1));
        for (candidate, rejection) in [
            (proof.replace("n + 0\n", "n\n"), Some("preamble_mismatch")),
            (proof.replace("n + 0\n", "n + 0\n\n  * 1\n"), Some("preamble_mismatch")),
            (hidden, Some("preamble_mismatch")),
            (commented.clone(), None),
        ] {
            let r = verify_at(&mut service, &task, &candidate).unwrap();
            assert_eq!(r.rejection.as_deref(), rejection, "{candidate}");
        }
        // What Lean ran for the last one: the frozen preamble through its
        // last line of code, then a command of the service's own on that
        // line, then the candidate after its copy, whose lines keep their
        // numbers. A `protected` field line there would start a new command
        // instead of adding a field to a structure.
        let probe = probes.borrow().last().cloned().unwrap();
        let after = probe.strip_prefix("import Mathlib\n\ndef shift (n : Nat) : Nat := n + 0 ").unwrap();
        assert!(after.starts_with("#print \"") && !probe.contains("-- a comment"), "{probe}");
        let theorem_line = |text: &str| text.lines().position(|l| l.starts_with("theorem"));
        assert_eq!(theorem_line(&probe), theorem_line(&commented), "{probe}");
        // A run without a registration falls back to the service's own file
        // only; otherwise the call names what is missing.
        let bench = benchmark.replace("sorry", "rfl");
        assert!(verify_at(&mut service, &[RUN, "putnam_bench2", "t1"], &bench).unwrap().accepted);
        let missing = verify_at(&mut service, &["other-run", "kmp_demo", "t1"], &proof).unwrap_err();
        assert!(nucleus_lean_interface::is_not_registered(&missing), "{missing:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Registrations live in memory, at most `MAX_REGISTRATIONS`: the least
    /// recently used one goes first, and a call on it then fails as
    /// `not_registered`, which tells the discovery service to register it
    /// again.
    #[test]
    fn registrations_are_bounded_and_the_least_recently_used_goes_first() {
        let ws =
            workspace_with("putnam_bench3", "import Mathlib\n\ntheorem putnam_bench3 : True :=\nsorry\n");
        let probes = std::cell::RefCell::new(vec![]);
        let mut service = LeanService::new(&ws, stub_lean(&probes));
        let run = |k: usize| format!("run-{k}");
        for k in 0..nucleus_lean_interface::MAX_REGISTRATIONS {
            assert!(register(&mut service, &[&run(k), "kmp_demo"], OWN).unwrap().accepted);
        }
        let statement = |service: &mut LeanService<_>, k: usize| {
            serve(service, ACTION_STATEMENT, &path(&[&run(k), "kmp_demo"]), &Payload::encode(&()).unwrap())
        };
        // Reading `run-0` makes `run-1` the least recently used.
        assert!(statement(&mut service, 0).is_ok());
        let full = nucleus_lean_interface::MAX_REGISTRATIONS;
        assert!(register(&mut service, &[&run(full), "kmp_demo"], OWN).unwrap().accepted);
        assert!(statement(&mut service, 0).is_ok() && statement(&mut service, full).is_ok());
        let gone = statement(&mut service, 1).unwrap_err();
        assert!(nucleus_lean_interface::is_not_registered(&gone), "{gone:?}");
        assert_eq!(probes.borrow().len(), 1, "one text, one Lean run");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// `register` refuses a statement only on Lean's own verdict. A run that
    /// outlives the deadline, cannot start, or is killed by a signal says
    /// nothing about the file, so it is a `service_error` the discovery
    /// service may retry, and nothing is registered. The runner is the real
    /// one; the Lean it runs is a script in the workspace.
    #[test]
    fn register_refuses_a_statement_only_on_leans_own_verdict() {
        let frozen = "import Mathlib\n\ntheorem kmp_run (n : Nat) : n = n :=\nsorry\n";
        let dir = std::env::temp_dir().join(format!("nucleus_lean_runner_{}", std::process::id()));
        for (name, script) in [
            ("verdict", "echo \"$1:3:2: error: unknown tactic\"; exit 1"),
            ("deadline", "exec sleep 30"),
            ("signal", "kill -9 $$"),
            ("spawn", ""),
        ] {
            let ws = dir.join(name);
            std::fs::create_dir_all(&ws).unwrap();
            let lean = ws.join("lean");
            std::fs::write(&lean, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&lean, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            let lean = if name == "spawn" { ws.join("no-such-lean") } else { lean };
            let timeout = Duration::from_millis(if name == "deadline" { 300 } else { 60_000 });
            let invoke = Invoke { lean, lean_path: String::new() };
            let mut service =
                LeanService::new(&ws, bounded_lean_runner(ws.clone(), ws.clone(), timeout, invoke));
            let reply = register(&mut service, &[RUN, "kmp_run"], frozen);
            if name == "verdict" {
                let reply = reply.unwrap();
                assert_eq!(reply.stage, Some(RegisterStage::Lean), "{reply:?}");
                assert!(reply.lean_output.unwrap().contains("unknown tactic"));
            } else {
                assert_eq!(reply.map_err(|e| e.code), Err(ErrorCode::ServiceError), "{name}");
            }
            let err = verify_at(&mut service, &[RUN, "kmp_run", "t000001"], "x").unwrap_err();
            assert!(nucleus_lean_interface::is_not_registered(&err), "{name}: nothing registered: {err:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dispatches that wait while Lean runs are not run once their callers
    /// are gone: one the kernel cancels meanwhile is answered `cancelled`,
    /// and one whose deadline has passed `timeout`, without a Lean run.
    #[test]
    fn a_dispatch_cancelled_or_expired_while_it_waits_is_answered_without_running() {
        use nucleus_protocol::{
            ActionName, DispatchId, FromKernel, ProcessId, ToKernel, read_frame, write_frame,
        };
        let ws = workspace_with("putnam_queue", "import Mathlib\n\ntheorem putnam_queue : True :=\nsorry\n");
        let (started, runs) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let mut service = LeanService::new(&ws, move |probe: &str| {
            started.send(()).unwrap();
            // The first run waits for the test's word; a later one does not get it.
            let _ = released.recv_timeout(Duration::from_secs(5));
            let name = probe.rsplit("#check @").next().and_then(|n| n.split_whitespace().next()).unwrap();
            Ok(stub_output(probe, &format!("'{name}' depends on axioms: [sorryAx]\n{name} : True\n")))
        });
        let register = |id: u64, problem: &str, remaining_ms| FromKernel::Dispatch {
            dispatch: DispatchId(id),
            endpoint: nucleus_protocol::EndpointName::new("lean").unwrap(),
            caller: ProcessId(2),
            action: ActionName::new(ACTION_REGISTER).unwrap(),
            resource: path(&[RUN, problem]),
            payload: Payload::encode(&RegisterRequest { frozen: OWN.replace("kmp_demo", problem) }).unwrap(),
            remaining_ms,
        };
        let (from_kernel, mut kernel_out) = std::io::pipe().unwrap();
        let (mut kernel_in, to_kernel) = std::io::pipe().unwrap();
        write_frame(&mut kernel_out, &FromKernel::Hello { process: ProcessId(1), handles: vec![] }).unwrap();
        let replies = std::thread::scope(|scope| {
            let served = scope.spawn(|| {
                let mut kernel = Connection::open(from_kernel, to_kernel).unwrap();
                serve_dispatches(&mut service, &mut kernel)
            });
            write_frame(&mut kernel_out, &register(1, "kmp_one", 60_000)).unwrap();
            runs.recv().unwrap();
            // Sent while the first one's Lean runs.
            write_frame(&mut kernel_out, &register(2, "kmp_two", 60_000)).unwrap();
            write_frame(&mut kernel_out, &FromKernel::CancelDispatch { dispatch: DispatchId(2) }).unwrap();
            write_frame(&mut kernel_out, &register(3, "kmp_three", 0)).unwrap();
            release.send(()).unwrap();
            let mut replies = std::collections::BTreeMap::new();
            for _ in 0..3 {
                match read_frame(&mut kernel_in).unwrap() {
                    ToKernel::Reply { dispatch, result } => {
                        replies.insert(dispatch.0, result.map_err(|e| e.code))
                    }
                    other => panic!("{other:?}"),
                };
            }
            write_frame(&mut kernel_out, &FromKernel::Terminate).unwrap();
            served.join().unwrap().unwrap();
            replies
        });
        assert!(replies[&1].is_ok(), "{replies:?}");
        assert_eq!(
            (replies[&2].clone().err(), replies[&3].clone().err()),
            (Some(ErrorCode::Cancelled), Some(ErrorCode::Timeout))
        );
        assert_eq!(runs.try_iter().count(), 0, "Lean ran for the first dispatch only");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn serve_rejects_unknown_actions_and_decodes_requests() {
        let ws = workspace_with("putnam_demo2", "import Mathlib\n\ntheorem putnam_demo2 : True :=\nsorry\n");
        let mut service = LeanService::new(&ws, |_: &str| Ok(String::new()));
        let payload = Payload::encode(&VerifyRequest { candidate: "x".into() }).unwrap();
        assert_eq!(
            serve(&mut service, "nope", &path(&["putnam_demo2"]), &payload).map_err(|e| e.code),
            Err(ErrorCode::InvalidInput)
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The interface's structs refuse `frozen`, and only what `serve` decodes
    /// decides what a caller can set.
    #[test]
    fn serve_refuses_a_callers_frozen_text() {
        let frozen = "import Mathlib\n\ntheorem putnam_demo3 : True :=\nsorry\n";
        let ws = workspace_with("putnam_demo3", frozen);
        let mut service = LeanService::new(&ws, |_: &str| Ok(String::new()));
        let forged = Payload::encode(&serde_json::json!({
            "candidate": frozen.replace("sorry", "trivial"),
            "frozen": frozen,
        }))
        .unwrap();
        for action in [ACTION_CHECK, ACTION_VERIFY, ACTION_ANALYZE, ACTION_COMPLETE] {
            let reply = serve(&mut service, action, &path(&["putnam_demo3"]), &forged);
            assert_eq!(reply.map_err(|e| e.code).err(), Some(ErrorCode::InvalidInput), "{action}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }
}
