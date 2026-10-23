//! The `lean` service interface, version 7: verify a candidate proof, check
//! and analyse a partial one, and register the frozen statement of a
//! discovery run.
//!
//! The **resource path names the frozen file** a call is about, and through
//! it the theorem, which is always named after the problem
//! ([`statement_ref`]):
//!
//! - `[problem]` (`["putnam_1962_a1"]`): the service's own read-only
//!   `<problem>.lean`, a benchmark statement.
//! - `[run, problem, …]`: the statement registered for `problem` in the
//!   discovery run `run` (`register`), or, when none is, the service's own
//!   `<problem>.lean`. Segments after the second (a worker's task) only
//!   narrow a capability; the service reads the first two. A run segment is
//!   a discovery run's key (the run's name and a digest of its project and
//!   name), which has no `_`, and never names one of the service's own
//!   `<id>.lean` files.
//!
//! A capability scoped to a prefix or to a set of problems bounds what a
//! caller may ask about; the kernel checks that before the provider is
//! reached. A worker holding `lean verify` on `[run, problem, task]` is
//! therefore judged against its own run's statement and no other. Because a
//! scope is a segment prefix, the run rule is what keeps a caller scoped to
//! benchmark `[a]` from naming `[a, b]` and reaching problem `b`.
//!
//! **No request carries the frozen text.** `check`, `verify`, `analyze` and
//! `complete` take the candidate alone, and a request with any other field,
//! `frozen` included, is refused. The standard a candidate is judged against
//! comes only from the trusted side: the service's own files, or a
//! registration made by the process that holds `register` (the discovery
//! service). A model never sees a `frozen` parameter, and a harness cannot
//! pass one on a model's behalf: every tool schema refuses properties it
//! does not list.
//!
//! | Action | Payload | Result |
//! | --- | --- | --- |
//! | `statement` | `{}` | [`StatementResponse`] |
//! | `check` | [`CheckRequest`] | [`CheckResponse`] |
//! | `analyze` | [`AnalyzeRequest`] | [`AnalyzeResponse`] |
//! | `complete` | [`CompleteRequest`] | [`CompleteResponse`] |
//! | `verify` | [`VerifyRequest`] | [`VerifyResponse`] |
//! | `register` | [`RegisterRequest`] on exactly `[run, problem]` | [`RegisterResponse`] |
//! | `forget` | `{}` on exactly `[run]` | `{}` |
//!
//! `register` (not a tool) checks a frozen file the way `verify` will use it
//! and, when it passes, keeps it for `[run, problem]`:
//!
//! 1. [`frozen_source_objection`] (stage `source`). For an id the service has
//!    a `<problem>.lean` for, the text must be that file's, byte for byte.
//! 2. [`header_objection`]: the imports are exactly `import Mathlib`, the
//!    environment `check`, `analyze` and `complete` elaborate in, so they and
//!    `verify` agree about the same text (stage `header`).
//! 3. One cold Lean run of the file exactly as `verify` builds it for a
//!    candidate (the frozen preamble, `verify`'s separator command at the end
//!    of its last line, then the theorem), followed by `verify`'s own report
//!    commands (a per-run marker, `#print axioms <problem>`, `#check
//!    @<problem>`): a clean exit and a well-formed report after the marker
//!    (stage `lean`). So no file is registered that `verify` could not run.
//!    The type it reports is the pristine type `verify` compares against,
//!    cached by the exact text, so `verify` itself runs Lean once per
//!    candidate.
//!
//! A refusal registers nothing and returns Lean's own output (at most
//! [`MAX_REGISTER_OUTPUT_BYTES`]), followed by a hint when an error stands in
//! text the service added: [`separator_hint`] at the separator,
//! [`register_hint`] on a line after the file. A Lean run that ends without
//! Lean's verdict (the service's deadline, Lean failing to start, a signal)
//! is a `service_error`, not a refusal, and registers nothing: it says
//! nothing about the file, and the call can be made again. Registering the
//! same text again is accepted and changes nothing; another text for a
//! registered `[run, problem]` is refused (`invalid_input`), so a run's
//! standard never changes under it. Registrations live in the service's
//! memory only, at most [`MAX_REGISTRATIONS`], least recently used dropped
//! first, so one can be gone when it is needed: after the service
//! restarted, or while a run waited in discovery's queue behind others. A
//! call naming `[run, problem, …]` with neither a registration nor a
//! `<problem>.lean` then fails with [`not_registered`]'s marked
//! `service_error`, never `invalid_input`, and there is no fallback to text
//! a caller sends. The discovery service registers a run's statements at
//! `start` and `resume`, again when the run begins executing, and again
//! whenever a call on the run fails with that error ([`is_not_registered`]).
//!
//! `forget` (not a tool) drops every registration of `[run]`: the discovery
//! service's, for the runs of a project being deleted, so that a run of the
//! same name in a new project of the same name registers its own statements.
//! It answers `{}`, also when there was none.
//!
//! `check` is the evaluator of a *partial* proof — the task-scoring protocol
//! of the Dream-RSI loop. `sorry` is allowed; the reply lists the remaining
//! `sorry`s with the goals they stand for, plus errors, at most 32 of each. A
//! candidate with
//! no errors and no sorries is only a *candidate* solve: the strict `verify`
//! still decides. Like `verify` it executes model-written Lean, so the
//! provider runs confined.
//!
//! `analyze` and `complete` are the two Lean-decided procedures of
//! task-scoring protocol v3, run on the same resident REPL as `check`:
//!
//! - `analyze` reads a `sorries` candidate as a **sketch**: its open leaves
//!   (the remaining `sorry` goals) and its closed steps (`have`/`obtain` steps
//!   and `·` blocks Lean accepted without `sorry`). For each leaf it decides
//!   whether the leaf is **hollow** — the theorem's own goal follows from the
//!   theorem's own hypotheses plus the leaf's target under the fixed
//!   automation suite [`ANALYSIS_SUITE`] (so a restatement, a
//!   disguised restatement and "one easy piece plus the rest" are hollow);
//!   for each closed step whether it is **trivial** (the suite, or the
//!   library search [`LIBRARY_SEARCH`], closes its statement outright) and
//!   whether the rest of the proof **uses** it (Lean's unused-variable
//!   linter). The counts a score is a pure function of are in
//!   [`SketchCounts`]; every verdict names the tactic that decided it, so a
//!   record is auditable.
//! - `complete` tries to close every remaining `sorry` with the fixed suite
//!   [`COMPLETION_SUITE`], one goal at a time, splicing a tactic into the file
//!   and re-elaborating it; when all close it returns the spliced file, which
//!   the caller submits to `verify` — this interface never calls a solve
//!   itself. Partial closures are reported as `auto_closable`.
//!
//! Both are deterministic: every tactic runs under a fixed `maxHeartbeats`
//! (a Lean-counted budget, not wall-clock), in a fixed order, and the suite
//! and caps are protocol constants — a change is a new protocol version.
//! A wall-clock backstop exists only against a hung REPL; when it fires, or
//! the REPL dies, the reply carries `error` and the affected fields are
//! absent, never a fabricated verdict. Both replies are bounded
//! ([`MAX_ANALYSIS_ITEMS`], [`MAX_ANALYSIS_TEXT_CHARS`]).
//!
//! `statement` returns the frozen formalization of the problem (the file the
//! candidate must reproduce verbatim up to the proof). It is an observation.
//!
//! `verify` is classified as an observation from the caller's point of view:
//! it reports whether the candidate is a solve under the PutnamBench protocol
//! (the checks of `nucleus-lean`'s crate documentation) and does not change
//! the problem. Note that it *executes* the candidate — Lean elaboration runs
//! untrusted, model-written code; the textual ban ([`exec_token`]) refuses
//! what runs code, but that is a deny-list — so the provider must still run
//! confined: no network, the workspace read-only, a bounded scratch
//! directory, a deadline.
//!
//! The verdict and any Lean diagnostics returned are untrusted text as far as
//! the caller's model is concerned. This crate defines data only.

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, ResourceFrom,
    ResourcePath,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

mod rules;

pub use rules::{
    PreambleCopy, check_forbidden_token, exec_token, extract_statement, forbidden_token,
    frozen_source_objection, header_objection, lean_code, preamble_copy, preamble_objection,
    protocol_objection,
};

pub const ENDPOINT: &str = "lean";
pub const ACTION_VERIFY: &str = "verify";
pub const ACTION_STATEMENT: &str = "statement";
pub const ACTION_CHECK: &str = "check";
pub const ACTION_ANALYZE: &str = "analyze";
pub const ACTION_COMPLETE: &str = "complete";
/// Check a frozen file as `verify` will use it and keep it for `[run,
/// problem]` ([`RegisterRequest`], [`RegisterResponse`]; module doc).
pub const ACTION_REGISTER: &str = "register";
pub const ACTION_FORGET: &str = "forget";
pub const VERSION: u32 = 7;
/// Most frozen files the service keeps registered at once; the least
/// recently used is dropped first (module doc).
pub const MAX_REGISTRATIONS: usize = 256;
/// The start of the message of [`not_registered`]'s error, which marks it
/// apart from every other `service_error`.
pub const NOT_REGISTERED: &str = "not_registered:";

/// The error of a call on `[run, problem, …]` when the service holds no
/// registration for it and has no `<problem>.lean` (module doc): a
/// `service_error`, because the statement is missing on the service's side,
/// not wrong in the request, and the discovery service can register it
/// again. Not `unavailable`: only the kernel reports that, and it passes a
/// provider's `unavailable` on as `service_error`, so the marker is what
/// tells the two apart.
#[must_use]
pub fn not_registered(run: &str, problem: &str) -> Error {
    Error::new(
        ErrorCode::ServiceError,
        format!(
            "{NOT_REGISTERED} no statement is registered for {run}/{problem}; the discovery service \
             registers it again"
        ),
    )
}

/// Whether `error`, as it reaches a caller through the kernel, is
/// [`not_registered`]'s.
#[must_use]
pub fn is_not_registered(error: &Error) -> bool {
    error.code == ErrorCode::ServiceError && error.message.starts_with(NOT_REGISTERED)
}

// ---- Task-scoring protocol v3 constants -------------------------------------
//
// The suites run in a fixed order under Lean-counted heartbeat caps, so a
// candidate gets the same verdict on every run, and each record names the
// suite that decided it: changing any constant here is a new suite id.

/// The automation suite, in order, that decides *hollow* (a leaf) and
/// *trivial* (a closed step) in `analyze`. Runs in tactic mode on a proof
/// state; contains no library search because the state's environment
/// contains the theorem itself.
pub const ANALYSIS_SUITE: [&str; 14] = [
    "assumption",
    "rfl",
    "trivial",
    "decide",
    "simp",
    "simp_all",
    "omega",
    "norm_num",
    "ring",
    "linarith",
    "positivity",
    "tauto",
    "aesop",
    "nlinarith",
];
/// The library search that additionally decides *trivial* for a closed step
/// (never for hollow): a step `exact?` proves from Mathlib in one lemma is
/// trivial, so listing one-line Mathlib facts as steps earns nothing.
pub const LIBRARY_SEARCH: &str = "exact?";
/// `maxHeartbeats` for every tactic of the analysis suite.
pub const ANALYSIS_HEARTBEATS: u32 = 50_000;
/// Names the analysis procedure in records; a new suite or cap is a new id.
pub const ANALYSIS_SUITE_ID: &str = "v3-S14+exact?/hb50000";
/// The completion suite, in order, spliced into the file one `sorry` at a
/// time (`tac` in tactic position, `(by tac)` in term position) and
/// re-elaborated under [`COMPLETION_HEARTBEATS`]; an `exact?` closer is
/// replaced by its `Try this` suggestion and re-checked.
pub const COMPLETION_SUITE: [&str; 13] = [
    "rfl",
    "assumption",
    "simp",
    "omega",
    "norm_num",
    "positivity",
    "decide",
    "linarith",
    "nlinarith",
    "ring",
    "simp_all",
    "aesop",
    "exact?",
];
/// `maxHeartbeats` for the whole file during a completion re-check.
pub const COMPLETION_HEARTBEATS: u32 = 100_000;
/// Names the completion procedure in records.
pub const COMPLETION_SUITE_ID: &str = "v3-C13/hb100000";
/// Most leaves, steps or closers one reply lists (the rest are counted, not
/// listed, and the reply says so with `truncated`).
pub const MAX_ANALYSIS_ITEMS: usize = 64;
/// Longest goal, statement or tactic text one item carries.
pub const MAX_ANALYSIS_TEXT_CHARS: usize = 2_000;
/// Longest frozen file `register` accepts, in bytes.
pub const MAX_FROZEN_BYTES: usize = 64 * 1024;

/// Payload of `verify`: the whole candidate `.lean` source. The frozen file is
/// the one the call's resource path names; the theorem name is the problem
/// name. Nothing else is accepted (module doc).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    pub candidate: String,
}

/// Result of `statement`: the frozen `.lean` source of the problem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementResponse {
    pub problem: String,
    pub statement: String,
}

/// Payload of `check`: the whole candidate `.lean` source, `sorry` allowed,
/// against the frozen file the resource path names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    pub candidate: String,
}

/// One Lean error diagnostic. Warnings (including "declaration uses `sorry`")
/// are not reported; `sorries` carries the sorry information instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// 1-based line, 0-based column, as Lean reports them.
    pub line: u64,
    pub column: u64,
    pub message: String,
}

/// One remaining `sorry` and the goal it stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenGoal {
    pub line: u64,
    pub column: u64,
    /// The goal as Lean prints it (hypotheses, `⊢`, target). Untrusted text.
    pub goal: String,
}

/// How a partial proof stands. The score is the harness's business (the
/// task-scoring protocol); this is Lean's verdict, not a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// A banned token (`native_decide`, user `axiom`) or the frozen statement
    /// was not reproduced verbatim: not a valid attempt at this problem.
    Invalid,
    /// Lean reported at least one error.
    Errors,
    /// Elaborates without errors; some `sorry` remains.
    Sorries,
    /// Elaborates without errors and without `sorry`: a candidate solve, to be
    /// confirmed by `verify`.
    Complete,
}

/// Result of `check`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResponse {
    pub status: CheckStatus,
    /// Why the attempt is `Invalid`, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    #[serde(default)]
    pub errors: Vec<Diagnostic>,
    #[serde(default)]
    pub sorries: Vec<OpenGoal>,
    /// Wall time of the Lean check in milliseconds.
    pub elapsed_ms: u64,
}

/// Payload of `analyze`: the whole candidate `.lean` source of a `sorries`
/// node (a candidate that is not `sorries` is re-checked and reported with
/// its status and no sketch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeRequest {
    pub candidate: String,
}

/// Whether an open leaf is the theorem in disguise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafVerdict {
    /// The theorem's goal follows from its own hypotheses plus this target
    /// under [`ANALYSIS_SUITE`]: the leaf restates the theorem (or is
    /// a stronger statement the suite can instantiate).
    Hollow,
    /// No suite tactic derives the theorem from the leaf: not shown hollow.
    /// One-directional — a leaf the suite cannot recognise is still not
    /// evidence of progress.
    NotHollow,
    /// The test could not run: a data-typed leaf (`sorry` for a `≃` or a
    /// witness), a statement that does not re-elaborate, or a REPL failure;
    /// `note` says which.
    Untestable,
}

/// How the hollow test was run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafRoute {
    /// In the leaf's own proof state: sketch-introduced hypotheses cleared
    /// (what `clear` refuses is reverted into the statement), then
    /// `Iff.mp` against the verbatim frozen target.
    Tactic,
    /// The root's binders were substituted or cased, so the closed leaf
    /// statement was re-elaborated as text beside the frozen theorem.
    Text,
}

/// One remaining `sorry`, read as an open leaf of the sketch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Leaf {
    pub line: u64,
    pub column: u64,
    /// The target as Lean prints it (bounded). Untrusted text.
    pub target: String,
    pub verdict: LeafVerdict,
    /// The suite tactic that derived the theorem from the leaf, when hollow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hollow_by: Option<String>,
    /// The target is `False`/`⊥` after stripping: a by-contradiction leaf.
    /// An observation only — the hollow test's vacuity control decides such
    /// a leaf (the theorem follows from `False` for free, so the test is
    /// uninformative and the verdict is [`LeafVerdict::Untestable`]).
    pub false_leaf: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<LeafRoute>,
    /// The leaf's context no longer holds the root's own locals as the root
    /// stated them (substituted, cased or renamed).
    pub root_modified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub elapsed_ms: u64,
}

/// What kind of closed sub-proof a step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Have,
    Obtain,
    /// A `·` block closed without `sorry`. Reported and counted in
    /// `closed_with_bullets`, not in `closed`.
    Bullet,
}

/// One closed step of the sketch: a maximal `have`/`obtain … : T := …` or
/// `·` block whose span contains no `sorry` and that lies inside no other
/// closed step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub kind: StepKind,
    pub line: u64,
    pub column: u64,
    /// The bound name or pattern (`h`, `⟨x, hx⟩`, `this`); none for a bullet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The stated type (bounded); none for a bullet. Untrusted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    /// The [`ANALYSIS_SUITE`] tactic that closes the step's statement from
    /// its context outright, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trivial_by: Option<String>,
    /// The one-lemma proof [`LIBRARY_SEARCH`] found, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_trivial_by: Option<String>,
    /// Lean's unused-variable linter reports every name the step binds: the
    /// closed part of the proof never uses it. `this` and `_` are never
    /// reported and count as used.
    pub unused: bool,
    /// The step's hypothesis is in some open leaf's context (recorded;
    /// never counted — it is what makes fact-listing pay).
    pub in_leaf_context: bool,
    /// Counts: a step that is not trivial, not unused and whose statement
    /// state was obtained. A `have`/`obtain` then counts toward `closed`
    /// (and `closed_with_bullets`); a bullet toward `closed_with_bullets`
    /// only.
    pub counted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub elapsed_ms: u64,
}

/// The counts the v3 score is a pure function of (`closed`, `open`, and
/// whether any leaf is hollow), plus variants that are recorded but not
/// scored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SketchCounts {
    /// Counted `have`/`obtain` steps (non-trivial, used, maximal).
    pub closed: u64,
    /// The same plus counted `·` blocks.
    pub closed_with_bullets: u64,
    /// Non-trivial steps that are used *or* in an open leaf's context.
    pub closed_leaf_used: u64,
    /// Open leaves (one per remaining `sorry`).
    pub open: u64,
    /// Leaves whose hollow test could not run.
    pub untestable: u64,
}

/// Result of `analyze`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalyzeResponse {
    /// The re-check's status; the sketch fields are filled only for
    /// `Sorries`.
    pub status: CheckStatus,
    #[serde(default)]
    pub leaves: Vec<Leaf>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub counts: SketchCounts,
    /// Some leaf is [`LeafVerdict::Hollow`].
    pub hollow: bool,
    /// Some leaf is a `False` leaf.
    pub false_leaf: bool,
    /// Some leaf's target is the frozen target verbatim.
    pub restates_root: bool,
    /// The candidate theorem's fully elaborated type equals the pristine
    /// statement's (`verify`'s `pp.all` check applied to a partial proof);
    /// `None` when it was not decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_type_identical: Option<bool>,
    /// More leaves or steps existed than [`MAX_ANALYSIS_ITEMS`] lists; the
    /// counts are still over all of them.
    #[serde(default)]
    pub truncated: bool,
    /// [`ANALYSIS_SUITE_ID`].
    pub suite: String,
    /// Why the analysis is incomplete (REPL death, wall-clock backstop,
    /// unparsable candidate); the fields it could not fill are absent or
    /// default, and the caller scores the base for the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// Payload of `complete`: the whole candidate `.lean` source of a `sorries`
/// node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRequest {
    pub candidate: String,
}

/// Where a `sorry` stands, which decides how a tactic is spliced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SorryPosition {
    /// A tactic `sorry`: replaced by `tac`.
    Tactic,
    /// A term `sorry` (`:= sorry`, `exact sorry`, `⟨sorry, _⟩`): replaced by
    /// `(by tac)`.
    Term,
    Unknown,
}

/// One tactic tried on one goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TacticAttempt {
    pub tactic: String,
    /// `closed`, `error`, `progress`, `no_progress`, `heartbeat_timeout`,
    /// `closed_but_suggestion_failed`, …
    pub outcome: String,
    pub elapsed_ms: u64,
}

/// One remaining `sorry` and what completion did with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closer {
    pub line: u64,
    pub column: u64,
    pub position: SorryPosition,
    /// The tactic (or `exact?`'s suggestion) that closed it, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tactic: Option<String>,
    #[serde(default)]
    pub attempts: Vec<TacticAttempt>,
    pub elapsed_ms: u64,
}

/// Result of `complete`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteResponse {
    /// The re-check's status; closers exist only for `Sorries`.
    pub status: CheckStatus,
    #[serde(default)]
    pub closers: Vec<Closer>,
    /// Every remaining `sorry` closed.
    pub all_closed: bool,
    /// The candidate with every closer spliced in, when `all_closed`: what
    /// the caller passes to `verify`. Never a solve by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spliced: Option<String>,
    /// How many remaining sorries the suite closed (the observation for
    /// partial closures, which earn nothing).
    pub auto_closable: u64,
    #[serde(default)]
    pub truncated: bool,
    /// [`COMPLETION_SUITE_ID`].
    pub suite: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// Result of `verify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyResponse {
    /// True iff every protocol check passed: this is a solve.
    pub accepted: bool,
    /// Machine-readable rejection kind (`forbidden`, `statement_mismatch`,
    /// `preamble_mismatch`, `elaboration`, `sorry_ax`, `non_standard_axioms`,
    /// `type_mismatch`, `no_type`), absent when accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
    /// Human/model-readable detail: Lean's diagnostics excerpt, the offending
    /// token, the extra axioms. Untrusted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Axioms the accepted proof depends on (a subset of the trusted three).
    #[serde(default)]
    pub axioms: Vec<String>,
}

/// Longest part of Lean's output a [`RegisterResponse`] carries, in bytes;
/// the [`register_hint`] line may follow it.
pub const MAX_REGISTER_OUTPUT_BYTES: usize = 4 * 1024;

/// Payload of `forget`: nothing; the run is the call's resource, `[run]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetRequest {}

/// Payload of `register`: the complete frozen file the run proves, at most
/// [`MAX_FROZEN_BYTES`]. The run and the problem are the call's resource
/// path, exactly `[run, problem]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterRequest {
    pub frozen: String,
}

/// The `register` step that refused a frozen file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisterStage {
    /// [`frozen_source_objection`]: size, a comment or `«` name left open,
    /// forbidden tokens, the theorem's name, a proof other than `sorry`
    /// alone, `sorry` before it, a preamble ending in `in` or one `verify`
    /// cannot read; or a benchmark id whose file has another text.
    Source,
    /// [`header_objection`]: the header is not exactly `import Mathlib`.
    Header,
    /// Lean refused the file as `verify` builds it, with `verify`'s own
    /// commands appended (`#print axioms <id>`, `#check @<id>`), or printed
    /// no report for them.
    Lean,
}

/// Result of `register`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// The file is registered for `[run, problem]`, and `verify` can judge
    /// proofs of it.
    pub accepted: bool,
    /// The step that refused the file; absent when accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<RegisterStage>,
    /// Lean's own output when Lean refused the file (at most
    /// [`MAX_REGISTER_OUTPUT_BYTES`], then [`separator_hint`] or
    /// [`register_hint`] when one applies), else the service's reason. Absent
    /// when accepted. Untrusted text: the file is the caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lean_output: Option<String>,
}

/// The line `register` adds after Lean's output when the error is on a line
/// the service appended. The usual cause is a theorem declared inside a
/// `namespace` or `section`, whose short name the appended commands, placed
/// after the whole file, cannot see.
#[must_use]
pub fn register_hint(problem: &str) -> String {
    format!(
        "verify appends `#print axioms {problem}` and `#check @{problem}` after the whole file; declare \
         {problem} at top level, not inside a namespace or section"
    )
}

/// The line `register` adds after Lean's output when the error is at the
/// command `verify` puts at the end of the preamble, on `line`, the last line
/// of code before the theorem ([`preamble_copy`]). That line must end a
/// command, so an attribute or a modifier standing alone there breaks every
/// proof.
#[must_use]
pub fn separator_hint(line: usize) -> String {
    format!(
        "verify puts a command of its own at the end of line {line}, the last line of code before the \
         theorem, so that line must end a command: drop an attribute or modifier standing there alone \
         (`@[simp]`, `private`, `noncomputable`); the theorem's own line must begin with `theorem`"
    )
}

/// What a call's resource path names (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementRef<'a> {
    /// `[problem]`: the service's own `<problem>.lean`.
    Benchmark { problem: &'a str },
    /// `[run, problem, …]`: the statement registered for `problem` in `run`,
    /// or the service's own `<problem>.lean` when none is.
    Run { run: &'a str, problem: &'a str },
}

impl StatementRef<'_> {
    /// The problem, which is also the theorem's name.
    #[must_use]
    pub fn problem(&self) -> &str {
        match self {
            Self::Benchmark { problem } | Self::Run { problem, .. } => problem,
        }
    }
}

/// Longest run segment, in characters: a discovery run's key, its name (at
/// most 40) and a digest of its project and name.
const MAX_RUN_CHARS: usize = 63;

/// Reads a call's resource path: `[problem]` or `[run, problem, …]`, where a
/// problem is `[A-Za-z0-9_]+` (it is the theorem's name and a file name) and
/// a run is a discovery run's key, `[a-z0-9][a-z0-9-]{0,62}`, for which
/// `is_benchmark` (the service's test for an id it has a `<id>.lean` for) is
/// false. `InvalidInput` for anything else, the empty path included.
/// `register` additionally requires exactly `[run, problem]`.
///
/// A scope is a segment prefix, so a caller scoped to benchmark `[a]` may
/// name `[a, b]`. With no `_` in a run, no PutnamBench id reads as one, and
/// `is_benchmark` refuses any other benchmark id as a run, so that path is
/// refused instead of being judged against, or falling back to, `b.lean`.
pub fn statement_ref(
    resource: &ResourcePath,
    is_benchmark: impl Fn(&str) -> bool,
) -> Result<StatementRef<'_>, Error> {
    let invalid = |why: &str| Err(Error::new(ErrorCode::InvalidInput, why.to_owned()));
    let problem_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let run_ok = |s: &str| {
        let lower = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        s.len() <= MAX_RUN_CHARS
            && s.bytes().next().is_some_and(lower)
            && s.bytes().all(|b| lower(b) || b == b'-')
    };
    match resource.segments() {
        [problem] if problem_ok(problem) => Ok(StatementRef::Benchmark { problem }),
        [run, problem, ..] if run_ok(run) && problem_ok(problem) => {
            if is_benchmark(run) {
                return invalid("a run segment must not name a benchmark problem");
            }
            Ok(StatementRef::Run { run, problem })
        }
        _ => invalid(
            "the resource must be [problem] or [run, problem, ...]: a problem is letters, digits and `_`; a \
             run is a discovery run's key, [a-z0-9][a-z0-9-]{0,62}",
        ),
    }
}

/// The model-facing description of every tool's `problem` argument, which
/// the caller's tool layer splits on `/` into the call's resource path.
const PROBLEM_PARAM: &str = "The problem as your task names it: a benchmark id such as putnam_1962_a1, or \
    <run key>/<problem> for a statement registered for a discovery run";

/// What this service tells a model that is given its tools: what it is, and
/// how to work with Lean and Mathlib through it. The second half is the
/// knowledge an agent needs and should not have to carry in its own prompt:
/// it belongs to the tool's description, so anything holding these tools
/// gets it. It is the provider's text; the kernel passes it through unread
/// with every `ToolInfo` of this endpoint.
pub const DESCRIPTION: &str = "The Lean 4 service: frozen theorem statements (the PutnamBench problems by id, \
    or the statement registered for your discovery run, named `<run>/<problem>`: a complete file, `import \
    Mathlib` and one theorem named after the problem whose proof is `sorry`), a checker for \
    partial proofs (`check`: `sorry` allowed, errors and open goals reported, at most 32 of each) and the verifier \
    (`verify`: the official protocol — no `sorry`, `axiom` or `native_decide`, the frozen preamble and \
    theorem statement kept unchanged, clean elaboration, no axioms beyond `propext`, `Classical.choice` and \
    `Quot.sound`; `accepted: true` is a solve). Both take the complete `.lean` source as `candidate` and the \
    problem as `problem`; `check` runs on a resident Lean with Mathlib, in seconds, and `verify` starts \
    Lean afresh, which takes longer. The frozen file itself is \
    never an argument: the service holds it, and judges every candidate against its own copy.\n\n\
    A frozen file is a preamble (imports, `open`s, the definitions the statement uses), the theorem \
    statement, and `sorry`. Your file must begin with the preamble's lines unchanged (blank lines and \
    comments aside) and contain the statement through its final `:=` byte for byte; helper lemmas go \
    between the two, the proof after. Retyping or reflowing either is what breaks the rule, and a rejected \
    candidate says so and quotes the line back.\n\n\
    Mathlib names are systematic: the additive twin of `Subgroup.dense_or_cyclic` is \
    `AddSubgroup.dense_or_cyclic`, `_of_` introduces hypotheses, a prime marks a variant.";

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: DESCRIPTION.to_owned(),
        actions: BTreeMap::from([
            (
                ActionName::new(ACTION_STATEMENT)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Return the frozen Lean 4 formalization of a problem: the file whose theorem \
                     statement a proof must reproduce verbatim.",
                    json!({"type": "object",
                        "properties": {"problem": {"type": "string", "description": PROBLEM_PARAM}},
                        "required": ["problem"],
                        "additionalProperties": false}),
                    ResourceFrom::Segments { param: "problem".to_owned() },
                ),
            ),
            (
                ActionName::new(ACTION_CHECK)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Check a partial Lean 4 proof of a problem: `sorry` is allowed. Returns the \
                     status, the errors, and the remaining sorries with the goals they stand for (at most 32 \
                     of each). Not a solve \
                     verdict; use verify for that.",
                    json!({"type": "object",
                        "properties": {
                            "problem": {"type": "string", "description": PROBLEM_PARAM},
                            "candidate": {"type": "string", "description": "The complete candidate .lean file source; sorry allowed. Leave it out to check the file in your workspace as it stands — that costs you one short call instead of the whole source, and it is the cheapest way to find out where you are. The file in your workspace changes only when verify accepts a candidate."}
                        },
                        "required": ["problem"],
                        "additionalProperties": false}),
                    ResourceFrom::Segments { param: "problem".to_owned() },
                ),
            ),
            (
                ActionName::new(ACTION_ANALYZE)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Analyse a partial Lean 4 proof as a sketch: which remaining `sorry` goals are the theorem \
                     in disguise (hollow) and which closed `have` steps are non-trivial and used. Deterministic \
                     (fixed tactic suite under fixed heartbeats). Not a score; the harness derives one.",
                    json!({"type": "object",
                        "properties": {
                            "problem": {"type": "string", "description": PROBLEM_PARAM},
                            "candidate": {"type": "string", "description": "The complete candidate .lean file source with sorry"}
                        },
                        "required": ["problem", "candidate"],
                        "additionalProperties": false}),
                    ResourceFrom::Segments { param: "problem".to_owned() },
                ),
            ),
            (
                ActionName::new(ACTION_COMPLETE)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Try to close every remaining `sorry` of a partial Lean 4 proof with a fixed tactic suite. \
                     Returns which goals closed and, when all did, the spliced file to pass to verify. Never a \
                     solve by itself.",
                    json!({"type": "object",
                        "properties": {
                            "problem": {"type": "string", "description": PROBLEM_PARAM},
                            "candidate": {"type": "string", "description": "The complete candidate .lean file source with sorry"}
                        },
                        "required": ["problem", "candidate"],
                        "additionalProperties": false}),
                    ResourceFrom::Segments { param: "problem".to_owned() },
                ),
            ),
            // `register` is idempotent for the same text, and `forget` drops
            // a deleted project's runs: mutations with no inverse, like
            // conversation's `ensure`. Only the discovery service holds
            // either; never a tool.
            (ActionName::new(ACTION_REGISTER)?, ActionDecl::internal(EffectClass::Irreversible)),
            (ActionName::new(ACTION_FORGET)?, ActionDecl::internal(EffectClass::Irreversible)),
            (
                ActionName::new(ACTION_VERIFY)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Check a candidate Lean 4 proof of a problem against the official protocol. \
                 Returns accepted=true only if it is a genuine solve; otherwise the rejection kind and Lean's diagnostics.",
                    json!({"type": "object",
                    "properties": {
                        "problem": {"type": "string", "description": PROBLEM_PARAM},
                        "candidate": {"type": "string", "description": "The complete candidate .lean file source. Leave it out to verify the file in your workspace as it stands."}
                    },
                    "required": ["problem"],
                    "additionalProperties": false}),
                    ResourceFrom::Segments { param: "problem".to_owned() },
                ),
            ),
        ]),
    })
}

#[cfg(test)]
mod contract_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn no_request_or_tool_schema_carries_the_frozen_text() {
        for (action, payload) in [
            (ACTION_CHECK, json!({"candidate": "x"})),
            (ACTION_VERIFY, json!({"candidate": "x"})),
            (ACTION_ANALYZE, json!({"candidate": "x"})),
            (ACTION_COMPLETE, json!({"candidate": "x"})),
        ] {
            let mut forged = payload.clone();
            forged["frozen"] = json!("theorem t : False := sorry");
            let decode = |value: serde_json::Value| -> bool {
                match action {
                    ACTION_CHECK => serde_json::from_value::<CheckRequest>(value).is_ok(),
                    ACTION_VERIFY => serde_json::from_value::<VerifyRequest>(value).is_ok(),
                    ACTION_ANALYZE => serde_json::from_value::<AnalyzeRequest>(value).is_ok(),
                    _ => serde_json::from_value::<CompleteRequest>(value).is_ok(),
                }
            };
            assert!(decode(payload), "{action}");
            assert!(!decode(forged), "{action} must refuse a caller's frozen text");
        }
        let decl = interface(EndpointName::new(ENDPOINT).unwrap()).unwrap();
        assert_eq!(decl.version, 7);
        for (name, action) in &decl.actions {
            if let Some(tool) = &action.tool {
                assert!(tool.params["properties"].get("frozen").is_none(), "{name} offers `frozen`");
                assert_eq!(tool.params["properties"]["problem"]["type"], "string", "{name}");
                // A tool layer passes a model's extra arguments through, so
                // the schema itself must refuse them.
                assert_eq!(tool.params["additionalProperties"], false, "{name}");
            }
        }
        for internal in [ACTION_REGISTER, ACTION_FORGET] {
            let action = &decl.actions[&ActionName::new(internal).unwrap()];
            assert!(action.tool.is_none(), "{internal} is never offered to a model");
            assert_ne!(action.effect, EffectClass::Observation);
        }
        assert!(!decl.description.contains("frozen`:") && !DESCRIPTION.contains("`frozen`"));
    }

    #[test]
    fn register_is_strict_and_its_reply_names_the_refusing_stage() {
        let request: RegisterRequest = serde_json::from_value(json!({"frozen": "import Mathlib"})).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), json!({"frozen": "import Mathlib"}));
        assert!(serde_json::from_value::<RegisterRequest>(json!({"frozen": "x", "problem": "p"})).is_err());
        assert!(serde_json::from_value::<RegisterRequest>(json!({})).is_err());
        let accepted = RegisterResponse { accepted: true, stage: None, lean_output: None };
        assert_eq!(serde_json::to_value(&accepted).unwrap(), json!({"accepted": true}));
        for (stage, wire) in [
            (RegisterStage::Source, "source"),
            (RegisterStage::Header, "header"),
            (RegisterStage::Lean, "lean"),
        ] {
            let refused =
                RegisterResponse { accepted: false, stage: Some(stage), lean_output: Some("e".into()) };
            let value = serde_json::to_value(&refused).unwrap();
            assert_eq!(value["stage"], wire);
            assert_eq!(serde_json::from_value::<RegisterResponse>(value).unwrap(), refused);
        }
        let hint = register_hint("demo");
        assert!(hint.contains("#print axioms demo") && hint.contains("#check @demo") && !hint.contains('\n'));
        let hint = separator_hint(3);
        assert!(hint.contains("line 3") && !hint.contains('\n'), "{hint}");
    }

    #[test]
    fn a_resource_names_a_benchmark_problem_or_a_runs_statement() {
        let path = |segments: &[&str]| ResourcePath::new(segments.iter().copied()).unwrap();
        // The service's own files: every PutnamBench id, and one without `_`.
        let is_benchmark = |id: &str| id.starts_with("putnam_") || id == "demo";
        let refusal = |segments: &[&str]| statement_ref(&path(segments), is_benchmark).err().map(|e| e.code);
        let benchmark = path(&["putnam_1962_a1"]);
        assert_eq!(
            statement_ref(&benchmark, is_benchmark).unwrap(),
            StatementRef::Benchmark { problem: "putnam_1962_a1" }
        );
        let run = path(&["gen2-heldout", "my_thm"]);
        assert_eq!(
            statement_ref(&run, is_benchmark).unwrap(),
            StatementRef::Run { run: "gen2-heldout", problem: "my_thm" }
        );
        let task = path(&["gen2-heldout", "my_thm", "t000003"]);
        assert_eq!(statement_ref(&task, is_benchmark).unwrap().problem(), "my_thm", "a task only narrows");
        let long_run = "r".repeat(MAX_RUN_CHARS + 1);
        for bad in [
            &[][..],
            &["my-thm"],
            &["run", "my-thm"],
            &["r!n", "my_thm"],
            &["run", "a.b"],
            // A caller scoped to one benchmark problem naming another.
            &["putnam_1962_a1", "putnam_1963_b2"],
            &["demo", "putnam_1963_b2"],
            &["Gen2", "my_thm"],
            &["-gen2", "my_thm"],
            &[long_run.as_str(), "my_thm"],
        ] {
            assert_eq!(refusal(bad), Some(ErrorCode::InvalidInput), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_registration_is_a_marked_service_error() {
        // A code the kernel passes on from a provider unchanged, so the
        // caller sees exactly this error.
        let missing = not_registered("gen2-heldout", "my_thm");
        assert_eq!(missing.code, ErrorCode::ServiceError);
        assert!(is_not_registered(&missing) && missing.message.contains("gen2-heldout/my_thm"));
        for other in [
            Error::new(ErrorCode::InvalidInput, missing.message.clone()),
            Error::new(ErrorCode::ServiceError, "the REPL died"),
        ] {
            assert!(!is_not_registered(&other), "{other:?}");
        }
    }
}
