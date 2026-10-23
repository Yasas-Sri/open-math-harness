//! The discovery harness: Dream-RSI's policy-guided tree search, applied
//! to theorem proving in Lean — on any statement a task supplies, with
//! PutnamBench as the benchmark that measures it — as a **resident service**
//! that provides two endpoints: `discovery`, through which a run is started,
//! watched, stopped and listed, and `explore`, through which its workers get
//! their tasks and hand back candidates.
//!
//! One rollout is a **discovery tree** `T` rooted at the initial workspace;
//! the single atomic operation is `CONTINUE(v)`: resume node `v`'s workspace,
//! generate and evaluate exactly one child; the **exploration policy** `π`
//! sees the revealed tree and its legal actions — the root plus the leaves,
//! at most `W` per batch — and returns a batch or stops; it decides ordering,
//! parallelism and stopping, nothing about what candidate gets generated. The
//! harness runs that loop for up to `K1` decision rounds per problem and
//! records the tree in the run's journal.
//!
//! # One process per run
//!
//! The program runs as the **service** ([`service`]) and, for every run, as
//! that run's own **process** ([`run_process`]), a child of the service
//! started from the same executable in `run` mode with the run's narrowing of
//! the service's authority. Different runs never share a process. The service
//! keeps the registry of runs and never waits on one: it hands each call to
//! the run's process ([`wire`]) and answers `status`, `list`, `problems` and
//! `tree` from the runs' reports and journals. A run's admission, rollouts,
//! evaluations, policy processes, journals, lease and workers live in its
//! process, so a slow Lean call, a hang or a crash of one run reaches no
//! other; the `lean` service itself is still shared.
//!
//! # The interface: tasks and runs
//!
//! Nothing about a task is in the service's options: a **task** — the
//! system-prompt template, the problem set, the exploration policy, the
//! per-problem limits and the concurrency — is the payload of `discovery start`
//! ([`nucleus_discovery_interface::Task`], version 1). The service validates
//! the document against its ceilings (`--max-concurrency`,
//! `--max-usd-per-problem`, `--max-problems-per-task`, `--max-width`), reads
//! `problems_file` and `prompt_template_file` under `--tasks-dir` and
//! `policy` under `--policies-dir` (relative paths, re-checked at access: the
//! resolved file must lie under that directory), parses the template in an
//! environment set up like the prover's (a syntax error refuses the task;
//! only an undefined variable or a runaway render can still fail inside a
//! worker, per attempt), and refuses a tool-loop task whose template has no
//! `context_reset` mode. It then starts the run's process, which resolves
//! every problem id through `lean statement` (a statement the task supplies
//! must pass the `lean` interface's `frozen_source_objection`, and may not
//! reuse a benchmark id for another text) and has the `lean` service register
//! every supplied statement for `[key, problem]` with the run's key
//! ([`wire::key_of`]; [`admit`]: the checks `verify` will make, one cold Lean
//! run each, a refusal quoting Lean, all within the call's time). The first
//! broken rule refuses the whole task with an `InvalidInput` error and
//! nothing is created for it. A run whose directory `<out>/<name>` already
//! exists is refused too, and so — with a `ServiceError`, the task itself
//! being fine — is a task that arrives while `--max-queued-runs` runs are
//! being started, queued or running. An accepted task becomes a **run**: its
//! directory is created, `task.json` holds the normalized document, and
//! `start` answers with how many runs are queued ahead. At most
//! `--max-running-runs` runs (default 1) execute at once, first come first
//! served; within a run, up to `concurrency` problems are explored at once,
//! each as its own [`Rollout`] with its own policy process and its own
//! journal. `status [project, run]` and `list` report what the run's process
//! last reported; `problems [project, run]` and `tree [project, run,
//! problem]` report each problem's rollout from its journal on disk
//! ([`observe`]) — a queued problem as empty, an active one with the rounds
//! its process holds and has not written yet, an ended one as recorded (read
//! once, then remembered) — so a window shows the tree as its journal records
//! it, bounded to the interface's limits. `stop [project, run]` kills the run's
//! process: the kernel stops its workers first, and the process records
//! `terminated` on its active rollouts and leaves its unstarted problems
//! unstarted; the run is `stopped`. A terminate request to the service does
//! the same to every run. A run whose process ends without its last report is
//! `failed`. A restarted service lists every run it finds under `--out` as
//! ended (`history`), so `list` and `resume` still reach them; `resume`
//! continues a run once the kernel has confirmed its last process gone,
//! closing what that process left open first. A tool-loop run whose template
//! has no `context_reset` mode is not resumed: its workers could not start.
//!
//! The kernel does not tell a provider which endpoint a dispatch arrived on,
//! so the service's interfaces' action names are disjoint (checked at
//! startup) and route each call.
//!
//! # The loop, per problem
//!
//! 1. Node 0 is the frozen formalization (the task's own statement, or read
//!    via `lean statement`), evaluated like any other node (below). It is the
//!    root workspace. The rollout first registers it with the `lean` service
//!    for `[key, problem]` (a benchmark problem's too: that one cold Lean run
//!    is where `verify`'s pristine type comes from); a statement Lean refuses
//!    stops the problem as `statement_rejected: <Lean's output>`. No `lean`
//!    call carries frozen text: every one — the harness's on `[key, problem]`
//!    and its workers' on `[key, problem, task]` — names the registration,
//!    so a supplied statement is what Lean checks against and nothing a
//!    worker sends can change it. A call that finds the registration gone
//!    (`not_registered`, after a restart of the `lean` service) registers it
//!    again and is retried once. The root node records the statement for
//!    resume.
//! 2. Rounds `r = 1..=K1`. The continuable nodes are the root plus the leaves;
//!    a non-root node may receive at most `fanout` continuations in total (so
//!    at most that many copies of it in one batch; after the round it is no
//!    longer a leaf), the root is exempt. The policy is given the view with the
//!    continuable nodes sorted by (score desc, depth asc, id asc), the revealed
//!    tree's shape (every node's parent, hence its depth) and an effective
//!    width `min(W, affordable)` where `affordable` is how many more
//!    continuations the problem's model-call budget can fund. The policy is
//!    either the built-in, hand-written [`InitialPolicy`] (generation 0: `W`
//!    copies of the root in round 1, then its rule over the continuable
//!    nodes) or, when the task names a `policy`, a policy *process* (see
//!    below), one per problem. Both are asked through
//!    [`Policy::decide_with_parents`]; whatever they answer, the harness
//!    admits only the legal part ([`ProblemTree::admit`]): at most `w`
//!    entries, continuable nodes only, the fan-out cap.
//! 3. For every node `v` in the batch the harness mints a task id
//!    (`t<seq:06>`, one sequence per run, continued past every saved worker
//!    by a resume) and spawns ONE worker (see "Workers") whose entire
//!    authority is: `model complete` (and `start`, when the run's process can
//!    stream) on the run's model resource, `max_calls =
//!    calls_per_continuation + collection_calls`, with an unbudgeted `next`
//!    and `cancel` beside it when it streams; `explore {assignment, submit}`
//!    on `[key, problem, task]`, plus `{publish_plan, read_plans}` when the
//!    task's `w` is at least 2 (below); and, for the fixed loop, `lean check`
//!    on `[key, problem, task]`, `max_calls = 4·calls + 4`, or, for the tool
//!    loop, `shell exec` on its own workspace and `lean verify` on `[key,
//!    problem, task]`, `max_calls = 2·calls + 2`. None of it is delegable.
//!    The worker learns everything else — the run's prompt template, the
//!    parent's file, Lean's verdict on it, its siblings, the parent's
//!    transcript, and pointers to other finished branches of the problem
//!    ([`research`]) — from `assignment`, and hands back ONE candidate
//!    through `submit`. The `W` workers of a round run concurrently, as do
//!    the rounds of the other rollouts of the run, and they coordinate
//!    through the problem's plan board ([`plans`]): each may publish a short
//!    research proposal and read the proposals published before its own. A
//!    proposal is untrusted text delivered only as a tool result, bound to
//!    its worker's task, and gone when the worker submits, exits or times
//!    out.
//! 4. The round completes when every worker has exited, or when the round's
//!    wall clock runs out: `(calls_per_continuation + collection_calls) ×
//!    --worker-timeout-ms`, so that a worker that neither submits nor exits
//!    (whatever pinned it) cannot hold the rollout, the run and the queue
//!    behind it. At that point every worker of the round still alive is
//!    killed, and a continuation that has not submitted fails as `timed_out`.
//!    A worker that exits without submitting yields no child (a failed
//!    continuation, `exited_without_submitting`; the kernel's exit event
//!    carries no status, so the worker's own `result.json` under the workers
//!    directory says why). A worker that submitted but lingers — one the
//!    kernel still lists among this process's live children once the round is
//!    over — is killed; a worker that already exited on its own is not, even
//!    when its exit event has not been read yet. A solve ends its own round
//!    too: the round's other live workers are killed, and a continuation
//!    among them that had not submitted is recorded as `solved_elsewhere`.
//! 5. The loop stops when a node scores 1.0 (solved), after round `K1`, when
//!    the rollout's spend has reached a cost cap (`token_cap`, `usd_cap`; see
//!    "Cost caps"), when the model-call budget cannot fund another
//!    continuation, when a round produced no children, when the policy returns
//!    an empty batch, when the policy process fails (`policy_failed`; its
//!    error is recorded and the built-in policy is never substituted for it),
//!    or when the run is stopped or its process terminated (`terminated`).
//!    A round in which every continuation failed at the provider is run
//!    again, with the same batch and without asking the policy, after a
//!    doubling backoff, up to `--provider-retries` times
//!    (`provider_unavailable` after that): an outage is not a decision.
//!
//! # Cost caps (`limits.max_tokens_per_problem`, `limits.max_usd_per_problem`)
//!
//! Reported-usage accounting. Cost is known where usage
//! is: each worker reports the `input_tokens` and `output_tokens` of its
//! submission, which the prover copies from the model adapter's usage
//! accounting; the harness sums them per rollout and, between rounds, refuses
//! to plan another once the sum has reached the token cap or the USD cap. The
//! USD figure is not derived here: the provider prices each call and reports
//! it, the worker sums it and passes it on with its submission. Missing cost
//! remains unknown and does not stop exploration. The USD cap compares only
//! reported charges; it cannot bound actual spending when reports are missing.
//! The task's USD cap defaults to the service's
//! `--max-usd-per-problem` and may not exceed it. Checked between rounds, a
//! rollout overshoots a reported-usage cap by at most one round, and that overshoot is itself
//! bounded by the kernel's `max_calls` on the workers' model grants times the
//! prover's output-token limit. A worker that exits without submitting leaves
//! its tokens unknown and uncounted (its calls stay charged in full), so the
//! caps are an accounting bound on reported usage, not a kernel mechanism:
//! the kernel bounds calls, not tokens. Every round record carries the
//! cumulative `tokens_in`, `tokens_out` and, when a provider reported one,
//! `usd`; `status` sums them over the run.
//!
//! # The exploration policy as a process (the task's `policy`)
//!
//! The exploration policy is model-written Python, run as
//! a zero-capability confined process that only sees the tree view and returns
//! a batch. A task's `policy` names such a program under `--policies-dir`,
//! and the harness drives it instead of [`InitialPolicy`], over the line
//! protocol of [`policy::process`] (version 1): a program ending in
//! `.py` runs as `<--policy-interpreter> <program>` (default
//! `/usr/bin/python3`), anything else executes directly; the interpreter path
//! must be absolute, because the process gets an empty environment and so no
//! `PATH`. One [`ProcessPolicy`] is started per problem — one process is one
//! rollout — with the run directory as its working directory, and it is
//! killed when the problem ends. Every decision is one request line and one
//! reply line within `--policy-timeout-ms` (default 5000).
//! `program-files/discovery/policies/pi0.py` is the initial policy as such a program;
//! it makes the same decisions as [`InitialPolicy`], round by round,
//! including the fan-out cap the harness applies afterwards (tested here).
//!
//! **Failure is a stop, not a substitution.** A policy process that exits,
//! times out, or answers anything but one `{"batch": [...]}` line has failed:
//! the rollout stops with reason `policy_failed`, the error — with the tail of
//! the policy's stderr, bounded, untrusted text — is recorded as `policy_error`
//! on the last round record, and the harness does not fall back to the
//! built-in policy. A run with a policy is a run of that policy or an honest
//! failure of it, so the journal never attributes a built-in decision to a
//! policy program. A program that cannot be started at all (a missing
//! interpreter or file) is refused with the task; should it happen later, the
//! run fails.
//!
//! **Confinement, stated honestly.** The policy process is a host child of
//! the run's process, not a kernel process: it has no kernel connection and
//! so holds no capability, and it inherits exactly that process's sandbox.
//! The platform's confinement of the run's process (no network; `/usr`, which
//! is where `python3` comes from, bound read-only; the process's own binds,
//! the output directory writable among them; its namespaces, so the policy
//! dies with it) applies to the policy as a descendant in the same
//! namespaces. What it is *not* is a sandbox narrower than the run's
//! process's: the policy can read the programs and policies directories and
//! read and write the output directory, because the process can, and
//! [`policy::process`] only guarantees that nothing said over the
//! policy's channel becomes authority.
//!
//! **Deploying a new policy** is naming it in the next task: the `policy`
//! field of every round record says which program produced the tree.
//!
//! # Workers
//!
//! Each continuation is started with the kernel's `spawn_executable` from
//! the run process's own namespace (`workers::worker_program`): the executable is
//! `--prover-executable`, the arguments are every `--prover-arg` in order
//! followed by `--out /out --lean-endpoint <name> --explore-endpoint <name>
//! --shell-endpoint <name>` (the workers' shell its run works in) and the
//! run's `--project <name>`; the only bind is the worker's own directory
//! `<out>/<run>.workers/<problem>/<task>`, mounted writable at
//! `/out/<problem>/<task>`, and there is no network. In its own namespace a
//! worker (or anything it left running) can therefore write only its own
//! directory, never a sibling's or a resumed run's new workers'. The
//! kernel checks the executable and the bind against the run process's
//! namespace, and the grants against its handles, before any process exists;
//! a refusal is a failed continuation, recorded as `spawn_refused`.
//!
//! # Task-scoring protocol, version 3
//!
//! The **harness**, never the worker, evaluates a file — the root's frozen
//! statement and every submission alike — through the `lean` service, in
//! this order (`Harness::evaluate`, combined by [`Evaluation::combine`]):
//!
//! 1. `check`. `invalid` scores 0.0; `errors` 0.05, and the node records
//!    `errors_obs` (the error classes, where in the proof body the first
//!    error falls, `repairable`); `complete` goes to `verify` (1.0 accepted,
//!    0.9 otherwise, `verify_rejection` says why). During proof search only a
//!    submission `verify` accepted is recorded; any other is refused and its
//!    task stays open.
//! 2. `sorries`, for a `final_offer` submission: `complete` tries the fixed completion
//!    suite on every remaining goal. When all close, `verify` runs on the
//!    **spliced** file; accepted, the node is a solve: its `file` is the
//!    spliced proof, `score` 1.0, `verified: true`, and `completion` keeps
//!    the closers, the suite id and the worker's `original_file` (the
//!    recorded `check` is the verdict on that original). Rejected, the
//!    rejection is kept on `completion` and the node is scored as a sketch.
//!    The root is never completed: it is the problem, not a candidate — a
//!    theorem the fixed suite proves outright would be the suite's solve,
//!    not a discovery, and would end the tree before its first round; it is
//!    analysed like any node (its own leaf is the theorem: hollow, the base).
//! 3. Otherwise `analyze`: the node records the whole reply as `sketch` and
//!    scores `0.5 + 0.4 · C / (C + L)` over the credit it implies
//!    ([`credit_of`]: `counts.closed`, `counts.open`, `hollow`; no credit
//!    when the analysis reports an error, re-checked the file to another
//!    status, or found the theorem's type not identical to the frozen one).
//!
//! An `analyze` call that fails — a kernel or service error, a reply
//! carrying `error` or re-checking the file to another status — leaves the
//! base 0.5 and `analysis_failed` saying why, and no `sketch` is recorded, so
//! no credit is. A `complete` call that fails is recorded on
//! `completion.error` and the node is scored from its sketch. The score itself is
//! `nucleus_discovery_interface::scoring::score`, and each node record says which version scored it (`scoring`).
//! Nothing a worker says changes a score. `complete` and `analyze` run on the
//! submission path like `check`, each under its own deadline
//! ([`COMPLETE_DEADLINE_MS`], [`ANALYZE_DEADLINE_MS`]), so one evaluation can
//! hold the run's single-threaded process for their sum in the worst case
//! (`check` + `complete` + `verify` + `analyze`).
//!
//! # Outputs: `<out>/<run>/` and `<out>/<run>.workers/`
//!
//! `<out>` here is a run's process's own output directory: its project's,
//! `<--out>/<project>/` in the service's, which reads the journals beneath
//! it with no link followed.
//!
//! `<out>/<run>/` is a journal directory: its only subdirectories are problems.
//! `task.json` is the accepted task, normalized (problem list and template
//! inline, every limit written out). `discovery.jsonl` holds the per-process
//! `{problem, task, process, event}` lines (`spawned`, `exited`,
//! `spawn_refused`, `killed`, `kill_refused` with the kernel's error when a
//! kill was refused — the worker was gone by the time the request arrived —
//! and `workspace_not_cloned` when a child could not start from its parent's
//! workspace). `.owner`, here and in the workers' directory, names the run
//! process that created the directory ([`materialize`]). `.session.lock` is
//! the run's lease, held by its process, and `resume.jsonl` records every
//! resume admitted. The workers' `/out` is the sibling directory
//! `<out>/<run>.workers/` (`<problem>/<task>/` inside, the prover's own
//! records): a run name never contains a `.`, so the two never collide with
//! another run, and the workers' files never land inside the journal. And the
//! journal, version 1 (append-only, `<problem>/`), two JSONL files per
//! problem, each line carrying `"v": 1`:
//!
//! - `nodes.jsonl` — one [`NodeRecord`] per node in creation order: `{v, id,
//!   parent, round, task, file, check, verified, verify_rejection?, score,
//!   scoring, sketch?, completion?, errors_obs?, analysis_failed?, worker:
//!   {process, model_calls, input_tokens, output_tokens} | null,
//!   ms_since_start}`. The root is `id 0,
//!   parent null, round 0, task null, worker null`.
//! - `rounds.jsonl` — one [`RoundRecord`] per decision round: `{v, round,
//!   policy: {kind: "builtin" | "process", program: <path> | null}, batch:
//!   [parent ids], children: [node ids], failed?: [{task, why}], tokens_in,
//!   tokens_out, usd?, reason?, policy_error?}`; `tokens_in`,
//!   `tokens_out` and `usd` are the rollout's cumulative spend
//!   through that round (see "Cost caps"); `policy` names the exploration
//!   policy that produced the tree; `reason` is present on the last record
//!   and says why the loop stopped, and `policy_error` beside it, when the
//!   reason is `policy_failed`, says how the policy process failed. A problem
//!   whose root could not be evaluated gets a single round-0 record with
//!   `reason: "failed: …"`.
//!
//! A run's directories are fresh by construction (a taken name is refused),
//! and a problem's journal is refused when its `nodes.jsonl` already holds a
//! tree, so a second tree is never appended to a first: overlapping ids
//! would corrupt the tree. A rollout the run's process itself cannot carry on
//! (its journal unwritable, a worker it cannot account for) is closed with
//! `reason: "failed: …"` and its spend counted before the run is marked
//! `failed`, so the journal never ends mid-round on the process's account.
//!
//! # Authority and compromise
//!
//! The service holds `kernel provide` for its `discovery` and `explore`
//! endpoints and `kernel create_endpoint` (its report endpoint and each run's
//! endpoint); it spawns each run's process with its delegable `kernel
//! spawn_executable` and narrows to it its delegable `model`, `lean
//! {statement, check, verify, analyze, complete}`, `lean register`, `explore
//! {assignment, submit, publish_plan, read_plans}` — the kernel's `provide`
//! grants no handle on the provided endpoint, and a delegation needs a handle
//! to derive from, so the launcher grants that one too — and, for the tool
//! loop, `shell exec`. A run's process holds each of them on its key, `[key]`
//! (`lean statement` on the root, `spawn_executable` on the prover
//! executable, `model` on the service's model resource, `shell exec` on its
//! run's workspaces) and gives its workers their tasks' narrowing. The
//! service keeps `lean forget` on the root for itself, to drop a deleted
//! project's statements. Missing authority fails at startup with a message
//! naming it.
//!
//! The service holds what only its runs use: of its `model`, `lean`, shell
//! and `explore` capabilities it calls only `lean forget` itself, and holds
//! the rest to give each run's process that run's narrowing. This is the
//! program's own privilege separation, not a broker: the operator grants
//! authority to one program, which splits it between processes running its
//! own code so that runs share no process; each run's process holds strictly
//! less than the service, and the service offers no interface through which
//! anyone could ask for authority. Runs are created at runtime from a task,
//! so a boot grant per run cannot exist.
//!
//! Whoever holds `discovery start` (the headless client, or a chat agent
//! through `sessions`) can start a run that spends model budget up to the
//! ceiling per problem; the task cannot raise the ceiling, name a file
//! outside the tasks and policies directories, or change what a worker may
//! do. A compromised service can run workers with any authority up to its
//! own, spend whatever the model account allows (the per-problem budget and
//! the cost caps here are soft policy fed by workers' untrusted usage
//! reports, calls clamped to their grants), start any executable it can see
//! with a subset of its authority, and write anything under `--out`. A
//! compromised run process can do the same within its run's narrowing, and
//! write anything under its own `--out`, its project's output directory, the
//! other runs' records included: change another run's `task.json`, whose
//! statements are registered again at that run's next resume if Lean no
//! longer holds them, or edit its spend receipts. That is all it can do to
//! another run: it runs no command in that run's workspaces, reaches none of
//! its tasks, calls no process of it (the service keeps the only handle on
//! each run's endpoint) and sends no report for it, since the kernel checks
//! every `[key]` scope and the service takes a report only from the process
//! it spawned for that run; another project's runs are not in its namespace.
//! Nothing caps how many workers it starts short of the kernel's process
//! limit. Neither has network or filesystem beyond its binds. A policy
//! process holds none of this authority — it has no kernel connection — but
//! shares its run process's sandbox, see above.
//!
//! A run's process is single-threaded: evaluating one submission (a Lean
//! run) blocks serving the run's other workers; no other run waits for it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nucleus_discovery_interface::{
    self as discovery, Ceilings, FailureKind, ListRequest, ListResult, ProblemRow, ProblemSet, ProblemState,
    ProblemsResult, RelativePath, RunState, RunStatus, RunSummary, StartResult, Task, TaskShape,
    TemplateSource, TreeResult, scoring,
};
use nucleus_explore_interface::{self as explore, Assignment, Sibling, Submission};
use nucleus_lean_interface::{
    self as lean, AnalyzeRequest, AnalyzeResponse, CheckRequest, CheckResponse, CheckStatus, Closer,
    CompleteRequest, CompleteResponse, VerifyRequest, VerifyResponse,
};
use nucleus_model_interface as model;
use nucleus_protocol::{
    ACTION_PROVIDE, ACTION_SPAWN_EXECUTABLE, ActionName, ChildBind, ChildProgram, EndpointName, Error,
    ErrorCode, Grant, HandleId, HandleInfo, KERNEL_ENDPOINT, Payload, ProcessId, ResourcePath,
};
use nucleus_sdk::{ChannelError, Connection, Dispatch, Event};
use nucleus_shell_interface as shell;
use policy::process::{PolicyProgram, ProcessPolicy};
use policy::{ErrorsObs, NodeId, Observation, Policy, Score, SketchObservation, View};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod observe;
pub mod policy;
use observe::{ProblemJournal, read_problem, summarize, tree_of};

/// Version of the journal's line format (`nodes.jsonl`, `rounds.jsonl`).
pub const JOURNAL_VERSION: u32 = 1;
mod admit;
mod failures;
mod harness;
mod history;
mod options;
pub use options::*;
mod plans;
mod progress;
mod registry;
mod research;
mod resume;
pub mod run_process;
pub mod service;
pub mod wire;
mod workers;
/// The score that means "solved": `verify` accepted the file.
pub const SOLVED: Score = 1.0;
/// Deadline the harness asks for on each of its own `lean` calls; the kernel
/// clamps it to its configured maximum.
const EVAL_DEADLINE_MS: u64 = 300_000;
/// Deadlines of the two analysis calls of protocol v3, above the `lean`
/// service's own budgets for them (180 s for `complete`, 120 s for
/// `analyze`), so an exhausted budget answers with the reply's `error` —
/// recorded as such — rather than a kernel timeout.
pub const COMPLETE_DEADLINE_MS: u64 = 200_000;
pub const ANALYZE_DEADLINE_MS: u64 = 150_000;
/// Length of an excerpt, a sibling's handed to a worker or a tree node's.
const EXCERPT_CHARS: usize = nucleus_discovery_interface::TREE_EXCERPT_CHARS;
/// Files of a run directory (module doc, "Outputs").
pub const TASK_FILE: &str = "task.json";
pub const EVENTS_FILE: &str = "discovery.jsonl";
/// In the run directory and the workers' directory: the endpoint of the run
/// process that created it ([`materialize`]).
pub const OWNER_FILE: &str = ".owner";
/// The two journal files of a problem (module doc, "Outputs").
pub const NODES_FILE: &str = "nodes.jsonl";
pub const ROUNDS_FILE: &str = "rounds.jsonl";
/// Suffix of the workers' directory beside the run directory:
/// `<out>/<run>.workers` (module doc, "Outputs").
pub const WORKERS_SUFFIX: &str = ".workers";
/// Where a worker sees its writable directory.
pub const WORKER_OUT: &str = "/out";
/// The transcript a tool-loop worker leaves in its directory.
pub const WORKER_TRANSCRIPT: &str = "transcript.jsonl";
/// Where a child finds its parent's transcript in its own directory.
pub const WORKER_PARENT_TRANSCRIPT: &str = "parent.jsonl";

// --- Which exploration policy ------------------------------------------------

/// Where a run's exploration policy comes from: compiled in, or a program
/// (module doc, "The exploration policy as a process").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicySource {
    /// [`InitialPolicy`].
    Builtin,
    /// A policy process per problem, speaking [`policy::process`]'s protocol.
    Process {
        /// Absolute path. Ends in `.py`: run as `<interpreter> <program>`;
        /// anything else executes directly.
        program: PathBuf,
        interpreter: PathBuf,
        timeout: Duration,
    },
}

fn is_python(program: &Path) -> bool {
    program.extension().is_some_and(|ext| ext == "py")
}

impl PolicySource {
    /// How to start the policy process, with `cwd` as its working directory;
    /// `None` for the built-in policy. The process gets no environment at all
    /// (Python needs none) and no argument but its own program.
    #[must_use]
    pub fn program(&self, cwd: &Path) -> Option<PolicyProgram> {
        let PolicySource::Process { program, interpreter, timeout } = self else { return None };
        let (executable, args) = if is_python(program) {
            (interpreter.clone(), vec![program.to_string_lossy().into_owned()])
        } else {
            (program.clone(), Vec::new())
        };
        Some(PolicyProgram {
            program: executable,
            args,
            cwd: Some(cwd.to_path_buf()),
            env: vec![],
            timeout: *timeout,
        })
    }

    /// The identity written into every round record.
    #[must_use]
    pub fn record(&self) -> PolicyRecord {
        match self {
            PolicySource::Builtin => PolicyRecord { kind: PolicyKind::Builtin, program: None },
            PolicySource::Process { program, .. } => PolicyRecord {
                kind: PolicyKind::Process,
                program: Some(program.to_string_lossy().into_owned()),
            },
        }
    }

    /// Startup check: the program, and the interpreter it needs, must exist
    /// where this process can see them, i.e. inside its sandbox. A missing one
    /// is a misconfiguration, not something to discover on the first problem.
    pub fn check(&self) -> Result<(), String> {
        let PolicySource::Process { program, interpreter, .. } = self else { return Ok(()) };
        if is_python(program) && !interpreter.is_absolute() {
            return Err("a Python policy requires an absolute --policy-interpreter path on this host".into());
        }
        if !program.is_file() {
            return Err(format!(
                "{}: not a file this harness can see (is it bound into its sandbox?)",
                program.display()
            ));
        }
        if is_python(program) && !interpreter.is_file() {
            return Err(format!(
                "--policy-interpreter {}: not a file this harness can see (the interpreter must be present \
                 in its sandbox)",
                interpreter.display()
            ));
        }
        Ok(())
    }
}

/// `kind` of a [`PolicyRecord`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    Builtin,
    Process,
}

/// Which exploration policy produced a tree, as every round record says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub kind: PolicyKind,
    /// The policy program's path, `null` for the built-in policy.
    pub program: Option<String>,
}

/// The exploration policy of one rollout. A policy process serves exactly one
/// rollout ([`policy::process`]): every problem starts a new one, and
/// it is killed when this value is dropped, at the latest when the problem
/// ends. There is no fallback from a failed process to the built-in policy.
pub enum RolloutPolicy {
    Builtin(InitialPolicy),
    Process(Box<ProcessPolicy>),
}

impl RolloutPolicy {
    /// Start the policy for one rollout; a process policy runs in `cwd`. A
    /// program that cannot be spawned is an error here (a misconfiguration),
    /// not a stop of the rollout.
    pub fn start(source: &PolicySource, cwd: &Path) -> Result<Self, String> {
        match source.program(cwd) {
            None => Ok(RolloutPolicy::Builtin(InitialPolicy)),
            Some(program) => ProcessPolicy::spawn(&program)
                .map(Box::new)
                .map(RolloutPolicy::Process)
                .map_err(|e| format!("cannot start the policy program {}: {e}", program.program.display())),
        }
    }

    /// Why the policy process stopped serving decisions, if it has: `None` for
    /// the built-in policy and for a process that is still serving. The text
    /// quotes the policy's stderr and reply, which are untrusted.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        match self {
            RolloutPolicy::Builtin(_) => None,
            RolloutPolicy::Process(p) => p.last_error().map(ToString::to_string),
        }
    }
}

impl Policy for RolloutPolicy {
    fn decide_with_parents(&mut self, view: &View, parents: &[Option<NodeId>], w: usize) -> Vec<NodeId> {
        match self {
            RolloutPolicy::Builtin(p) => p.decide_with_parents(view, parents, w),
            RolloutPolicy::Process(p) => p.decide_with_parents(view, parents, w),
        }
    }
}

// --- Scoring (task-scoring protocol v3, `nucleus_discovery_interface::scoring`) ---

fn verdict_of(status: CheckStatus) -> scoring::Verdict {
    match status {
        CheckStatus::Complete => scoring::Verdict::Complete,
        CheckStatus::Sorries => scoring::Verdict::Sorries,
        CheckStatus::Errors => scoring::Verdict::Errors,
        CheckStatus::Invalid => scoring::Verdict::Invalid,
    }
}

/// Lean's verdicts on one file and the score they imply, with what protocol
/// v3 recorded beside them (module doc, "Task-scoring protocol").
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub check: CheckResponse,
    /// `Some` only when `verify` was attempted: on the file itself (the check
    /// was `complete`) or on its completion (`spliced` is then the file).
    pub verified: Option<bool>,
    /// The verifier's rejection kind, or `verifier_<code>` when the verifier
    /// call itself failed.
    pub verify_rejection: Option<String>,
    pub score: Score,
    /// The `analyze` reply of a `sorries` node, whole, when the analysis ran
    /// to the end (a reply carrying `error` is not kept: `analysis_failed`
    /// says why instead).
    pub sketch: Option<AnalyzeResponse>,
    /// What `complete` did with a `sorries` node, when the call answered.
    pub completion: Option<CompletionRecord>,
    /// The spliced proof `verify` accepted: the node is a completion solve
    /// and this is its file (the worker's is `completion.original_file`).
    pub spliced: Option<String>,
    /// The error observations of an `errors` node.
    pub errors_obs: Option<scoring::ErrorsObservation>,
    /// Why a `sorries` node scored the base without a sketch: how `analyze`
    /// failed.
    pub analysis_failed: Option<String>,
}

/// What `complete` did with a `sorries` node, as the journal keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionRecord {
    /// The `lean` interface's `COMPLETION_SUITE_ID`; absent when the call
    /// itself failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub suite: String,
    pub all_closed: bool,
    /// Remaining sorries the suite closed (an observation; partial closures
    /// earn nothing).
    pub auto_closable: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closers: Vec<Closer>,
    /// The worker's file, when the node is a completion solve (its `file` is
    /// then the spliced proof `verify` accepted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_file: Option<String>,
    /// Every goal closed but `verify` rejected the spliced file: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_rejection: Option<String>,
    /// The service could not run the completion to the end, or the call
    /// itself failed (`<code>: <message>`), or its reply re-checked the
    /// file to another status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

impl CompletionRecord {
    /// A completion whose call did not answer.
    fn failed(reason: String) -> Self {
        CompletionRecord { error: Some(reason), ..Default::default() }
    }
}

impl From<&CompleteResponse> for CompletionRecord {
    fn from(c: &CompleteResponse) -> Self {
        CompletionRecord {
            suite: c.suite.clone(),
            all_closed: c.all_closed,
            auto_closable: c.auto_closable,
            closers: c.closers.clone(),
            original_file: None,
            verify_rejection: None,
            error: c.error.clone(),
            elapsed_ms: c.elapsed_ms,
        }
    }
}

/// The replies one evaluation gathered after `check`, each `None` when the
/// call was not made (`Err` is the kernel's or the service's refusal).
#[derive(Debug, Default)]
pub struct LeanReplies {
    pub complete: Option<Result<CompleteResponse, Error>>,
    /// On the file itself for a `complete` check; on the spliced file when
    /// the completion closed every goal.
    pub verify: Option<Result<VerifyResponse, Error>>,
    pub analyze: Option<Result<AnalyzeResponse, Error>>,
}

/// The credit a recorded sketch implies: its counted closed steps, its open
/// leaves and whether any leaf is hollow — or none, and the base score, when
/// the analysis found the theorem's type not identical to the frozen one
/// (the sub-proofs are about a different proposition). A sketch is recorded
/// only when the analysis ran to the end ([`Evaluation::combine`]).
#[must_use]
pub fn credit_of(sketch: &AnalyzeResponse) -> Option<scoring::Credit> {
    if sketch.statement_type_identical == Some(false) {
        return None;
    }
    Some(scoring::Credit { closed: sketch.counts.closed, open: sketch.counts.open, hollow: sketch.hollow })
}

/// The proof body's line range in `candidate`, 1-based: from the line of the
/// frozen statement's final `:=` (the statement as the `lean` interface
/// reads it; line 1 when it is not found in the candidate) to the last
/// non-empty line.
#[must_use]
pub fn body_lines(statement: &str, candidate: &str) -> (u64, u64) {
    let start = lean::extract_statement(statement)
        .and_then(|head| candidate.find(head).map(|at| at + head.len()))
        .map_or(1, |end| candidate[..end].matches('\n').count() as u64 + 1);
    let end = candidate
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .last()
        .map_or(start, |(i, _)| i as u64 + 1);
    (start, end.max(start))
}

impl Evaluation {
    /// Combine the `check` verdict with the `verify` outcome (attempted only
    /// when the check is `complete`) and, for a `sorries` node, the sketch
    /// credit the `lean` service's analysis produced (`None`: no analysis,
    /// the base score). Records nothing beyond the score.
    #[must_use]
    pub fn from_verdicts(
        check: CheckResponse,
        verify: Option<Result<VerifyResponse, Error>>,
        credit: Option<scoring::Credit>,
    ) -> Self {
        let (verified, verify_rejection) = match (check.status, verify) {
            (CheckStatus::Complete, Some(Ok(v))) => (Some(v.accepted), v.rejection),
            (CheckStatus::Complete, Some(Err(e))) => {
                (Some(false), Some(format!("verifier_{:?}", e.code).to_lowercase()))
            }
            _ => (None, None),
        };
        let score = scoring::score(verdict_of(check.status), verified, credit);
        Evaluation {
            check,
            verified,
            verify_rejection,
            score,
            sketch: None,
            completion: None,
            spliced: None,
            errors_obs: None,
            analysis_failed: None,
        }
    }

    /// Protocol v3 (module doc): the evaluation of `file`, a candidate for
    /// the problem whose frozen `statement` is given, from its `check` and
    /// the replies gathered after it.
    #[must_use]
    pub fn combine(file: &str, statement: &str, check: CheckResponse, replies: LeanReplies) -> Self {
        let LeanReplies { complete, verify, analyze } = replies;
        match check.status {
            CheckStatus::Complete => Self::from_verdicts(check, verify, None),
            CheckStatus::Invalid => Self::from_verdicts(check, None, None),
            CheckStatus::Errors => {
                let (start, end) = body_lines(statement, file);
                let errors: Vec<(u64, &str)> =
                    check.errors.iter().map(|d| (d.line, d.message.as_str())).collect();
                let errors_obs = Some(scoring::errors_observation(&errors, start, end));
                Evaluation { errors_obs, ..Self::from_verdicts(check, None, None) }
            }
            CheckStatus::Sorries => Self::sketch(file, check, complete, verify, analyze),
        }
    }

    /// A `sorries` node: a completion solve when every goal closed and
    /// `verify` accepted the spliced file; otherwise the credited sketch, or
    /// the base with `analysis_failed` when `analyze` did not run to the end
    /// (`complete`'s own failure is on `completion.error`).
    fn sketch(
        file: &str,
        check: CheckResponse,
        complete: Option<Result<CompleteResponse, Error>>,
        verify: Option<Result<VerifyResponse, Error>>,
        analyze: Option<Result<AnalyzeResponse, Error>>,
    ) -> Self {
        let completion = match complete {
            None => None,
            Some(Err(e)) => Some(CompletionRecord::failed(format!("{:?}: {}", e.code, e.message))),
            Some(Ok(c)) => {
                let mut record = CompletionRecord::from(&c);
                if record.error.is_none() && c.status != CheckStatus::Sorries {
                    record.error = Some(format!("re-checked as {}", observe::status_name(c.status)));
                }
                if let (true, Some(spliced)) = (c.all_closed, c.spliced) {
                    match verify {
                        Some(Ok(v)) if v.accepted => {
                            record.original_file = Some(file.to_owned());
                            return Evaluation {
                                check,
                                verified: Some(true),
                                verify_rejection: None,
                                score: SOLVED,
                                sketch: None,
                                completion: Some(record),
                                spliced: Some(spliced),
                                errors_obs: None,
                                analysis_failed: None,
                            };
                        }
                        Some(Ok(v)) => {
                            record.verify_rejection = v.rejection.or_else(|| Some("rejected".to_owned()));
                        }
                        Some(Err(e)) => {
                            record.verify_rejection = Some(format!("verifier_{:?}", e.code).to_lowercase());
                        }
                        None => record.verify_rejection = Some("verifier_not_called".to_owned()),
                    }
                }
                Some(record)
            }
        };
        let (sketch, analysis_failed) = match analyze {
            None => (None, Some("analyze: not attempted".to_owned())),
            Some(Err(e)) => (None, Some(format!("analyze: {:?}: {}", e.code, e.message))),
            Some(Ok(a)) => {
                if let Some(error) = &a.error {
                    (None, Some(format!("analyze: {error}")))
                } else if a.status != CheckStatus::Sorries {
                    (None, Some(format!("analyze: re-checked as {}", observe::status_name(a.status))))
                } else {
                    (Some(a), None)
                }
            }
        };
        let credit = sketch.as_ref().and_then(credit_of);
        Evaluation { sketch, completion, analysis_failed, ..Self::from_verdicts(check, None, credit) }
    }

    /// The credit the score was computed from, when one was.
    #[cfg(test)]
    #[must_use]
    pub fn credit(&self) -> Option<scoring::Credit> {
        if self.spliced.is_some() {
            return None;
        }
        self.sketch.as_ref().and_then(credit_of)
    }
}

// --- The tree ----------------------------------------------------------------

/// What a worker cost, as it reported it (untrusted; the kernel enforced the
/// real budget).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub process: u64,
    pub model_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Refusal keeps the task open but does not refund the model call or its cost.
const UNCHANGED_SUBMISSION: &str = "No artifact was recorded: this is the parent file, byte for byte. \
    Continue solving within your remaining model-call allowance. During normal proof search, \
    submit only after eliminating every `sorry` and checking the complete proof. \
    An accepted submission ends this attempt; this refusal leaves it open.";

/// One node of the online tree: its parent, its workspace snapshot (the
/// file), Lean's evaluation of it and its score, plus provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub depth: usize,
    pub round: u32,
    pub task: Option<String>,
    pub file: String,
    pub eval: Evaluation,
    pub worker: Option<WorkerRecord>,
    /// How the check's counts moved against the parent's (`None` for the
    /// root).
    pub delta_vs_parent: Option<scoring::DeltaVsParent>,
    /// Children in creation order.
    pub children: Vec<NodeId>,
}

impl TreeNode {
    #[must_use]
    pub fn score(&self) -> Score {
        self.eval.score
    }

    /// What a policy may see of this node beyond its score, from the
    /// recorded values: the verdict, the spend, and (protocol v3) the sketch summary — present
    /// when a sketch was recorded or the analysis failed; `hollow` `None`
    /// without a sketch, the counts as recorded — the error observations,
    /// the move against the parent and the verify rejection.
    #[must_use]
    pub fn observation(&self) -> Observation {
        let e = &self.eval;
        let analysed = e.sketch.is_some() || e.analysis_failed.is_some();
        let sketch = analysed.then(|| SketchObservation {
            hollow: e.sketch.as_ref().map(|s| s.hollow),
            false_leaf: e.sketch.as_ref().is_some_and(|s| s.false_leaf),
            restates_root: e.sketch.as_ref().is_some_and(|s| s.restates_root),
            closed: e.sketch.as_ref().map_or(0, |s| s.counts.closed),
            open: e.sketch.as_ref().map_or(0, |s| s.counts.open),
            auto_closable: e.completion.as_ref().map_or(0, |c| c.auto_closable),
            analysis_failed: e.analysis_failed.is_some(),
        });
        Observation {
            status: observe::status_name(e.check.status).to_owned(),
            errors: e.check.errors.len() as u64,
            sorries: e.check.sorries.len() as u64,
            tokens_in: self.worker.as_ref().map_or(0, |w| w.input_tokens),
            tokens_out: self.worker.as_ref().map_or(0, |w| w.output_tokens),
            sketch,
            errors_obs: e.errors_obs.as_ref().map(|o| ErrorsObs {
                repairable: o.repairable,
                classes: o.classes.clone(),
                first_error_fraction: o.first_error_fraction,
            }),
            delta_vs_parent: self.delta_vs_parent.map(|d| (d.sorries, d.errors)),
            verify_rejection: e.verify_rejection.clone(),
        }
    }
}

/// One problem's discovery tree. Node 0 is the root.
///
/// A node carries its score, its file, Lean's evaluation, the worker's usage,
/// its round and its task; a policy sees the tree through [`policy`]'s
/// `View`, `NodeId` and `Score`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProblemTree {
    nodes: Vec<TreeNode>,
    fanout: usize,
}

impl ProblemTree {
    /// A tree holding only the root: the frozen file and Lean's verdict on it
    /// (a completion solve's file is its spliced proof, here as in
    /// [`ProblemTree::push`]).
    #[must_use]
    pub fn new(root_file: String, root_eval: Evaluation, fanout: usize) -> Self {
        let root = TreeNode {
            id: 0,
            parent: None,
            depth: 0,
            round: 0,
            task: None,
            file: root_eval.spliced.clone().unwrap_or(root_file),
            eval: root_eval,
            worker: None,
            delta_vs_parent: None,
            children: Vec::new(),
        };
        ProblemTree { nodes: vec![root], fanout: fanout.max(1) }
    }

    /// Append a child of `parent`; returns its id, or `None` if `parent` does
    /// not exist (ids come from this tree, so that is a caller bug).
    pub fn push(
        &mut self,
        parent: NodeId,
        round: u32,
        task: String,
        file: String,
        eval: Evaluation,
        worker: Option<WorkerRecord>,
    ) -> Option<NodeId> {
        let (depth, delta_vs_parent) = {
            let p = self.nodes.get(parent)?;
            let moved = |child: usize, parent: usize| child as i64 - parent as i64;
            let delta = scoring::DeltaVsParent {
                sorries: moved(eval.check.sorries.len(), p.eval.check.sorries.len()),
                errors: moved(eval.check.errors.len(), p.eval.check.errors.len()),
            };
            (p.depth + 1, Some(delta))
        };
        let id = self.nodes.len();
        self.nodes.push(TreeNode {
            id,
            parent: Some(parent),
            depth,
            round,
            task: Some(task),
            file: eval.spliced.clone().unwrap_or(file),
            eval,
            worker,
            delta_vs_parent,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        Some(id)
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&TreeNode> {
        self.nodes.get(id)
    }

    #[must_use]
    pub fn nodes(&self) -> &[TreeNode] {
        &self.nodes
    }

    /// The best score in the tree.
    #[must_use]
    pub fn best(&self) -> Score {
        self.nodes.iter().map(TreeNode::score).fold(Score::MIN, Score::max)
    }

    /// `A(T)`: the root plus the leaves.
    #[must_use]
    pub fn is_continuable(&self, v: NodeId) -> bool {
        self.nodes.get(v).is_some_and(|n| v == 0 || n.children.is_empty())
    }

    /// What the policy sees: every node's score, and the continuable nodes
    /// sorted by (score desc, depth asc, id asc).
    #[must_use]
    pub fn view(&self) -> View {
        let revealed = self.nodes.iter().map(|n| (n.id, n.score())).collect();
        // What the policy may see beyond the score: Lean's verdict, the
        // worker's spend and the protocol v3 observations, exactly what the
        // journal records.
        let observations = self.nodes.iter().map(|n| Some(n.observation())).collect();
        let mut continuable: Vec<NodeId> =
            self.nodes.iter().filter(|n| self.is_continuable(n.id)).map(|n| n.id).collect();
        continuable.sort_by(|a, b| {
            let (x, y) = (&self.nodes[*a], &self.nodes[*b]);
            y.score().total_cmp(&x.score()).then(x.depth.cmp(&y.depth)).then(x.id.cmp(&y.id))
        });
        View { revealed, continuable, observations }
    }

    /// The parent of every node, indexed like the `revealed` of
    /// [`ProblemTree::view`] (node order; `None` for the root): the shape a
    /// policy needs to derive depths ([`Policy::decide_with_parents`]).
    #[must_use]
    pub fn parents(&self) -> Vec<Option<NodeId>> {
        self.nodes.iter().map(|n| n.parent).collect()
    }

    /// The legal part of a policy's batch, in order: at most `w` entries, only
    /// continuable nodes, and a non-root node at most `fanout - children`
    /// times (the root any number of times).
    #[must_use]
    pub fn admit(&self, batch: &[NodeId], w: usize) -> Vec<NodeId> {
        let mut admitted = Vec::new();
        let mut copies: BTreeMap<NodeId, usize> = BTreeMap::new();
        for &v in batch {
            if admitted.len() >= w {
                break;
            }
            if !self.is_continuable(v) {
                continue;
            }
            let taken = copies.entry(v).or_insert(0);
            if v != 0 && self.nodes[v].children.len() + *taken >= self.fanout {
                continue;
            }
            *taken += 1;
            admitted.push(v);
        }
        admitted
    }
}

// --- The initial policy -------------------------------------------------------

/// The hand-written exploration policy, generation 0
/// (`program-files/discovery/policies/pi0.py` is the same rule as a policy
/// program, and the two must decide identically). Round 1 (only the root
/// revealed): `W` copies of the root, opening `W` branches. Later, over the
/// continuable nodes ordered by (score desc, depth asc, id asc):
///
/// - a node whose check is `invalid` is never continued. A **hollow** node
///   is: under the tool loop its file may restate the theorem while its
///   workspace and its transcript hold the work that produced it, and a
///   continuation resumes that work where re-rooting throws it away. The
///   score ordering already puts credited sketches first, so hollow leaves
///   are continued only once nothing better is on the frontier;
/// - one slot is reserved for the child with the best repair prospects when
///   one exists: an `errors` leaf whose `errors_obs` says `repairable`, the
///   fewest errors first, then the latest `first_error_fraction`, then the
///   ordering; else the `sorries` leaf with the most credited `closed`
///   steps, ties by the ordering. It goes first in the batch;
/// - the remaining slots follow the ordering, each node once.
///
/// A missing observation means unknown: not hollow, not invalid, not
/// repairable, nothing closed. It holds nothing and sees only the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialPolicy;

impl InitialPolicy {
    /// The rule over `view`, given every revealed node's depth when the
    /// shape is known (`None`: the view's own order, which the harness sorts
    /// the same way).
    fn plan(view: &View, depth: Option<&[usize]>, w: usize) -> Vec<NodeId> {
        if view.revealed.len() <= 1 {
            return vec![0; w];
        }
        let observations = view.observations.iter().map(Option::as_ref).chain(std::iter::repeat(None));
        let nodes: BTreeMap<NodeId, (Score, usize, Option<&Observation>)> = view
            .revealed
            .iter()
            .enumerate()
            .zip(observations)
            .map(|((i, (id, score)), o)| (*id, (*score, depth.and_then(|d| d.get(i)).map_or(0, |d| *d), o)))
            .collect();
        let mut ordered = view.continuable.clone();
        if depth.is_some() {
            ordered.sort_by(|a, b| {
                let key = |v: &NodeId| nodes.get(v).map_or((Score::MIN, usize::MAX), |(s, d, _)| (*s, *d));
                let ((sa, da), (sb, db)) = (key(a), key(b));
                sb.total_cmp(&sa).then(da.cmp(&db)).then(a.cmp(b))
            });
        }
        let observation = |v: NodeId| nodes.get(&v).and_then(|(_, _, o)| *o);
        let eligible: Vec<NodeId> =
            ordered.into_iter().filter(|&v| v == 0 || observation(v).is_none_or(Self::eligible)).collect();
        let reserved = Self::repair_prospect(&eligible, observation);
        let mut batch: Vec<NodeId> = reserved.into_iter().collect();
        batch.extend(eligible.iter().copied().filter(|v| Some(*v) != reserved));
        batch.truncate(w);
        batch
    }

    /// Not `invalid`. Hollowness does not disqualify a node: what a
    /// hollow file says about the *file* is true, but the node also carries
    /// the workspace and transcript of the work behind it.
    fn eligible(o: &Observation) -> bool {
        o.status != "invalid"
    }

    /// The child with the best repair prospects among `eligible` (in order),
    /// if any: see the type's doc. The earliest wins a tie.
    fn repair_prospect<'a>(
        eligible: &[NodeId],
        observation: impl Fn(NodeId) -> Option<&'a Observation>,
    ) -> Option<NodeId> {
        let mut repair: Option<(NodeId, u64, f64)> = None;
        let mut sketch: Option<(NodeId, u64)> = None;
        for &v in eligible {
            if v == 0 {
                continue;
            }
            let Some(o) = observation(v) else { continue };
            match (o.status.as_str(), &o.errors_obs) {
                ("errors", Some(obs)) if obs.repairable => {
                    let fraction = obs.first_error_fraction.unwrap_or(-1.0);
                    if repair.is_none_or(|(_, errors, best)| {
                        o.errors < errors || (o.errors == errors && fraction > best)
                    }) {
                        repair = Some((v, o.errors, fraction));
                    }
                }
                ("sorries", _) => {
                    let closed = o.sketch.as_ref().map_or(0, |s| s.closed);
                    if sketch.is_none_or(|(_, best)| closed > best) {
                        sketch = Some((v, closed));
                    }
                }
                _ => {}
            }
        }
        repair.map(|(v, ..)| v).or(sketch.map(|(v, _)| v))
    }
}

impl Policy for InitialPolicy {
    fn decide_with_parents(&mut self, view: &View, parents: &[Option<NodeId>], w: usize) -> Vec<NodeId> {
        let depth = policy::process::depths(&view.revealed, parents).ok();
        Self::plan(view, depth.as_deref(), w)
    }
}

// --- Task ids ----------------------------------------------------------------

/// Mints task ids `t000001, t000002, …`: one sequence per run, continued past
/// every saved worker by a resume, so a task id names one continuation of the
/// run unambiguously across its problems and executions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskIds {
    next: u64,
}

impl TaskIds {
    pub fn mint(&mut self) -> String {
        self.next += 1;
        format!("t{:06}", self.next)
    }
}

// --- The rollout state machine (pure) -----------------------------------------

/// The bounds of one problem's rollout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub w: usize,
    pub k1: u32,
    pub fanout: usize,
    pub calls_per_continuation: u64,
    /// Zero for the fixed loop; five for a worker with the workspace tool loop.
    pub collection_calls: u64,
    pub model_calls: u64,
    /// Tokens the rollout may spend; 0 = uncapped.
    pub max_tokens: u64,
    /// Reported micro-USD at which to stop; 0 = uncapped. Missing costs do
    /// not stop the rollout, so this does not bound unreported charges.
    pub max_usd_micro: u64,
}

impl Limits {
    /// The cost cap `spend` has reached, if any: tokens first, then USD. Caps
    /// are checked between rounds, so a rollout that reaches one has already
    /// overshot it by at most the last round of reported usage. Missing cost
    /// is unknown, not a reached cap; call and token limits still apply.
    #[must_use]
    pub fn cap_reached(&self, spend: &Spend) -> Option<Cap> {
        if self.max_tokens > 0 && spend.tokens() >= self.max_tokens {
            return Some(Cap::Tokens);
        }
        if self.max_usd_micro > 0 && spend.usd_micro().is_some_and(|usd| usd >= self.max_usd_micro) {
            return Some(Cap::Usd);
        }
        None
    }
}

/// What one problem's rollout has spent so far, from the workers' reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spend {
    /// Model calls charged: the grant of every worker still open or exited
    /// without a report, and the reported usage of every submission.
    pub calls: u64,
    /// Tokens the submissions reported. A worker that exited without
    /// submitting left its tokens unknown and uncounted; its calls stay
    /// charged in full, and the kernel's `max_calls` on its grant bounds what
    /// it could have spent.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// What the providers charged for those calls, in nano-USD, summed from
    /// the submissions as each provider reported it. Zero when none did, which
    /// is not a claim that the calls were free: see `usd_micro`. Nano is the
    /// unit the model interface reports costs in (`Usage::cost_nano_usd`).
    pub cost_nano: u64,
}

impl Spend {
    /// What this spend cost in micro-USD, rounded up so a call is never free
    /// for having been cheap; `None` when no provider reported a cost.
    /// Nothing here derives a figure from token counts.
    #[must_use]
    pub fn usd_micro(&self) -> Option<u64> {
        (self.cost_nano > 0).then(|| self.cost_nano.div_ceil(1_000))
    }

    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.tokens_in.saturating_add(self.tokens_out)
    }

    /// Both spends together, saturating.
    #[must_use]
    pub fn plus(self, other: Spend) -> Spend {
        Spend {
            calls: self.calls.saturating_add(other.calls),
            tokens_in: self.tokens_in.saturating_add(other.tokens_in),
            tokens_out: self.tokens_out.saturating_add(other.tokens_out),
            cost_nano: self.cost_nano.saturating_add(other.cost_nano),
        }
    }
}

/// Which cost cap a rollout reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Tokens,
    Usd,
}

/// Why a rollout stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A node scored [`SOLVED`].
    Solved,
    /// `K1` rounds ran.
    Rounds,
    /// The model-call budget cannot fund another continuation.
    ModelBudget,
    /// The rollout's spend reached its token or USD cap.
    CostCap(Cap),
    /// The policy returned an empty batch.
    PolicyStopped,
    /// The policy process failed (exited, timed out, broke the protocol); the
    /// text is [`RolloutPolicy::failure`], recorded as `policy_error`. Not a
    /// decision, and never replaced by a built-in one.
    PolicyFailed(String),
    /// A round produced no children.
    NoChildren,
    /// Every continuation of a round failed for a provider reason, and the
    /// configured retries (`--provider-retries`) are used up: the problem is
    /// unmeasured, not failed.
    ProviderUnavailable,
    /// The supervisor asked this process to exit.
    Terminated,
    /// The `lean` service refused to register the problem's statement when
    /// its rollout started; the text is Lean's output (at most 4 KiB and a
    /// hint line; untrusted).
    StatementRejected(String),
    /// The problem could not be set up (statement or root evaluation failed).
    Failed(String),
}

impl Stop {
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Stop::Solved => "solved".to_owned(),
            Stop::Rounds => "rounds".to_owned(),
            Stop::ModelBudget => "model_budget".to_owned(),
            Stop::CostCap(Cap::Tokens) => "token_cap".to_owned(),
            Stop::CostCap(Cap::Usd) => "usd_cap".to_owned(),
            Stop::PolicyStopped => "policy_stopped".to_owned(),
            Stop::PolicyFailed(_) => "policy_failed".to_owned(),
            Stop::NoChildren => "no_children".to_owned(),
            Stop::ProviderUnavailable => "provider_unavailable".to_owned(),
            Stop::Terminated => "terminated".to_owned(),
            Stop::StatementRejected(lean) => format!("statement_rejected: {lean}"),
            Stop::Failed(why) => format!("failed: {why}"),
        }
    }

    /// How the policy process failed, when that is why the rollout stopped.
    #[must_use]
    pub fn policy_error(&self) -> Option<&str> {
        match self {
            Stop::PolicyFailed(why) => Some(why),
            _ => None,
        }
    }
}

/// One worker of the current round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Continuation {
    pub parent: NodeId,
    pub process: ProcessId,
    /// This worker has produced at least one node.
    pub submitted: bool,
    pub exited: bool,
    /// What this task has already been charged, so recording its submission
    /// again (receipt reconciliation) adds only what it spent since. A worker
    /// reports its totals, not its deltas.
    pub charged: Spend,
    /// The parent's transcript as the worker sees it, when the harness put
    /// one in the worker's directory.
    pub parent_transcript: Option<String>,
}

/// A continuation that yielded no child: the same record on the journal and
/// on the wire (`discovery tree`).
pub use discovery::Failed;

/// What one round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundLog {
    pub batch: Vec<NodeId>,
    pub children: Vec<NodeId>,
    pub failed: Vec<Failed>,
}

/// The end of a round: its log, and the workers that submitted but have not
/// exited (to be killed; their authority is spent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundEnd {
    pub round: u32,
    pub log: RoundLog,
    pub stragglers: Vec<ProcessId>,
    /// Cumulative spend through this round, and what the providers charged
    /// for it when they said.
    pub spend: Spend,
    pub usd_micro: Option<u64>,
}

/// The outcome of serving one `explore` dispatch.
#[derive(Debug)]
pub enum Served {
    /// Answer the caller with this.
    Reply(Result<Payload, Error>),
    /// A submission was accepted for `task` (answer `{}`), and the harness now
    /// evaluates it.
    Accepted { task: String, parent: NodeId, process: ProcessId, submission: Submission },
}

/// One problem's rollout: the tree, the current round's workers, and the
/// budget. Pure — it never touches the kernel — so the harness's decisions are
/// testable without one.
#[derive(Clone, Debug, PartialEq)]
pub struct Rollout {
    problem: String,
    statement: String,
    /// The run's system-prompt template, handed to every worker verbatim.
    prompt_template: String,
    tree: ProblemTree,
    limits: Limits,
    round: u32,
    /// Rounds retried after a provider failure: they keep their number in
    /// the journal but do not count toward `K1`.
    retried: u32,
    spent: Spend,
    open: BTreeMap<String, Continuation>,
    current: RoundLog,
    /// The live workers' research proposals ([`plans`]), when `w` is at
    /// least 2.
    plans: plans::Plans,
}

/// Why a continuation appears in a round's `failed` list when the harness
/// stopped it: its round-mate solved the problem. Not a failure of the run.
pub const CUT_FOR_SOLVE: &str = "solved_elsewhere";

/// A round that produced nothing because every one of its continuations
/// failed to get an answer from the model (`model_error`, never the task's
/// own budget or attempts): the provider's failure, not the tree's dead end.
#[must_use]
pub fn provider_failure_round(log: &RoundLog) -> bool {
    log.children.is_empty()
        && !log.failed.is_empty()
        && log.failed.iter().all(|f| f.why.starts_with("exited_without_submitting: model_error:"))
}

impl Rollout {
    #[must_use]
    pub fn new(
        problem: &str,
        statement: String,
        prompt_template: String,
        root_eval: Evaluation,
        limits: Limits,
    ) -> Self {
        Rollout {
            problem: problem.to_owned(),
            statement: statement.clone(),
            prompt_template,
            tree: ProblemTree::new(statement, root_eval, limits.fanout),
            limits,
            round: 0,
            retried: 0,
            spent: Spend::default(),
            open: BTreeMap::new(),
            current: RoundLog::default(),
            plans: plans::Plans::default(),
        }
    }

    #[must_use]
    pub fn problem(&self) -> &str {
        &self.problem
    }

    #[must_use]
    pub fn tree(&self) -> &ProblemTree {
        &self.tree
    }

    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The round currently being run (0 before the first).
    #[cfg(test)]
    #[must_use]
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Model calls charged so far: the grant of every worker still open or
    /// exited without a report, and the reported usage of every submission.
    #[cfg(test)]
    #[must_use]
    pub fn spent_calls(&self) -> u64 {
        self.spent.calls
    }

    /// Everything charged so far: calls and the reported tokens.
    #[must_use]
    pub fn spent(&self) -> Spend {
        self.spent
    }

    fn worker_call_limit(&self) -> u64 {
        self.limits.calls_per_continuation.saturating_add(self.limits.collection_calls)
    }

    /// Continuations the model-call budget can still fund.
    #[must_use]
    pub fn affordable(&self) -> usize {
        let remaining = self.limits.model_calls.saturating_sub(self.spent.calls);
        usize::try_from(remaining / self.worker_call_limit().max(1)).unwrap_or(usize::MAX)
    }

    /// Check the stop conditions, then ask the policy for the next batch (the
    /// view plus the tree's shape, through [`Policy::decide_with_parents`])
    /// and admit its legal part. On `Ok` the round counter has advanced and
    /// the caller spawns one worker per entry (`open_task`) or records its
    /// failure. An empty batch is [`Stop::PolicyStopped`] whatever its cause;
    /// [`Rollout::plan_round_with`] tells a failed policy process apart.
    pub fn plan_round(&mut self, policy: &mut dyn Policy) -> Result<Vec<NodeId>, Stop> {
        if self.tree.best() >= SOLVED {
            return Err(Stop::Solved);
        }
        if self.round.saturating_sub(self.retried) >= self.limits.k1 {
            return Err(Stop::Rounds);
        }
        if let Some(cap) = self.limits.cap_reached(&self.spent) {
            return Err(Stop::CostCap(cap));
        }
        let w = self.limits.w.min(self.affordable());
        if w == 0 {
            return Err(Stop::ModelBudget);
        }
        let (view, parents) = (self.tree.view(), self.tree.parents());
        let batch = self.tree.admit(&policy.decide_with_parents(&view, &parents, w), w);
        if batch.is_empty() {
            return Err(Stop::PolicyStopped);
        }
        self.round += 1;
        self.current = RoundLog { batch: batch.clone(), children: Vec::new(), failed: Vec::new() };
        Ok(batch)
    }

    /// [`Rollout::plan_round`] for the run's configured policy. A policy
    /// process answers an empty batch when it has failed as well as when it
    /// decides to stop; the former is [`Stop::PolicyFailed`] with the error,
    /// so the journal records an honest failure and the built-in policy is
    /// never substituted for it.
    pub fn plan_round_with(&mut self, policy: &mut RolloutPolicy) -> Result<Vec<NodeId>, Stop> {
        match self.plan_round(policy) {
            Err(Stop::PolicyStopped) => Err(policy.failure().map_or(Stop::PolicyStopped, Stop::PolicyFailed)),
            other => other,
        }
    }

    /// [`Rollout::plan_round`] for a round lost to the provider: the same
    /// parents again, within what the budget still funds, without asking the
    /// policy. An outage is not a decision, and a policy whose schedule
    /// follows the round (`pi_doubling.py`) would otherwise move on.
    pub fn retry_round(&mut self, parents: &[NodeId]) -> Result<Vec<NodeId>, Stop> {
        struct Same<'a>(&'a [NodeId]);
        impl Policy for Same<'_> {
            fn decide_with_parents(&mut self, _: &View, _: &[Option<NodeId>], w: usize) -> Vec<NodeId> {
                self.0.iter().copied().take(w).collect()
            }
        }
        self.plan_round(&mut Same(parents))
    }

    /// Register a spawned worker for `task`, continuing `parent`, and reserve
    /// its model-call grant.
    #[cfg(test)]
    pub fn open_task(&mut self, task: &str, parent: NodeId, process: ProcessId) -> Result<(), String> {
        self.open_task_with(task, parent, process, None)
    }

    /// Register a spawned worker for `task`, continuing `parent`, and reserve
    /// its model-call grant; the worker's assignment carries the parent's
    /// transcript when one is named.
    pub fn open_task_with(
        &mut self,
        task: &str,
        parent: NodeId,
        process: ProcessId,
        parent_transcript: Option<String>,
    ) -> Result<(), String> {
        if self.open.contains_key(task) {
            return Err(format!("task {task} is already open"));
        }
        if !self.tree.is_continuable(parent) {
            return Err(format!("node {parent} is not continuable"));
        }
        self.open.insert(
            task.to_owned(),
            Continuation {
                parent,
                process,
                submitted: false,
                exited: false,
                charged: Spend::default(),
                parent_transcript,
            },
        );
        self.spent.calls = self.spent.calls.saturating_add(self.worker_call_limit());
        Ok(())
    }

    /// Record a continuation that yielded no child (spawn refused, worker
    /// exited without submitting).
    pub fn fail_task(&mut self, task: &str, why: impl Into<String>) {
        self.current.failed.push(Failed { task: task.to_owned(), why: why.into() });
    }

    /// Add what a worker that exited without submitting said about it (its
    /// `result.json`: `stopped`, `error`) to the round's record of that
    /// failure, so the journal and `status` say *why* — not only that it
    /// exited. A no-op for a task not recorded as such.
    pub fn explain_failure(&mut self, task: &str, detail: &str) {
        if let Some(f) = self
            .current
            .failed
            .iter_mut()
            .rev()
            .find(|f| f.task == task && f.why == "exited_without_submitting")
        {
            f.why = format!("exited_without_submitting: {detail}");
        }
    }

    /// True once every worker of the round has exited (or there are none).
    ///
    /// Acceptance ends the agent loop, but resource ownership lasts until
    /// the worker exits. Do not close the round while its cleanup is pending.
    #[must_use]
    pub fn round_done(&self) -> bool {
        self.open.values().all(|c| c.exited)
    }

    /// The round's wall clock ran out: every continuation that has not
    /// submitted fails as `timed_out` and is closed to further dispatches;
    /// the round is then done. Returns the processes of those continuations,
    /// for the caller to kill (the ones still alive among them).
    pub fn time_out(&mut self) -> Vec<ProcessId> {
        // Every worker still alive, not only the ones that owe an answer: a
        // submitted worker may still be finishing its receipt or cleanup.
        // Only one that never submitted is failed; an accepted node remains.
        let late: Vec<(String, ProcessId, bool)> = self
            .open
            .iter()
            .filter(|(_, c)| !c.exited)
            .map(|(task, c)| (task.clone(), c.process, c.submitted))
            .collect();
        for (task, _, submitted) in &late {
            self.plans.release(task);
            if let Some(c) = self.open.get_mut(task) {
                c.exited = true;
            }
            if !submitted {
                self.fail_task(task, "timed_out");
            }
        }
        late.into_iter().map(|(_, process, _)| process).collect()
    }

    /// A node of this round was verified, so the problem is over: every other
    /// continuation of the round still running is closed and its process
    /// returned for the caller to kill. The solver's own, `solver`, stays
    /// open until its worker exits, which it does once Lean has verified its
    /// proof: the verdict reaches the worker through the service, and a kill
    /// now would overtake it and lose the worker's receipt. The round ends
    /// with that exit, or its clock kills a worker that lingers.
    ///
    /// What is cut here is spend on a rollout whose answer is already in
    /// hand; a cut continuation that had not submitted is recorded as
    /// `solved_elsewhere`, which is not a failure of its own.
    pub fn stop_for_solve(&mut self, solver: &str) -> Vec<ProcessId> {
        // Every other worker still alive, not only the ones that owe an
        // answer: a submitted worker may still be finishing its receipt or
        // cleanup. Only one that never submitted is failed; an accepted node
        // remains.
        let running: Vec<(String, ProcessId, bool)> = self
            .open
            .iter()
            .filter(|(task, c)| !c.exited && task.as_str() != solver)
            .map(|(task, c)| (task.clone(), c.process, c.submitted))
            .collect();
        for (task, _, submitted) in &running {
            self.plans.release(task);
            if let Some(c) = self.open.get_mut(task) {
                c.exited = true;
            }
            if !submitted {
                self.fail_task(task, CUT_FOR_SOLVE);
            }
        }
        running.into_iter().map(|(_, process, _)| process).collect()
    }

    /// A worker exited. Returns its task and whether it had submitted; a
    /// worker that exits without submitting is a failed continuation. A
    /// continuation already closed by [`Rollout::time_out`] is not failed
    /// twice.
    pub fn worker_exited(&mut self, process: ProcessId) -> Option<(String, bool)> {
        let task = self.open.iter().find(|(_, c)| c.process == process).map(|(t, _)| t.clone())?;
        let (submitted, already) = {
            let c = self.open.get_mut(&task)?;
            let already = c.exited;
            c.exited = true;
            (c.submitted, already)
        };
        self.plans.release(&task);
        if !submitted && !already {
            self.fail_task(&task, "exited_without_submitting");
        }
        Some((task, submitted))
    }

    fn continuation(
        &self,
        resource: &ResourcePath,
        caller: ProcessId,
    ) -> Result<(String, &Continuation), Error> {
        // The harness has matched the run; a rollout knows only its problem.
        let (_, problem, task) = explore::task_of(resource)?;
        let open = self.open.get(task).filter(|_| problem == self.problem);
        let Some(c) = open.filter(|c| !c.exited) else {
            return Err(Error::new(ErrorCode::InvalidInput, "that task is not open"));
        };
        if c.process != caller {
            return Err(Error::new(ErrorCode::PermissionDenied, "not this task's worker"));
        }
        Ok((task.to_owned(), c))
    }

    /// What the worker of `task` is to continue.
    pub fn assignment(&self, resource: &ResourcePath, caller: ProcessId) -> Result<Assignment, Error> {
        let (task, c) = self.continuation(resource, caller)?;
        let parent = self
            .tree
            .node(c.parent)
            .ok_or_else(|| Error::new(ErrorCode::ServiceError, "the parent node is missing"))?;
        let siblings = parent
            .children
            .iter()
            .filter_map(|id| self.tree.node(*id))
            .map(|child| Sibling {
                node: node_u64(child.id),
                check: child.eval.check.clone(),
                excerpt: excerpt(&parent.file, &child.file),
            })
            .collect();
        Ok(Assignment {
            problem: self.problem.clone(),
            task,
            parent: node_u64(parent.id),
            statement: self.statement.clone(),
            file: parent.file.clone(),
            check: Some(parent.eval.check.clone()),
            siblings,
            model_calls: self.limits.calls_per_continuation,
            prompt_template: self.prompt_template.clone(),
            parent_transcript: c.parent_transcript.clone(),
            branch_references: research::references(&self.problem, &self.tree, c.parent, |task| {
                self.open.get(task).is_some_and(|worker| !worker.exited)
            }),
        })
    }

    /// Accept the first submission for a task; a second is refused and the
    /// first stands. The reported calls (clamped to the grant) replace the
    /// reservation made at spawn; the reported tokens count towards the cost
    /// caps.
    pub fn accept_submission(
        &mut self,
        resource: &ResourcePath,
        caller: ProcessId,
        submission: Submission,
    ) -> Result<Served, Error> {
        let (task, c) = self.continuation(resource, caller)?;
        if c.submitted {
            return Err(invalid("this attempt has already submitted its final artifact"));
        }
        // The protocol's textual rules, before anything is recorded. A
        // submission that breaks them cannot be graded — a node whose file
        // does not reproduce the frozen statement scores nothing at all — and
        // refusing it here does NOT consume the task's one submission, so the
        // worker can fix the file and hand it over again.
        // `self.statement` is the whole frozen file; the rule is about the
        // theorem inside it, from `theorem` to the proof assignment.
        // The candidate must also begin with the frozen preamble unchanged
        // (the definitions the statement uses), which `verify` requires too:
        // refused here, it costs neither a Lean run nor the submission.
        if let Some(statement) = lean::extract_statement(&self.statement)
            && let Some(objection) = lean::protocol_objection(statement, &submission.file)
                .or_else(|| lean::preamble_objection(&self.statement, &submission.file))
        {
            return Err(Error::new(ErrorCode::InvalidInput, objection));
        }
        let (parent, process) = (c.parent, c.process);
        // A continuation that hands back its parent's file byte for byte has
        // not continued anything: the node it would create is a copy, with
        // the same score and the same diagnostics, and it costs the tree a
        // round it cannot get back. A continuation inherits its parent's
        // transcript, which ends with the parent deciding to submit, so its
        // model may repeat those last words.
        //
        // As with the protocol rules above, the refusal does NOT spend the
        // task's one submission: the worker has its whole budget left and now
        // knows that handing this file back is not an option. A last word
        // (`final_offer`) is held to the same rule: a worker whose model was
        // refused or never answered hands back the parent's file, and taking
        // it would grow the tree with copies instead of ending the round.
        if self.tree.node(parent).is_some_and(|p| p.file == submission.file) {
            return Err(Error::new(ErrorCode::InvalidInput, UNCHANGED_SUBMISSION.to_owned()));
        }
        // Nothing is recorded yet: the caller runs Lean on the file first and
        // only then calls [`Self::record_submission`]. Unless it is a
        // `final_offer`, a submission that is not a verified complete proof
        // is refused by the caller without consuming the task's one
        // submission, so the model can fix it.
        Ok(Served::Accepted { task, parent, process, submission })
    }

    /// Mark the task submitted and charge the run for what the worker spent.
    /// Called once, after the candidate has been evaluated and accepted.
    pub fn record_submission(&mut self, task: &str, submission: &Submission) {
        self.plans.release(task);
        let grant = self.worker_call_limit();
        // Apply cumulative accounting idempotently. Admission accepts only
        // one artifact; receipt reconciliation must not charge it twice.
        let (first, charged) = match self.open.get_mut(task) {
            Some(c) => {
                let first = !c.submitted;
                let charged = c.charged;
                c.submitted = true;
                c.charged = Spend {
                    calls: u64::from(submission.model_calls).min(grant),
                    tokens_in: submission.input_tokens,
                    tokens_out: submission.output_tokens,
                    cost_nano: submission.cost_nano_usd,
                };
                (first, charged)
            }
            None => (true, Spend::default()),
        };
        let used = u64::from(submission.model_calls).min(grant);
        if first {
            self.spent.calls = self.spent.calls.saturating_sub(grant);
        }
        self.spent.calls = self.spent.calls.saturating_add(used.saturating_sub(charged.calls));
        self.spent.tokens_in =
            self.spent.tokens_in.saturating_add(submission.input_tokens.saturating_sub(charged.tokens_in));
        self.spent.tokens_out =
            self.spent.tokens_out.saturating_add(submission.output_tokens.saturating_sub(charged.tokens_out));
        self.spent.cost_nano =
            self.spent.cost_nano.saturating_add(submission.cost_nano_usd.saturating_sub(charged.cost_nano));
    }

    /// Serve one `explore` dispatch. The kernel already scoped the caller's
    /// capability to `[key, problem, task]` (the run's key, [`wire::key_of`]); this re-validates at access and
    /// binds the task to the process the harness spawned for it.
    pub fn serve(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        caller: ProcessId,
        payload: &Payload,
    ) -> Served {
        match action {
            explore::ACTION_ASSIGNMENT => {
                Served::Reply(self.assignment(resource, caller).and_then(|a| Payload::encode(&a)))
            }
            explore::ACTION_SUBMIT => match payload.decode::<Submission>() {
                Ok(submission) => match self.accept_submission(resource, caller, submission) {
                    Ok(served) => served,
                    Err(e) => Served::Reply(Err(e)),
                },
                Err(e) => Served::Reply(Err(e)),
            },
            explore::ACTION_PUBLISH_PLAN | explore::ACTION_READ_PLANS => {
                Served::Reply(self.serve_plans(action, resource, caller, payload, unix_ms()))
            }
            _ => Served::Reply(Err(Error::new(ErrorCode::InvalidInput, "unknown action"))),
        }
    }

    /// Add the evaluated child of an accepted submission to the tree.
    pub fn add_child(
        &mut self,
        parent: NodeId,
        task: &str,
        process: ProcessId,
        submission: &Submission,
        eval: Evaluation,
    ) -> Result<NodeId, String> {
        let worker = WorkerRecord {
            process: process.0,
            model_calls: submission.model_calls,
            input_tokens: submission.input_tokens,
            output_tokens: submission.output_tokens,
        };
        let id = self
            .tree
            .push(parent, self.round, task.to_owned(), submission.file.clone(), eval, Some(worker))
            .ok_or_else(|| format!("node {parent} does not exist"))?;
        self.current.children.push(id);
        Ok(id)
    }

    /// The round just finished is retried: its number stays taken, `K1` is
    /// not charged for it.
    pub fn note_provider_retry(&mut self) {
        self.retried += 1;
    }

    /// Close the round: forget its tasks (later dispatches for them are
    /// refused) and report what it did.
    pub fn finish_round(&mut self) -> RoundEnd {
        // Anything the harness has not yet seen exit: a worker whose exit
        // event is still in flight, or one that had answered and was left
        // running. Nothing here may outlive the round, so the caller kills
        // whichever are still alive.
        let stragglers = self.open.values().filter(|c| !c.exited).map(|c| c.process).collect();
        self.open.clear();
        self.plans.clear();
        RoundEnd {
            round: self.round,
            log: std::mem::take(&mut self.current),
            stragglers,
            spend: self.spent,
            usd_micro: self.spent.usd_micro(),
        }
    }

    /// The round in progress as a journal record would show it once closed:
    /// its batch, the children and failures so far, the spend so far, no
    /// reason. What `discovery tree` adds for a live problem, since the
    /// journal gets the record only when the round ends.
    #[must_use]
    pub fn open_round_record(&self, policy: &PolicyRecord) -> RoundRecord {
        RoundRecord::of_round(self.round, &self.current, self.spent, self.spent.usd_micro(), policy)
    }

    /// The journal line for node `id`.
    #[must_use]
    pub fn node_record(&self, id: NodeId, ms_since_start: u64) -> Option<NodeRecord> {
        let n = self.tree.node(id)?;
        Some(NodeRecord {
            v: JOURNAL_VERSION,
            id: node_u64(n.id),
            parent: n.parent.map(node_u64),
            round: n.round,
            task: n.task.clone(),
            file: n.file.clone(),
            check: n.eval.check.clone(),
            verified: n.eval.verified,
            verify_rejection: n.eval.verify_rejection.clone(),
            score: n.eval.score,
            scoring: scoring::VERSION,
            sketch: n.eval.sketch.clone(),
            completion: n.eval.completion.clone(),
            errors_obs: n.eval.errors_obs.clone(),
            analysis_failed: n.eval.analysis_failed.clone(),
            worker: n.worker.clone(),
            ms_since_start,
        })
    }
}

fn node_u64(id: NodeId) -> u64 {
    u64::try_from(id).unwrap_or(u64::MAX)
}

/// Micro-USD as a USD figure for the record; the integer is what is enforced.
fn usd(micro: u64) -> f64 {
    micro as f64 / 1e6
}

/// What a worker that exited without submitting recorded in its `result.json`
/// (`stopped`, and `error` when it has one), as one bounded line; `None` when
/// the bytes are not such a record. The file is the worker's own — untrusted
/// text, quoted, never interpreted.
#[must_use]
pub fn worker_failure_detail(bytes: &[u8]) -> Option<String> {
    const EXCERPT: usize = 160;
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let stopped = value.get("stopped").and_then(serde_json::Value::as_str)?;
    let mut detail = stopped.to_owned();
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        let one_line = error.split_whitespace().collect::<Vec<_>>().join(" ");
        let excerpt: String = one_line.chars().take(EXCERPT).collect();
        if !excerpt.is_empty() {
            detail.push_str(": ");
            detail.push_str(&excerpt);
            if one_line.chars().count() > EXCERPT {
                detail.push('…');
            }
        }
    }
    Some(detail)
}

/// A short, diff-ish excerpt of a child for its siblings' workers: the child's
/// text from the first line that differs from the parent (its head when nothing
/// differs), cut to [`EXCERPT_CHARS`] characters. Untrusted text either way.
#[must_use]
pub fn excerpt(parent: &str, child: &str) -> String {
    let common = parent.lines().zip(child.lines()).take_while(|(a, b)| a == b).count();
    let tail: Vec<&str> = child.lines().skip(common).collect();
    let text = if tail.is_empty() { child.to_owned() } else { tail.join("\n") };
    text.chars().take(EXCERPT_CHARS).collect()
}

// --- Journal records ---------------------------------------------------------

/// One line of `nodes.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub v: u32,
    pub id: u64,
    pub parent: Option<u64>,
    pub round: u32,
    pub task: Option<String>,
    pub file: String,
    pub check: CheckResponse,
    pub verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_rejection: Option<String>,
    pub score: f64,
    /// The task-scoring protocol version that produced `score`.
    pub scoring: u32,
    /// Protocol v3: the whole `analyze` reply of a `sorries` node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sketch: Option<AnalyzeResponse>,
    /// What `complete` did with a `sorries` node; `original_file` set when
    /// the node is a completion solve (`file` is then the spliced proof).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionRecord>,
    /// The error observations of an `errors` node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_obs: Option<scoring::ErrorsObservation>,
    /// Why a `sorries` node scored the base without a credit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_failed: Option<String>,
    pub worker: Option<WorkerRecord>,
    /// Milliseconds since this problem's rollout started.
    pub ms_since_start: u64,
}

/// One line of `rounds.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoundRecord {
    pub v: u32,
    pub round: u32,
    /// The exploration policy that produced the tree. The harness always
    /// writes it; a record without it reads as `None`, honestly "not
    /// recorded".
    #[serde(default)]
    pub policy: Option<PolicyRecord>,
    pub batch: Vec<u64>,
    pub children: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<Failed>,
    /// The rollout's cumulative token spend through this round, as the
    /// workers reported it (a worker that exited without submitting is not in
    /// it). Zero in a record without it.
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    /// What the providers charged for those tokens, in USD, as they reported
    /// it; absent when none did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// With `reason: "policy_failed"`: how the policy process failed. Quotes
    /// the policy's stderr and reply — bounded, untrusted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_error: Option<String>,
}

impl RoundRecord {
    fn from_end(end: &RoundEnd, policy: &PolicyRecord) -> Self {
        Self::of_round(end.round, &end.log, end.spend, end.usd_micro, policy)
    }

    /// A round as it stands — completed, or still open (then `children` and
    /// `failed` are what has arrived so far) — with the rollout's cumulative
    /// spend; no reason.
    fn of_round(
        round: u32,
        log: &RoundLog,
        spend: Spend,
        usd_micro: Option<u64>,
        policy: &PolicyRecord,
    ) -> Self {
        RoundRecord {
            v: JOURNAL_VERSION,
            round,
            policy: Some(policy.clone()),
            batch: log.batch.iter().copied().map(node_u64).collect(),
            children: log.children.iter().copied().map(node_u64).collect(),
            failed: log.failed.clone(),
            tokens_in: spend.tokens_in,
            tokens_out: spend.tokens_out,
            usd: usd_micro.map(usd),
            reason: None,
            policy_error: None,
        }
    }

    fn empty(round: u32, policy: &PolicyRecord) -> Self {
        RoundRecord {
            v: JOURNAL_VERSION,
            round,
            policy: Some(policy.clone()),
            batch: vec![],
            children: vec![],
            failed: vec![],
            tokens_in: 0,
            tokens_out: 0,
            usd: None,
            reason: None,
            policy_error: None,
        }
    }

    /// The last record of a rollout: why it stopped, and the policy's error
    /// when that is why.
    #[must_use]
    fn closed(mut self, stop: &Stop) -> Self {
        self.reason = Some(stop.reason());
        self.policy_error = stop.policy_error().map(str::to_owned);
        self
    }
}

/// The two append-only files of one problem.
struct Journal {
    nodes: File,
    rounds: File,
}

impl Journal {
    /// Opens a problem's journal for a new tree: a directory that already
    /// holds a tree is refused, because appending a second one would corrupt
    /// the tree. One whose problem ended before it had a root keeps its round
    /// records, and the new tree follows them.
    fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let open = |name: &str| {
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join(name))
                .map_err(|e| format!("{}: {e}", dir.join(name).display()))
        };
        let nodes = open(NODES_FILE)?;
        if nodes.metadata().map_err(|e| e.to_string())?.len() > 0 {
            return Err(format!("{} already holds a tree", dir.display()));
        }
        Ok(Journal { nodes, rounds: open(ROUNDS_FILE)? })
    }

    fn node(&mut self, record: &NodeRecord) -> Result<(), String> {
        append(&mut self.nodes, record)
    }

    fn round(&mut self, record: &RoundRecord) -> Result<(), String> {
        append(&mut self.rounds, record)
    }
}

fn append<T: Serialize>(file: &mut File, record: &T) -> Result<(), String> {
    let line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

// --- Accepting a task ---------------------------------------------------------

/// A task accepted for execution: everything a run needs, resolved from the
/// document and the service's directories, plus the normalized document that
/// `task.json` records.
#[derive(Clone, Debug, PartialEq)]
pub struct RunPlan {
    pub name: String,
    /// `<out>/<name>`, the journal directory.
    pub dir: PathBuf,
    /// `<out>/<name>.workers`, the workers' writable directory.
    pub workers_dir: PathBuf,
    /// In the order they are started (the curriculum).
    pub problems: Vec<String>,
    /// The frozen file of each problem the task supplied (`statements`),
    /// by id; a problem not here is the `lean` service's own.
    pub statements: BTreeMap<String, String>,
    /// Passed to every worker verbatim.
    pub prompt_template: String,
    pub policy: PolicySource,
    pub concurrency: usize,
    pub limits: Limits,
    /// Whether the run's workers' shell has network: the task's choice, or
    /// the service's default (`--worker-network`) when it made none.
    pub worker_network: bool,
    pub document: Task,
}

impl Limits {
    /// A task's resolved limits.
    #[must_use]
    pub fn from_task(limits: discovery::Limits) -> Self {
        Limits {
            w: limits.w,
            k1: limits.k1,
            fanout: limits.fanout,
            calls_per_continuation: limits.calls_per_continuation,
            collection_calls: 0,
            model_calls: limits.model_calls_per_problem,
            max_tokens: limits.max_tokens_per_problem,
            max_usd_micro: limits.max_usd_micro_per_problem,
        }
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

/// The file `<dir>/<path>`, re-checked at access: it must exist, be a file,
/// and — symlinks resolved — still lie under `dir`. Returns the resolved path,
/// which is what gets read or run. A concurrent rewrite of `dir` between this
/// check and the access is not defended against; the directory is the
/// operator's read-only bind.
fn file_under(dir: &Path, flag: &str, field: &str, path: &RelativePath) -> Result<PathBuf, Error> {
    let root = dir.canonicalize().map_err(|e| {
        Error::new(
            ErrorCode::ServiceError,
            format!("{flag} {}: not available to this harness: {e}", dir.display()),
        )
    })?;
    let joined = path.segments().iter().fold(dir.to_path_buf(), |p, s| p.join(s));
    let real = joined.canonicalize().map_err(|e| invalid(format!("task: {field} {path:?}: {e}")))?;
    if !real.starts_with(&root) {
        return Err(invalid(format!("task: {field} {path:?}: resolves outside {}", dir.display())));
    }
    if !real.is_file() {
        return Err(invalid(format!("task: {field} {path:?}: not a file")));
    }
    Ok(real)
}

fn read_under(dir: &Path, flag: &str, field: &str, path: &RelativePath) -> Result<String, Error> {
    let real = file_under(dir, flag, field, path)?;
    std::fs::read_to_string(&real).map_err(|e| invalid(format!("task: {field} {path:?}: {e}")))
}

/// Parses a task's template the way the prover will (strict undefined
/// variables, no autoescape, no other templates, `trim_blocks` and
/// `lstrip_blocks`): a template that does not parse is refused with the
/// task, before any process exists. Parsing is not rendering — the harness
/// still never renders it; an undefined variable or a runaway render can
/// only surface in a worker, per attempt.
pub fn check_template_syntax(text: &str) -> Result<(), Error> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.add_template_owned("system".to_owned(), text.to_owned())
        .map(drop)
        .map_err(|e| invalid(format!("task: prompt_template does not parse: {e:#}")))
}

/// Everything about a task that the document and this process's filesystem
/// decide: the document's own rules against the ceilings, then the files
/// under `--tasks-dir`, the template's syntax, the policy under
/// `--policies-dir` (it must be startable), and the run name against the
/// directories under `--out`. The first broken rule refuses the whole task as
/// an `InvalidInput` error. What remains for the caller is the `lean`
/// service's word ([`admit`]): every benchmark id must resolve through `lean
/// statement`, and every supplied statement must be registered.
///
/// The run's directories are `<out>/<name>` and `<out>/<name>.workers`:
/// `out` is the directory of the run's project, the service's
/// `<--out>/<project>`, which a run's process sees as its own `--out`.
pub fn resolve_task(task: &Task, options: &Options, out: &Path) -> Result<RunPlan, Error> {
    resolve_task_mode(task, options, out, &options.ceilings(), false)
}

/// [`resolve_task`] under `ceilings`, of a run that exists when `resuming`.
fn resolve_task_mode(
    task: &Task,
    options: &Options,
    out: &Path,
    ceilings: &Ceilings,
    resuming: bool,
) -> Result<RunPlan, Error> {
    let mut accepted = task.validate(ceilings)?;
    // The recorded task carries the value the run runs with, so a resume
    // keeps it whatever the service's default is by then.
    let worker_network = accepted.worker_network.unwrap_or(options.worker_network);
    accepted.worker_network = Some(worker_network);
    let mut statements = BTreeMap::new();
    let problems = match &accepted.problems {
        ProblemSet::Inline(ids) => ids.clone(),
        ProblemSet::File { path, range } => {
            let text = read_under(&options.tasks_dir, "--tasks-dir", "problems_file", path)?;
            discovery::problem_list(discovery::problem_ids_in(&text), *range, options.max_problems_per_task)?
        }
        ProblemSet::Statements(list) => {
            for s in list {
                if let Some(why) = lean::frozen_source_objection(&s.id, &s.source) {
                    return Err(invalid(format!("task: statement {:?}: {why}", s.id)));
                }
                statements.insert(s.id.clone(), s.source.clone());
            }
            list.iter().map(|s| s.id.clone()).collect()
        }
    };
    let prompt_template = match &accepted.prompt_template {
        TemplateSource::Inline(text) => text.clone(),
        TemplateSource::File(path) => {
            let text = read_under(&options.tasks_dir, "--tasks-dir", "prompt_template_file", path)?;
            discovery::check_prompt_template(&text)?;
            text
        }
    };
    check_template_syntax(&prompt_template)?;
    let program = accepted
        .policy
        .as_ref()
        .map(|path| file_under(&options.policies_dir, "--policies-dir", "policy", path))
        .transpose()?;
    let policy = options.policy_source(program);
    policy.check().map_err(|why| invalid(format!("task: policy: {why}")))?;
    let dir = out.join(&accepted.name);
    let workers_dir = out.join(format!("{}{WORKERS_SUFFIX}", accepted.name));
    if !resuming && (dir.exists() || workers_dir.exists()) {
        return Err(invalid(format!("task: run {:?} already exists", accepted.name)));
    }
    Ok(RunPlan {
        name: accepted.name.clone(),
        dir,
        workers_dir,
        document: accepted.normalized(problems.clone(), prompt_template.clone()),
        problems,
        statements,
        prompt_template,
        policy,
        concurrency: accepted.concurrency,
        limits: Limits::from_task(accepted.limits),
        worker_network,
    })
}

/// Creates the run directory with `task.json` and an empty `discovery.jsonl`,
/// and the workers' directory beside it. Both are created exclusively, so two
/// runs never share one, and each names `owner` (the creating run process's
/// endpoint) in its [`OWNER_FILE`] at once, so the service removes the
/// directories of a start that did not happen only when that process made
/// them. A partial failure removes what this call created and nothing else.
pub fn materialize(plan: &RunPlan, owner: &str) -> Result<(), Error> {
    if let Err(e) = std::fs::create_dir(&plan.dir) {
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            invalid(format!("task: run {:?} already exists", plan.name))
        } else {
            Error::new(ErrorCode::ServiceError, format!("{}: {e}", plan.dir.display()))
        });
    }
    let mut workers_created = false;
    let mut populate = || -> Result<(), String> {
        std::fs::write(plan.dir.join(OWNER_FILE), owner).map_err(|e| e.to_string())?;
        let task = serde_json::to_string_pretty(&plan.document).map_err(|e| e.to_string())?;
        std::fs::write(plan.dir.join(TASK_FILE), format!("{task}\n")).map_err(|e| e.to_string())?;
        File::create(plan.dir.join(EVENTS_FILE)).map_err(|e| e.to_string())?;
        std::fs::create_dir(&plan.workers_dir).map_err(|e| format!("{}: {e}", plan.workers_dir.display()))?;
        workers_created = true;
        std::fs::write(plan.workers_dir.join(OWNER_FILE), owner).map_err(|e| e.to_string())
    };
    let populated = populate();
    populated.map_err(|why| {
        let _ = std::fs::remove_dir_all(&plan.dir);
        if workers_created {
            let _ = std::fs::remove_dir_all(&plan.workers_dir);
        }
        Error::new(ErrorCode::ServiceError, format!("{}: {why}", plan.dir.display()))
    })
}

/// The wall clock, for the record of when a run started (a status field; the
/// loop's own timing uses `Instant`).
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use nucleus_discovery_interface::TaskLimits;

    pub(super) const TEST_PROVER: &str = "/programs/nucleus-prover";
    pub(super) const TEST_INTERPRETER: &str = "/opt/py";

    pub(super) fn parse(a: &[&str]) -> Result<Options, String> {
        Options::parse(a.iter().map(|s| (*s).to_owned()))
    }

    /// The required options, with the USD ceiling declared absent.
    pub(super) fn base_args() -> Vec<&'static str> {
        vec!["--out", "/out", "--prover-executable", TEST_PROVER, "--no-usd-ceiling"]
    }

    /// The required options without any word on the USD ceiling.
    pub(super) fn bare_args() -> Vec<&'static str> {
        vec!["--out", "/out", "--prover-executable", TEST_PROVER]
    }

    pub(super) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nucleus_discovery_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- scoring protocol ---

    fn check(status: CheckStatus) -> CheckResponse {
        CheckResponse { status, invalid_reason: None, errors: vec![], sorries: vec![], elapsed_ms: 1 }
    }

    fn verdict(accepted: bool, rejection: Option<&str>) -> VerifyResponse {
        VerifyResponse { accepted, rejection: rejection.map(str::to_owned), detail: None, axioms: vec![] }
    }

    fn goal(text: &str) -> nucleus_lean_interface::OpenGoal {
        nucleus_lean_interface::OpenGoal { line: 1, column: 0, goal: text.to_owned() }
    }

    #[test]
    fn the_score_protocol_maps_every_verdict() {
        let solved =
            Evaluation::from_verdicts(check(CheckStatus::Complete), Some(Ok(verdict(true, None))), None);
        assert_eq!(
            (solved.score, solved.verified, solved.verify_rejection.as_deref()),
            (1.0, Some(true), None)
        );
        let rejected = Evaluation::from_verdicts(
            check(CheckStatus::Complete),
            Some(Ok(verdict(false, Some("type_mismatch")))),
            None,
        );
        assert_eq!(
            (rejected.score, rejected.verified, rejected.verify_rejection.as_deref()),
            (0.9, Some(false), Some("type_mismatch"))
        );
        let broken = Evaluation::from_verdicts(
            check(CheckStatus::Complete),
            Some(Err(Error::new(ErrorCode::Timeout, "deadline"))),
            None,
        );
        assert_eq!(
            (broken.score, broken.verified, broken.verify_rejection.as_deref()),
            (0.9, Some(false), Some("verifier_timeout"))
        );
        let sorries = Evaluation::from_verdicts(check(CheckStatus::Sorries), None, None);
        assert_eq!((sorries.score, sorries.verified), (0.5, None), "no analysis recorded: the base");
        let full = Some(scoring::Credit { closed: 3, open: 1, hollow: false });
        assert_eq!(Evaluation::from_verdicts(check(CheckStatus::Errors), None, full).score, 0.05);
        assert_eq!(Evaluation::from_verdicts(check(CheckStatus::Invalid), None, full).score, 0.0);
        // A `complete` check never scores 1.0 without the verifier's word.
        assert_eq!(Evaluation::from_verdicts(check(CheckStatus::Complete), None, None).score, 0.9);

        // Version 3: a partial proof earns its verified share of the sketch.
        let root = Evaluation::from_verdicts(
            CheckResponse {
                sorries: vec![goal("n : ℕ\n⊢ P n ∧ Q n ∧ R n")], ..check(CheckStatus::Sorries)
            },
            None,
            Some(scoring::Credit { closed: 0, open: 1, hollow: true }),
        );
        assert_eq!(root.score, 0.5, "the root restates itself: hollow");
        let genuine = Evaluation::from_verdicts(
            CheckResponse {
                sorries: vec![goal("n : ℕ\nh : P n\n⊢ Q n ∧ R n")],
                ..check(CheckStatus::Sorries)
            },
            None,
            full,
        );
        assert_eq!(genuine.score, 0.8, "0.5 + 0.4 · 3/4");
        let spoiled = Evaluation::from_verdicts(
            CheckResponse {
                sorries: vec![goal("h : ¬(P n ∧ Q n ∧ R n)\n⊢ False")],
                ..check(CheckStatus::Sorries)
            },
            None,
            Some(scoring::Credit { closed: 3, open: 1, hollow: true }),
        );
        assert_eq!(spoiled.score, 0.5, "a hollow leaf spoils every closed step");
    }

    // --- protocol v3: the combination of the lean replies ---

    const STATEMENT: &str = "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n :=\nsorry\n";
    const PARTIAL: &str = "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n := by\n  have h : n + 0 = n := by simp\n  sorry\n";
    const BROKEN: &str =
        "import Mathlib\n\ntheorem demo (n : Nat) : n + 0 = n := by\n  simp\n  omega\n  rfl\n\n";

    fn analysis(closed: u64, open: u64, hollow: bool) -> AnalyzeResponse {
        AnalyzeResponse {
            status: CheckStatus::Sorries,
            leaves: vec![],
            steps: vec![],
            counts: nucleus_lean_interface::SketchCounts { closed, open, ..Default::default() },
            hollow,
            false_leaf: false,
            restates_root: false,
            statement_type_identical: Some(true),
            truncated: false,
            suite: lean::ANALYSIS_SUITE_ID.into(),
            error: None,
            elapsed_ms: 5,
        }
    }

    fn completion(all_closed: bool, spliced: Option<&str>) -> CompleteResponse {
        CompleteResponse {
            status: CheckStatus::Sorries,
            closers: vec![],
            all_closed,
            spliced: spliced.map(str::to_owned),
            auto_closable: u64::from(all_closed),
            truncated: false,
            suite: lean::COMPLETION_SUITE_ID.into(),
            error: None,
            elapsed_ms: 7,
        }
    }

    fn replies(
        complete: Option<Result<CompleteResponse, Error>>,
        verify: Option<Result<VerifyResponse, Error>>,
        analyze: Option<Result<AnalyzeResponse, Error>>,
    ) -> LeanReplies {
        LeanReplies { complete, verify, analyze }
    }

    /// A `sorries` node whose analysis answered.
    fn sketched(closed: u64, open: u64, hollow: bool) -> Evaluation {
        let sorries =
            CheckResponse { sorries: vec![goal("n : ℕ\n⊢ n + 0 = n")], ..check(CheckStatus::Sorries) };
        let r = replies(Some(Ok(completion(false, None))), None, Some(Ok(analysis(closed, open, hollow))));
        Evaluation::combine(PARTIAL, STATEMENT, sorries, r)
    }

    /// An `errors` node with these diagnostics, one per line of the body.
    fn broken(messages: &[&str]) -> Evaluation {
        let errors = messages
            .iter()
            .enumerate()
            .map(|(i, m)| nucleus_lean_interface::Diagnostic {
                line: 4 + i as u64,
                column: 2,
                message: (*m).to_owned(),
            })
            .collect();
        let errors = CheckResponse { errors, ..check(CheckStatus::Errors) };
        Evaluation::combine(BROKEN, STATEMENT, errors, LeanReplies::default())
    }

    #[test]
    fn a_partial_proof_is_completed_or_credited_and_a_broken_one_observed() {
        // A genuine sketch earns its verified share; the record keeps the whole analysis.
        let credited = sketched(1, 1, false);
        assert_eq!(
            (credited.score, credited.verified, credited.analysis_failed.as_deref()),
            (0.7, None, None)
        );
        assert_eq!(credited.credit(), Some(scoring::Credit { closed: 1, open: 1, hollow: false }));
        assert_eq!(credited.sketch.as_ref().map(|s| s.counts.closed), Some(1));
        assert_eq!(credited.completion.as_ref().map(|c| (c.all_closed, c.auto_closable)), Some((false, 0)));
        // A hollow leaf spoils the credit; a redefined theorem withholds it (no failure).
        assert_eq!(sketched(3, 1, true).score, 0.5);
        let redefined = AnalyzeResponse { statement_type_identical: Some(false), ..analysis(3, 1, false) };
        let sorries =
            || CheckResponse { sorries: vec![goal("⊢ n + 0 = n")], ..check(CheckStatus::Sorries) };
        let mismatch = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(Some(Ok(completion(false, None))), None, Some(Ok(redefined))),
        );
        assert_eq!((mismatch.score, mismatch.credit(), mismatch.analysis_failed), (0.5, None, None));

        // Every goal closed and verify accepted: a solve on the spliced file, the original kept.
        let solved = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(Some(Ok(completion(true, Some("spliced proof")))), Some(Ok(verdict(true, None))), None),
        );
        assert_eq!(
            (solved.score, solved.verified, solved.spliced.as_deref(), solved.credit()),
            (1.0, Some(true), Some("spliced proof"), None)
        );
        assert_eq!(solved.completion.as_ref().and_then(|c| c.original_file.as_deref()), Some(PARTIAL));
        assert_eq!(solved.check.status, CheckStatus::Sorries, "the check is the verdict on the original");
        let mut tree = ProblemTree::new("root".into(), eval(0.5), 3);
        let id = tree.push(0, 1, "t1".into(), PARTIAL.into(), solved, None).unwrap();
        assert_eq!(tree.node(id).unwrap().file, "spliced proof", "the node's file is the accepted proof");
        // Verify rejected the splice: scored as a sketch, the rejection kept on the completion.
        let rejected = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(
                Some(Ok(completion(true, Some("spliced proof")))),
                Some(Ok(verdict(false, Some("non_standard_axioms")))),
                Some(Ok(analysis(1, 1, false))),
            ),
        );
        assert_eq!((rejected.score, rejected.verified, rejected.spliced), (0.7, None, None));
        assert_eq!(
            rejected.completion.as_ref().and_then(|c| c.verify_rejection.as_deref()),
            Some("non_standard_axioms")
        );

        // A service that does not know the action, or a reply carrying `error`: the base, and why.
        let unknown = Error::new(ErrorCode::InvalidInput, "unknown action");
        let older = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(Some(Err(unknown.clone())), None, Some(Err(unknown.clone()))),
        );
        assert_eq!((older.score, older.credit()), (0.5, None));
        assert_eq!(older.analysis_failed.as_deref(), Some("analyze: InvalidInput: unknown action"));
        assert_eq!(
            older.completion.as_ref().and_then(|c| c.error.as_deref()),
            Some("InvalidInput: unknown action")
        );
        assert!(older.sketch.is_none());
        let backstop = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(
                Some(Ok(completion(false, None))),
                None,
                Some(Ok(AnalyzeResponse { error: Some("REPL died".into()), ..analysis(2, 1, false) })),
            ),
        );
        assert_eq!((backstop.score, backstop.analysis_failed.as_deref()), (0.5, Some("analyze: REPL died")));
        assert!(
            backstop.sketch.is_none(),
            "a reply carrying `error` is not a sketch: a reader would credit it"
        );
        // Only the completion failed: the sketch still earns its credit, the failure is on the record.
        let half = Evaluation::combine(
            PARTIAL,
            STATEMENT,
            sorries(),
            replies(Some(Err(unknown)), None, Some(Ok(analysis(1, 1, false)))),
        );
        assert_eq!((half.score, half.analysis_failed.as_deref()), (0.7, None));
        assert_eq!(
            half.completion.as_ref().and_then(|c| c.error.as_deref()),
            Some("InvalidInput: unknown action")
        );

        // Errors: the observation places the first error in the proof body.
        let observed = broken(&["unsolved goals\n⊢ n = n", "Tactic `omega` failed: no contradiction"]);
        assert_eq!(observed.score, 0.05);
        let obs = observed.errors_obs.clone().unwrap();
        assert_eq!((obs.count, obs.repairable, obs.first_error_fraction), (2, true, Some(0.3333)));
        assert_eq!(obs.classes, ["unsolved_goals", "automation_failed"]);
        assert_eq!(body_lines(STATEMENT, BROKEN), (3, 6));
        assert_eq!(body_lines("no theorem here", BROKEN), (1, 6), "an unmatched statement: the whole file");

        // The journal line carries it all.
        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        r.open_task("t000002", 0, ProcessId(8)).unwrap();
        let a = r.add_child(0, "t000001", ProcessId(7), &submission(PARTIAL), credited).unwrap();
        let b = r.add_child(0, "t000002", ProcessId(8), &submission(BROKEN), observed).unwrap();
        let json = serde_json::to_value(r.node_record(a, 1).unwrap()).unwrap();
        assert_eq!(json["scoring"], 3);
        assert_eq!(json["sketch"]["counts"]["closed"], 1);
        assert_eq!(json["completion"]["all_closed"], false);
        assert!(json.get("analysis_failed").is_none() && json.get("errors_obs").is_none(), "{json}");
        let record = r.node_record(b, 2).unwrap();
        let text = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<NodeRecord>(&text).unwrap(), record, "round trip");
        for id in [0, a, b] {
            let saved = r.node_record(id, 2).unwrap();
            assert_eq!(
                resume::evaluation(&saved),
                r.tree.node(id).unwrap().eval,
                "resume retains the original score, sketch and error observations"
            );
        }
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["errors_obs"]["repairable"], true);
        assert!(json.get("sketch").is_none(), "{json}");
        // The policy's view says the same as the record, and each child's
        // move against its parent.
        let view = r.tree().view();
        let seen = view.observations[a].as_ref().unwrap();
        assert_eq!(seen.sketch.as_ref().map(|s| (s.hollow, s.closed, s.open)), Some((Some(false), 1, 1)));
        assert_eq!(seen.delta_vs_parent, Some((1, 0)));
        let seen = view.observations[b].as_ref().unwrap();
        assert_eq!(seen.errors_obs.as_ref().map(|e| e.repairable), Some(true));
        assert_eq!(seen.delta_vs_parent, Some((0, 2)));
        assert!(seen.sketch.is_none());
    }

    #[test]
    fn a_workers_recorded_failure_reaches_the_round_record_and_the_policy_sees_verdicts() {
        // What the prover writes when it gives up before submitting.
        let record = br#"{"attempts":0,"stopped":"model_error","submitted":false,
            "error":"ServiceError: provider reports insufficient credit (http 402): This request\n requires more credits"}"#;
        assert_eq!(
            worker_failure_detail(record).as_deref(),
            Some(
                "model_error: ServiceError: provider reports insufficient credit (http 402): This request requires more credits"
            )
        );
        let long = format!(r#"{{"stopped":"truncated","error":"{}"}}"#, "x".repeat(500));
        let detail = worker_failure_detail(long.as_bytes()).unwrap();
        assert!(
            detail.starts_with("truncated: xxx") && detail.ends_with('…') && detail.chars().count() < 180
        );
        assert_eq!(worker_failure_detail(br#"{"stopped":"no_candidate"}"#).as_deref(), Some("no_candidate"));
        assert_eq!(worker_failure_detail(b"not json"), None);
        assert_eq!(worker_failure_detail(br#"{"submitted":false}"#), None, "no `stopped`, nothing to say");

        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t1", 0, ProcessId(11)).unwrap();
        r.open_task("t2", 0, ProcessId(12)).unwrap();
        assert_eq!(r.worker_exited(ProcessId(11)), Some(("t1".to_owned(), false)));
        r.explain_failure("t1", "model_error: insufficient credit");
        r.explain_failure("t2", "ignored: t2 has not failed");
        let end = r.finish_round();
        assert_eq!(
            end.log.failed,
            [Failed {
                task: "t1".into(),
                why: "exited_without_submitting: model_error: insufficient credit".into()
            }]
        );

        // The policy's view carries Lean's verdict and the spend of every node.
        let mut tree = ProblemTree::new("root".into(), eval(0.5), 3);
        grow(&mut tree, 0, 1, 0);
        let view = tree.view();
        assert_eq!(view.observations.len(), view.revealed.len());
        let root = view.observations[0].as_ref().unwrap();
        assert_eq!((root.status.as_str(), root.errors, root.sorries, root.tokens_in), ("sorries", 0, 0, 0));
    }

    // --- policy over Fixture-style trees ---

    pub(super) fn eval(score: Score) -> Evaluation {
        let status = if score >= 1.0 {
            CheckStatus::Complete
        } else if score >= 0.5 {
            CheckStatus::Sorries
        } else {
            CheckStatus::Errors
        };
        Evaluation {
            check: check(status),
            verified: (score >= 1.0).then_some(true),
            verify_rejection: None,
            score,
            sketch: None,
            completion: None,
            spliced: None,
            errors_obs: None,
            analysis_failed: None,
        }
    }

    pub(super) fn limits() -> Limits {
        Limits {
            w: 3,
            k1: 4,
            fanout: 3,
            calls_per_continuation: 3,
            collection_calls: 5,
            model_calls: 100,
            max_tokens: 0,
            max_usd_micro: 0,
        }
    }

    /// A spend as a worker would report it, with the cost the provider
    /// charged: this fake one charges 1 micro-USD per input token and 2 per
    /// output token. Nothing in the service derives that figure.
    pub(super) fn spend(tokens_in: u64, tokens_out: u64) -> Spend {
        Spend { calls: 0, tokens_in, tokens_out, cost_nano: charged(tokens_in, tokens_out) }
    }

    fn charged(tokens_in: u64, tokens_out: u64) -> u64 {
        (tokens_in + 2 * tokens_out).saturating_mul(1_000)
    }

    fn report(tokens_in: u64, tokens_out: u64) -> Submission {
        Submission {
            file: submission("report").file,
            model_calls: 1,
            input_tokens: tokens_in,
            output_tokens: tokens_out,
            cost_nano_usd: charged(tokens_in, tokens_out),
            final_offer: true,
        }
    }

    #[test]
    fn the_usd_ceiling_is_exact_and_a_tasks_cap_defaults_to_it() {
        let with = |extra: &[&str]| parse(&[&base_args()[..], extra].concat());
        let bare = |extra: &[&str]| parse(&[&bare_args()[..], extra].concat());
        let capped = bare(&["--max-usd-per-problem", "20"]).unwrap();
        assert_eq!(capped.max_usd_micro_per_problem, 20_000_000);
        let limits = Limits::from_task(
            TaskLimits { max_tokens_per_problem: Some(500_000), ..TaskLimits::default() }
                .resolve(&capped.ceilings())
                .unwrap(),
        );
        assert_eq!(
            limits,
            Limits {
                w: 2,
                k1: 12,
                fanout: 3,
                calls_per_continuation: 3,
                collection_calls: 0,
                model_calls: 8,
                max_tokens: 500_000,
                max_usd_micro: 20_000_000,
            },
            "the task's cap defaults to the ceiling"
        );
        let free = Limits::from_task(TaskLimits::default().resolve(&with(&[]).unwrap().ceilings()).unwrap());
        assert!(free.cap_reached(&spend(u64::MAX, 0)).is_none(), "uncapped only under --no-usd-ceiling");
    }

    /// One round of `limits` in which each report is a task of the root that
    /// submitted it.
    fn round_of(limits: Limits, reports: &[Submission]) -> (Rollout, RoundEnd) {
        let mut r = rollout(0.5, limits);
        r.plan_round(&mut InitialPolicy).unwrap();
        for (k, submission) in reports.iter().enumerate() {
            let (task, process) = (format!("t{k}"), ProcessId(11 + k as u64));
            r.open_task(&task, 0, process).unwrap();
            let res = ResourcePath::new(["run", "demo", task.as_str()]).unwrap();
            r.accept_submission(&res, process, submission.clone()).unwrap();
            r.record_submission(&task, submission);
        }
        let end = r.finish_round();
        (r, end)
    }

    /// The caps are spent against the tokens and the cost the providers
    /// reported, never a price table; a missing cost is unknown, which
    /// neither stops exploration nor bypasses the other budgets.
    #[test]
    fn the_caps_are_spent_against_the_reported_spend_and_missing_cost_is_unknown() {
        let run = Spend { calls: 1, tokens_in: 10, tokens_out: 20, cost_nano: 740_000_000 };
        assert_eq!(run.usd_micro(), Some(740_000));
        assert_eq!(Spend { cost_nano: 1, ..run }.usd_micro(), Some(1), "a sub-micro call is not free");
        assert_eq!(Spend { cost_nano: 0, ..run }.usd_micro(), None, "no report is not zero");

        // Two reports of 10 + 20 tokens: 60 tokens and 100 micro-USD.
        let two = [report(10, 20), report(10, 20)];
        let unreported = [Submission { cost_nano_usd: 0, ..report(10, 20) }];
        let one = |limits: Limits| Limits { w: 1, max_usd_micro: 100, ..limits };
        let cases: [(Limits, &[Submission], Option<Stop>); 7] = [
            (Limits { max_tokens: 60, ..limits() }, &two, Some(Stop::CostCap(Cap::Tokens))),
            (Limits { max_tokens: 61, ..limits() }, &two, None),
            (Limits { max_usd_micro: 100, ..limits() }, &two, Some(Stop::CostCap(Cap::Usd))),
            (Limits { max_usd_micro: 101, ..limits() }, &two, None),
            (one(limits()), &unreported, None),
            (one(Limits { max_tokens: 30, ..limits() }), &unreported, Some(Stop::CostCap(Cap::Tokens))),
            (one(Limits { model_calls: 8, ..limits() }), &unreported, Some(Stop::ModelBudget)),
        ];
        for (i, (limits, reports, expected)) in cases.into_iter().enumerate() {
            let (mut r, end) = round_of(limits, reports);
            assert_eq!(end.usd_micro.is_none(), reports[0].cost_nano_usd == 0, "case {i}");
            assert_eq!(r.plan_round(&mut InitialPolicy).err(), expected, "case {i}");
        }

        // The round record carries the reported spend, and an unknown cost as unknown.
        let record = |end: &RoundEnd| RoundRecord::from_end(end, &PolicySource::Builtin.record());
        let (_, end) = round_of(limits(), &two);
        let last = record(&end).closed(&Stop::CostCap(Cap::Usd));
        let json = serde_json::to_value(&last).unwrap();
        assert_eq!(json["reason"], "usd_cap");
        assert_eq!(
            (&json["usd"], &json["tokens_in"], &json["tokens_out"]),
            (&0.0001.into(), &20.into(), &40.into())
        );
        assert_eq!(serde_json::from_value::<RoundRecord>(json).unwrap(), last);
        let (_, end) = round_of(one(limits()), &unreported);
        let json = serde_json::to_value(record(&end)).unwrap();
        assert!(json.get("usd").is_none_or(serde_json::Value::is_null), "unknown is not free");
        // A record without the spend fields reads back as zero spend, no estimate.
        let old: RoundRecord =
            serde_json::from_str(r#"{"v":1,"round":1,"batch":[0],"children":[1]}"#).unwrap();
        assert_eq!((old.tokens_in, old.tokens_out, old.usd), (0, 0, None));

        // A worker that exits without submitting leaves its tokens unknown: not
        // counted, while its calls stay charged.
        let mut silent = rollout(0.5, Limits { max_tokens: 1, ..limits() });
        silent.plan_round(&mut InitialPolicy).unwrap();
        silent.open_task("t1", 0, ProcessId(11)).unwrap();
        assert_eq!(silent.worker_exited(ProcessId(11)), Some(("t1".to_owned(), false)));
        assert_eq!(silent.spent(), Spend { calls: 8, tokens_in: 0, tokens_out: 0, cost_nano: 0 });
        silent.finish_round();
        assert!(silent.plan_round(&mut InitialPolicy).is_ok());
        // Solved wins over the caps: a proof is a proof.
        let mut solved = rollout(1.0, Limits { max_tokens: 1, max_usd_micro: 1, ..limits() });
        assert_eq!(solved.plan_round(&mut InitialPolicy), Err(Stop::Solved));
    }

    const TEMPLATE: &str = "{{ statement }}\n{% if mode == 'continue' %}continue{% endif %}";

    pub(super) fn rollout(root_score: Score, limits: Limits) -> Rollout {
        let statement = "import Mathlib\ntheorem demo : True := by\n  sorry\n".to_owned();
        Rollout::new("demo", statement, TEMPLATE.to_owned(), eval(root_score), limits)
    }

    /// The 0th child of a node improves most,
    /// later children barely.
    pub(super) fn grow(tree: &mut ProblemTree, parent: NodeId, round: u32, k: usize) -> NodeId {
        let base = tree.node(parent).unwrap().score();
        let bump = if k == 0 { 0.3 } else { 0.01 * (k as f64) };
        let file = format!("file of child {k} of {parent}");
        tree.push(parent, round, format!("t{parent}{k}"), file, eval(base + bump), None).unwrap()
    }

    #[test]
    fn later_rounds_take_the_best_leaves_with_deterministic_tie_breaks() {
        let mut tree = ProblemTree::new("root".into(), eval(0.1), 3);
        let a = grow(&mut tree, 0, 1, 0); // 0.4
        let b = grow(&mut tree, 0, 1, 1); // 0.11
        let c = grow(&mut tree, 0, 1, 2); // 0.12
        let deep = grow(&mut tree, a, 2, 1); // 0.41, depth 2 — a is no longer a leaf
        let view = tree.view();
        assert_eq!(view.continuable, [deep, c, b, 0], "score desc; a is not continuable");
        assert_eq!(InitialPolicy.decide_with_parents(&view, &tree.parents(), 2), [deep, c]);
        // Ties: the same score at two depths → shallower first; same depth → lower id first.
        let mut tied = ProblemTree::new("root".into(), eval(0.5), 3);
        let x = grow(&mut tied, 0, 1, 0); // 0.8
        let y = tied.push(0, 1, "ty".into(), "y".into(), eval(0.8), None).unwrap();
        let z = tied.push(x, 2, "tz".into(), "z".into(), eval(0.8), None).unwrap();
        assert_eq!(tied.view().continuable, [y, z, 0]);
    }

    #[test]
    fn the_initial_policy_refuses_only_invalid_reserves_a_slot_for_a_repair_and_keeps_the_root() {
        const TIMEOUT: &str = "(deterministic) timeout at `whnf`, maximum number of heartbeats";
        let repairable = broken(&["unsolved goals\n⊢ n = n", "Type mismatch"]);
        assert!(repairable.errors_obs.as_ref().is_some_and(|o| o.repairable));
        // A timeout is repairable; what is hopeless is a pile of errors.
        let timed_out = broken(&[TIMEOUT]);
        assert!(timed_out.errors_obs.as_ref().is_some_and(|o| o.repairable));
        let hopeless = broken(&["a", "b", "c", "d", "e"]);
        assert!(hopeless.errors_obs.as_ref().is_some_and(|o| !o.repairable));
        // Round 2 of a W = 3 rollout: the root's children, by score, a credited
        // sketch (0.7), a hollow one (0.5, like the root), two errors nodes
        // (0.05, one repairable), an invalid one (0.0).
        let mut tree = ProblemTree::new("root".into(), eval(0.5), 3);
        let credited = tree.push(0, 1, "t1".into(), "a".into(), sketched(1, 1, false), None).unwrap();
        let hollow = tree.push(0, 1, "t2".into(), "b".into(), sketched(3, 1, true), None).unwrap();
        let repair = tree.push(0, 1, "t3".into(), "c".into(), repairable, None).unwrap();
        let timeout = tree.push(0, 1, "t4".into(), "d".into(), hopeless, None).unwrap();
        let invalid = Evaluation::from_verdicts(check(CheckStatus::Invalid), None, None);
        let invalid = tree.push(0, 1, "t5".into(), "e".into(), invalid, None).unwrap();
        let (view, parents) = (tree.view(), tree.parents());
        assert_eq!(view.continuable, [credited, 0, hollow, repair, timeout, invalid]);
        assert_eq!(
            InitialPolicy.decide_with_parents(&view, &parents, 3),
            [repair, credited, 0],
            "the repairable child first, then the ordering"
        );
        assert_eq!(
            InitialPolicy.decide_with_parents(&view, &parents, 5),
            [repair, credited, 0, hollow, timeout],
            "invalid never; a hollow leaf ranks by its score like any other (v3); each node once"
        );
        // Without a repairable errors leaf, the slot goes to the sorries leaf
        // with the most credited steps, whatever its score.
        let mut sketches = ProblemTree::new("root".into(), eval(0.5), 3);
        let one = sketches.push(0, 1, "t1".into(), "a".into(), sketched(1, 3, false), None).unwrap(); // 0.6
        let two = sketches.push(0, 1, "t2".into(), "b".into(), sketched(2, 7, false), None).unwrap(); // 0.5889
        // Too many errors to be a repair candidate, timeout or not.
        sketches.push(0, 1, "t3".into(), "c".into(), broken(&["a", "b", "c", "d", TIMEOUT]), None).unwrap();
        let (view2, parents2) = (sketches.view(), sketches.parents());
        assert_eq!(InitialPolicy.decide_with_parents(&view2, &parents2, 2), [two, one]);
        // Round 1 is still W copies of the root, whatever the root's own sketch says.
        let alone = ProblemTree::new("root".into(), sketched(0, 1, true), 3);
        let (view3, parents3) = (alone.view(), alone.parents());
        assert_eq!(InitialPolicy.decide_with_parents(&view3, &parents3, 3), [0, 0, 0]);

        // pi0.py decides the same over the same views.
        let python = python3();
        let dir = scratch("pi0v2");
        let mut process = process_policy(&python, &pi0_script(), &dir);
        for (view, parents, w) in
            [(&view, &parents, 3), (&view, &parents, 5), (&view2, &parents2, 2), (&view3, &parents3, 3)]
        {
            let expected = InitialPolicy.decide_with_parents(view, parents, w);
            assert_eq!(process.decide_with_parents(view, parents, w), expected, "w {w}");
        }
        assert_eq!(process.failure(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fanout_caps_copies_of_a_leaf_but_never_the_root() {
        let mut tree = ProblemTree::new("root".into(), eval(0.1), 2);
        let leaf = grow(&mut tree, 0, 1, 0);
        assert_eq!(tree.admit(&[leaf, leaf, leaf, leaf], 10), [leaf, leaf], "at most fanout copies");
        assert_eq!(tree.admit(&[0, 0, 0, 0, 0], 10), [0, 0, 0, 0, 0], "the root is exempt");
        assert_eq!(tree.admit(&[leaf, 0, leaf, 0, leaf], 3), [leaf, 0, leaf], "w bounds the batch");
        grow(&mut tree, leaf, 2, 0);
        assert!(!tree.is_continuable(leaf), "a node with a child is not a leaf");
        assert_eq!(tree.admit(&[leaf, 42], 10), Vec::<NodeId>::new(), "not continuable, unknown");
    }

    #[test]
    fn the_loop_stops_when_solved_after_k1_rounds_or_when_the_budget_is_spent() {
        let mut solved = rollout(1.0, limits());
        assert_eq!(solved.plan_round(&mut InitialPolicy), Err(Stop::Solved));

        let mut short = rollout(0.5, Limits { k1: 1, ..limits() });
        assert!(short.plan_round(&mut InitialPolicy).is_ok());
        short.finish_round();
        assert_eq!(short.plan_round(&mut InitialPolicy), Err(Stop::Rounds));

        // Budget 17, grant 3 + 5: round 1 affords two workers (w narrows from 3), round 2 none.
        let mut poor = rollout(0.5, Limits { model_calls: 17, ..limits() });
        assert_eq!(poor.plan_round(&mut InitialPolicy).unwrap(), [0, 0]);
        poor.open_task("t1", 0, ProcessId(11)).unwrap();
        poor.open_task("t2", 0, ProcessId(12)).unwrap();
        assert_eq!(poor.spent_calls(), 16);
        poor.finish_round();
        assert_eq!(poor.plan_round(&mut InitialPolicy), Err(Stop::ModelBudget));

        // A reported usage below the grant refunds the difference (clamped to the grant).
        let mut thrifty = rollout(0.5, Limits { model_calls: 17, ..limits() });
        thrifty.plan_round(&mut InitialPolicy).unwrap();
        thrifty.open_task("t1", 0, ProcessId(11)).unwrap();
        let res = ResourcePath::new(["run", "demo", "t1"]).unwrap();
        let sub = Submission {
            file: submission("thrifty").file,
            model_calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            cost_nano_usd: 0,
            final_offer: true,
        };
        thrifty.accept_submission(&res, ProcessId(11), sub.clone()).unwrap();
        thrifty.record_submission("t1", &sub);
        assert_eq!(thrifty.spent_calls(), 1);
        let mut liar = rollout(0.5, limits());
        liar.plan_round(&mut InitialPolicy).unwrap();
        liar.open_task("t1", 0, ProcessId(11)).unwrap();
        let sub = Submission {
            file: submission("liar").file,
            model_calls: 999,
            input_tokens: 0,
            output_tokens: 0,
            cost_nano_usd: 0,
            final_offer: true,
        };
        liar.accept_submission(&res, ProcessId(11), sub.clone()).unwrap();
        liar.record_submission("t1", &sub);
        assert_eq!(liar.spent_calls(), 8, "over-reporting is clamped to the grant");

        struct Quit;
        impl Policy for Quit {
            fn decide_with_parents(&mut self, _: &View, _: &[Option<NodeId>], _: usize) -> Vec<NodeId> {
                vec![]
            }
        }
        assert_eq!(rollout(0.5, limits()).plan_round(&mut Quit), Err(Stop::PolicyStopped));
    }

    #[test]
    fn a_solve_ends_the_round_it_happened_in_rather_than_only_the_next_one() {
        // A solve stops its round-mates at once: nothing would continue their
        // work, so letting them run to their caps only spends.
        let mut r = rollout(0.5, Limits { w: 3, ..limits() });
        assert_eq!(r.plan_round(&mut InitialPolicy).unwrap(), [0, 0, 0]);
        for (task, pid) in [("t1", 11), ("t2", 12), ("t3", 13)] {
            r.open_task(task, 0, ProcessId(pid)).unwrap();
        }
        let res = ResourcePath::new(["run", "demo", "t1"]).unwrap();
        r.accept_submission(&res, ProcessId(11), submission("f")).unwrap();
        // Accepting validates; recording is what marks the task submitted,
        // because the service runs Lean in between.
        r.record_submission("t1", &submission("f"));
        assert!(!r.round_done());
        // The two others. t1, whose proof was verified, ends by itself once
        // it has the verdict, and the round waits for its exit.
        assert_eq!(r.stop_for_solve("t1"), [ProcessId(12), ProcessId(13)]);
        assert!(!r.round_done(), "the solver has its verdict still to take");
        r.worker_exited(ProcessId(11));
        assert!(r.round_done());
        let end = r.finish_round();
        assert_eq!(
            end.log.failed,
            [
                Failed { task: "t2".into(), why: "solved_elsewhere".into() },
                Failed { task: "t3".into(), why: "solved_elsewhere".into() }
            ],
            "cut, not failed on their own account"
        );
        assert!(
            !provider_failure_round(&end.log),
            "a cut round is not a provider outage, whatever its failures say"
        );
    }

    #[test]
    fn a_round_whose_clock_ran_out_fails_its_unsubmitted_continuations_as_timed_out() {
        let mut r = rollout(0.5, Limits { w: 3, ..limits() });
        assert_eq!(r.plan_round(&mut InitialPolicy).unwrap(), [0, 0, 0]);
        for (task, pid) in [("t1", 11), ("t2", 12), ("t3", 13)] {
            r.open_task(task, 0, ProcessId(pid)).unwrap();
        }
        let res = ResourcePath::new(["run", "demo", "t1"]).unwrap();
        r.accept_submission(&res, ProcessId(11), submission("f")).unwrap();
        r.record_submission("t1", &submission("f"));
        assert!(!r.round_done(), "two workers have neither submitted nor exited");
        assert_eq!(
            r.time_out(),
            [ProcessId(11), ProcessId(12), ProcessId(13)],
            "all three are killed; only the two that never answered are failed"
        );
        assert!(r.round_done(), "the round closes");
        // The late workers' exit events, arriving afterwards, do not fail them twice.
        assert_eq!(r.worker_exited(ProcessId(12)), Some(("t2".into(), false)));
        let end = r.finish_round();
        assert_eq!(
            end.log.failed,
            [
                Failed { task: "t2".into(), why: "timed_out".into() },
                Failed { task: "t3".into(), why: "timed_out".into() }
            ]
        );
        assert!(end.stragglers.is_empty(), "the timeout already closed every worker of the round");
        assert_eq!(r.spent_calls(), 2 + 8 + 8, "the timed-out grants stay charged in full");
        // A late dispatch from a timed-out worker is refused.
        let late = ResourcePath::new(["run", "demo", "t3"]).unwrap();
        assert_eq!(
            r.accept_submission(&late, ProcessId(13), submission("g")).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }

    // --- submissions, once ---

    /// A submission the harness will accept: the tests' frozen statement
    /// verbatim (the harness refuses a file that does not reproduce it), with
    /// `tag` marking which submission this is.
    pub(super) fn submission(tag: &str) -> Submission {
        Submission {
            file: format!("import Mathlib\ntheorem demo : True := by\n  trivial -- {tag}\n"),
            model_calls: 2,
            input_tokens: 10,
            output_tokens: 20,
            cost_nano_usd: 0,
            final_offer: true,
        }
    }

    pub(super) fn payload<T: Serialize>(v: &T) -> Payload {
        Payload::encode(v).unwrap()
    }

    /// A continuation inherits its parent's transcript, which ends with the
    /// parent deciding to submit, so handing the parent's file back is
    /// refused: it would spend a round on a node that already exists.
    #[test]
    fn a_continuation_may_not_hand_its_parents_file_back_unchanged() {
        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        let t1 = ResourcePath::new(["run", "demo", "t000001"]).unwrap();
        let parent_file = r.assignment(&t1, ProcessId(7)).unwrap().file;

        let copy = Submission { file: parent_file.clone(), final_offer: false, ..submission("x") };
        assert_eq!(
            r.accept_submission(&t1, ProcessId(7), copy.clone()).unwrap_err().code,
            ErrorCode::InvalidInput
        );

        // Refused, not spent: the task can still submit, and one changed byte
        // is enough to make it a different node.
        let changed = Submission { file: format!("{parent_file}-- one more line\n"), ..copy.clone() };
        assert!(matches!(r.accept_submission(&t1, ProcessId(7), changed).unwrap(), Served::Accepted { .. }));

        // A worker's last word is no exception: a copy is not a node.
        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        let last = Submission { final_offer: true, ..copy };
        assert_eq!(r.accept_submission(&t1, ProcessId(7), last).unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn a_task_takes_one_submission_and_is_bound_to_its_worker() {
        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        r.open_task("t000002", 0, ProcessId(8)).unwrap();
        let t1 = ResourcePath::new(["run", "demo", "t000001"]).unwrap();

        // Assignment: the root's file, its check, no siblings yet, the worker's model budget.
        let a = r.assignment(&t1, ProcessId(7)).unwrap();
        assert_eq!((a.problem.as_str(), a.task.as_str(), a.parent, a.model_calls), ("demo", "t000001", 0, 3));
        assert_eq!(a.file, a.statement);
        assert_eq!(a.check.as_ref().map(|c| c.status), Some(CheckStatus::Sorries));
        assert!(a.siblings.is_empty());
        assert_eq!(a.prompt_template, TEMPLATE, "the run's template, verbatim");

        // Another process cannot act on this task, even though the kernel let its call through.
        assert_eq!(r.assignment(&t1, ProcessId(8)).unwrap_err().code, ErrorCode::PermissionDenied);
        // An unknown task, a foreign problem, and a malformed resource are refused.
        let unknown = ResourcePath::new(["run", "demo", "t000009"]).unwrap();
        assert_eq!(r.assignment(&unknown, ProcessId(7)).unwrap_err().code, ErrorCode::InvalidInput);
        let foreign = ResourcePath::new(["run", "other", "t000001"]).unwrap();
        assert_eq!(r.assignment(&foreign, ProcessId(7)).unwrap_err().code, ErrorCode::InvalidInput);
        let flat = ResourcePath::new(["t000001"]).unwrap();
        assert_eq!(r.assignment(&flat, ProcessId(7)).unwrap_err().code, ErrorCode::InvalidInput);

        let first = r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(7), &payload(&submission("first")));
        let Served::Accepted { task, parent, process, submission: sub } = first else { panic!("{first:?}") };
        assert_eq!((task.as_str(), parent, process), ("t000001", 0, ProcessId(7)));
        assert!(sub.file.contains("-- first"), "{}", sub.file);
        r.record_submission("t000001", &sub);
        let second = r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(7), &payload(&submission("second")));
        assert!(matches!(second, Served::Reply(Err(_))), "an accepted submission is terminal: {second:?}");
        // A submission from the wrong worker, or with an unknown field, is refused.
        let wrong = r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(8), &payload(&submission("x")));
        assert!(matches!(wrong, Served::Reply(Err(e)) if e.code == ErrorCode::PermissionDenied));
        let junk =
            r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(7), &payload(&serde_json::json!({"file": "x"})));
        assert!(matches!(junk, Served::Reply(Err(e)) if e.code == ErrorCode::InvalidInput));
        assert!(matches!(
            r.serve("steal", &t1, ProcessId(7), &payload(&serde_json::json!({}))),
            Served::Reply(Err(e)) if e.code == ErrorCode::InvalidInput
        ));

        // The evaluated child joins the tree and the next assignment from the root lists it.
        let id = r.add_child(0, "t000001", ProcessId(7), &submission("first"), eval(0.05)).unwrap();
        assert_eq!(id, 1);
        r.record_submission("t000001", &submission("first"));
        let third = r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(7), &payload(&submission("third")));
        assert!(
            matches!(third, Served::Reply(Err(_))),
            "a terminal submission cannot create a second node: {third:?}"
        );
        let t2 = ResourcePath::new(["run", "demo", "t000002"]).unwrap();
        let a2 = r.assignment(&t2, ProcessId(8)).unwrap();
        assert_eq!(a2.siblings.len(), 1);
        assert_eq!((a2.siblings[0].node, a2.siblings[0].check.status), (1, CheckStatus::Errors));
        assert!(a2.siblings[0].excerpt.contains("-- first"), "{}", a2.siblings[0].excerpt);

        // The round is done once every worker has *exited*. Submitting does
        // not end a worker's part in it.
        assert!(!r.round_done());
        assert_eq!(r.worker_exited(ProcessId(8)), Some(("t000002".into(), false)));
        assert!(!r.round_done(), "t000001 has answered but is still working");
        assert_eq!(r.worker_exited(ProcessId(7)), Some(("t000001".into(), true)));
        assert!(r.round_done());
        assert_eq!(r.worker_exited(ProcessId(99)), None, "not one of ours");
        let end = r.finish_round();
        assert_eq!(end.round, 1);
        assert_eq!((end.log.batch.as_slice(), end.log.children.as_slice()), (&[0, 0, 0][..], &[1][..]));
        assert_eq!(
            end.log.failed,
            [Failed { task: "t000002".into(), why: "exited_without_submitting".into() }]
        );
        assert!(end.stragglers.is_empty(), "both workers exited before the round closed");
        // After the round, its tasks are closed.
        assert_eq!(r.assignment(&t1, ProcessId(7)).unwrap_err().code, ErrorCode::InvalidInput);
        assert!(matches!(
            r.serve(explore::ACTION_SUBMIT, &t1, ProcessId(7), &payload(&submission("late"))),
            Served::Reply(Err(e)) if e.code == ErrorCode::InvalidInput
        ));
    }

    #[test]
    fn a_solved_child_stops_the_next_round_and_its_journal_line_is_complete() {
        let mut r = rollout(0.5, limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        let id = r.add_child(0, "t000001", ProcessId(7), &submission("proof"), eval(1.0)).unwrap();
        r.finish_round();
        assert_eq!(r.plan_round(&mut InitialPolicy), Err(Stop::Solved));

        let record = r.node_record(id, 1234).unwrap();
        let text = serde_json::to_string(&record).unwrap();
        let back: NodeRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, record);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!((&json["id"], &json["parent"], &json["round"]), (&1.into(), &0.into(), &1.into()));
        assert_eq!(json["task"], "t000001");
        assert_eq!(json["score"], 1.0);
        assert_eq!(json["verified"], true);
        assert_eq!(json["worker"]["process"], 7);
        assert_eq!(json["worker"]["model_calls"], 2);
        assert_eq!(json["ms_since_start"], 1234);
        assert!(json.get("verify_rejection").is_none());
        let root = r.node_record(0, 0).unwrap();
        assert_eq!((root.parent, root.round, root.task, root.worker), (None, 0, None, None));
        let round = serde_json::to_value(
            RoundRecord::empty(1, &PolicySource::Builtin.record()).closed(&Stop::Solved),
        )
        .unwrap();
        assert_eq!(round["reason"], "solved");
        assert_eq!(round["policy"], serde_json::json!({"kind": "builtin", "program": null}));
        assert!(round.get("failed").is_none(), "empty lists are omitted");
        assert!(round.get("policy_error").is_none(), "only a policy failure carries an error");
        assert_eq!(Stop::Failed("x".into()).reason(), "failed: x");
    }

    #[test]
    fn the_journal_is_append_only_and_refuses_a_second_tree_for_a_problem() {
        let dir = scratch("journal");
        let mut journal = Journal::open(&dir.join("demo")).unwrap();
        let builtin = PolicySource::Builtin.record();
        let refused = Stop::StatementRejected("refused".into());
        journal.round(&RoundRecord::empty(0, &builtin).closed(&refused)).unwrap();
        // Ended before it had a root: the record stays, and a tree follows it.
        let mut again = Journal::open(&dir.join("demo")).unwrap();
        again.round(&RoundRecord::empty(1, &builtin)).unwrap();
        let lines = std::fs::read_to_string(dir.join("demo/rounds.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 2);
        writeln!(again.nodes, "{{}}").unwrap();
        assert!(Journal::open(&dir.join("demo")).is_err(), "a tree for demo already exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_excerpt_starts_where_the_child_diverges_from_its_parent() {
        assert_eq!(excerpt("a\nb\nc", "a\nb\nd\ne"), "d\ne");
        assert_eq!(excerpt("same", "same"), "same", "the head when nothing differs");
        let long = format!("x\n{}", "y".repeat(500));
        assert_eq!(excerpt("x", &long).chars().count(), EXCERPT_CHARS);
    }

    // --- the exploration policy as a process ---

    fn pi0_script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("policies/pi0.py")
            .canonicalize()
            .expect("program-files/discovery/policies/pi0.py")
    }

    /// The interpreter the process tests run under: the default one when it
    /// is there, else the first `python3` on `PATH`. Without one they fail.
    fn python3() -> PathBuf {
        let default = PathBuf::from(DEFAULT_POLICY_INTERPRETER);
        if default.is_file() {
            return default;
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|p| p.is_file())
            .expect("the policy process tests need python3")
    }

    fn process_policy(python: &Path, program: &Path, cwd: &Path) -> RolloutPolicy {
        let source = PolicySource::Process {
            program: program.to_path_buf(),
            interpreter: python.to_path_buf(),
            timeout: Duration::from_secs(20),
        };
        RolloutPolicy::start(&source, cwd).unwrap()
    }

    /// Drive `plan` over a synthetic rollout the way the harness runs a round:
    /// every task of the batch is opened first (a leaf continued twice is still
    /// a leaf while its workers run), then each yields a child. Child `k` of a
    /// node scores its parent's score + 0.3 (`k` = 0) or + 0.01·k, capped at
    /// 0.95 so nothing is ever "solved". Returns every round's admitted batch
    /// and the stop.
    fn simulate(
        root: Score,
        limits: Limits,
        mut plan: impl FnMut(&mut Rollout) -> Result<Vec<NodeId>, Stop>,
    ) -> (Vec<Vec<NodeId>>, Stop) {
        let mut r = rollout(root, limits);
        let mut batches = Vec::new();
        let mut next_process: u64 = 100;
        let stop = loop {
            let batch = match plan(&mut r) {
                Ok(batch) => batch,
                Err(stop) => break stop,
            };
            batches.push(batch.clone());
            let mut workers = Vec::new();
            for &parent in &batch {
                next_process += 1;
                let (task, process) = (format!("t{next_process}"), ProcessId(next_process));
                r.open_task(&task, parent, process).unwrap();
                workers.push((task, process, parent));
            }
            for (task, process, parent) in workers {
                let (k, base) = {
                    let node = r.tree().node(parent).unwrap();
                    (node.children.len(), node.score())
                };
                let bump = if k == 0 { 0.3 } else { 0.01 * (k as f64) };
                let score = (base + bump).min(0.95);
                let file = format!("child {k} of {parent}");
                r.add_child(parent, &task, process, &submission(&file), eval(score)).unwrap();
            }
            r.finish_round();
        };
        (batches, stop)
    }

    #[test]
    fn a_task_policy_selects_the_builtin_or_a_process_policy() {
        let options = parse(&[base_args(), vec!["--policy-interpreter", TEST_INTERPRETER]].concat()).unwrap();
        let builtin = options.policy_source(None);
        assert_eq!(builtin, PolicySource::Builtin);
        assert_eq!(builtin.program(Path::new("/out/run")), None);
        assert_eq!(
            serde_json::to_value(builtin.record()).unwrap(),
            serde_json::json!({"kind": "builtin", "program": null})
        );

        // A `.py` program runs under the interpreter, in the run directory,
        // with no environment and no argument but itself.
        let source = options.policy_source(Some("/policies/pi7.py".into()));
        assert_eq!(
            source,
            PolicySource::Process {
                program: "/policies/pi7.py".into(),
                interpreter: TEST_INTERPRETER.into(),
                timeout: Duration::from_millis(DEFAULT_POLICY_TIMEOUT_MS),
            }
        );
        assert_eq!(
            source.program(Path::new("/out/run")),
            Some(PolicyProgram {
                program: TEST_INTERPRETER.into(),
                args: vec!["/policies/pi7.py".into()],
                cwd: Some("/out/run".into()),
                env: vec![],
                timeout: Duration::from_secs(5),
            })
        );
        assert_eq!(
            serde_json::to_value(source.record()).unwrap(),
            serde_json::json!({"kind": "process", "program": "/policies/pi7.py"})
        );

        // Anything else executes directly; interpreter and timeout are options.
        let mut args = base_args();
        args.extend(["--policy-interpreter", TEST_INTERPRETER, "--policy-timeout-ms", "250"]);
        let tuned = parse(&args).unwrap();
        assert_eq!(
            tuned.policy_source(Some("/policies/pi-bin".into())).program(Path::new("/out/run")),
            Some(PolicyProgram {
                program: "/policies/pi-bin".into(),
                args: vec![],
                cwd: Some("/out/run".into()),
                env: vec![],
                timeout: Duration::from_millis(250),
            })
        );
        let under =
            tuned.policy_source(Some("/policies/pi7.py".into())).program(Path::new("/out/run")).unwrap();
        assert_eq!(
            (under.program.as_path(), under.args.as_slice()),
            (Path::new(TEST_INTERPRETER), &["/policies/pi7.py".to_owned()][..])
        );

        // A program that is not there, or an interpreter that is not, cannot be started.
        assert!(PolicySource::Builtin.check().is_ok());
        let missing = PolicySource::Process {
            program: "/nonexistent/pi.py".into(),
            interpreter: TEST_INTERPRETER.into(),
            timeout: Duration::from_secs(1),
        };
        assert!(missing.check().unwrap_err().contains("/nonexistent/pi.py"));
        let dir = scratch("missing_interpreter");
        let script = dir.join("policy.py");
        std::fs::write(&script, "fixture").unwrap();
        let no_interpreter = PolicySource::Process {
            program: script,
            interpreter: dir.join("missing-python"),
            timeout: Duration::from_secs(1),
        };
        assert!(no_interpreter.check().unwrap_err().contains("--policy-interpreter"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_policy_process_running_pi0_plans_the_same_rounds_as_the_builtin_policy() {
        let python = python3();
        let dir = scratch("pi0");
        let pi0 = pi0_script();
        let cases = [
            (0.1, limits()),
            (0.5, Limits { w: 2, fanout: 2, ..limits() }),
            (0.1, Limits { w: 4, k1: 3, fanout: 1, ..limits() }),
        ];
        for (root, limits) in cases {
            let mut builtin = RolloutPolicy::start(&PolicySource::Builtin, &dir).unwrap();
            let expected = simulate(root, limits, |r| r.plan_round_with(&mut builtin));
            let mut process = process_policy(&python, &pi0, &dir);
            let actual = simulate(root, limits, |r| r.plan_round_with(&mut process));
            assert_eq!(actual, expected, "round by round, root {root}, {limits:?}");
            assert_eq!(expected.1, Stop::Rounds);
            assert!(expected.0.len() >= 3, "a meaningful run: {expected:?}");
            assert_eq!(process.failure(), None);
            let RolloutPolicy::Process(p) = &process else { panic!("a process policy") };
            assert_eq!(p.rounds(), expected.0.len() as u64, "one request per round");
        }
        // The first case, spelled out: W copies of the root, then the best W
        // continuable nodes by (score desc, depth asc, id asc).
        let mut builtin = RolloutPolicy::Builtin(InitialPolicy);
        let (batches, _) = simulate(0.1, limits(), |r| r.plan_round_with(&mut builtin));
        assert_eq!(batches, [vec![0, 0, 0], vec![1, 3, 2], vec![4, 5, 6], vec![7, 8, 9]]);
        // Ties on score at different depths, the deeper node holding the lower
        // id, so (score, id) and (score, depth, id) orders differ: the two
        // decide alike as the tree grows node by node.
        let mut tied = ProblemTree::new("root".into(), eval(0.0), 3);
        let mut process = process_policy(&python, &pi0, &dir);
        for (parent, score) in
            [(0, 0.5), (1, 0.5), (0, 0.5), (2, 0.5), (3, 0.9), (4, 0.5), (5, 0.95), (6, 0.5)]
        {
            tied.push(parent, 1, format!("t{parent}"), format!("child of {parent}"), eval(score), None)
                .unwrap();
            let (view, parents) = (tied.view(), tied.parents());
            let expected = InitialPolicy.decide_with_parents(&view, &parents, 1);
            assert_eq!(process.decide_with_parents(&view, &parents, 1), expected, "{view:?}");
        }
        assert_eq!(process.failure(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ten copies of the best continuable node, whatever `w` says.
    const GREEDY_PY: &str = "import json, sys\n\
        for line in iter(sys.stdin.readline, \"\"):\n\
        \x20   req = json.loads(line)\n\
        \x20   sys.stdout.write(json.dumps({\"batch\": [req[\"continuable\"][0]] * 10}) + \"\\n\")\n\
        \x20   sys.stdout.flush()\n";

    /// The same rule in Rust.
    struct Greedy;

    impl Policy for Greedy {
        fn decide_with_parents(&mut self, view: &View, _: &[Option<NodeId>], _: usize) -> Vec<NodeId> {
            vec![view.continuable[0]; 10]
        }
    }

    #[test]
    fn the_fanout_cap_and_width_apply_to_a_policy_process_exactly_as_to_a_rust_policy() {
        let python = python3();
        let dir = scratch("greedy");
        let script = dir.join("greedy.py");
        std::fs::write(&script, GREEDY_PY).unwrap();
        let limits = Limits { w: 3, k1: 4, fanout: 2, ..limits() };
        let rust = simulate(0.1, limits, |r| r.plan_round(&mut Greedy));
        let mut process = process_policy(&python, &script, &dir);
        let python_side = simulate(0.1, limits, |r| r.plan_round_with(&mut process));
        assert_eq!(python_side, rust);
        // Round 1: the root is exempt from the cap and `w` bounds the batch;
        // later rounds: at most `fanout` copies of a leaf, however many asked.
        assert_eq!(rust.0, [vec![0, 0, 0], vec![1, 1], vec![4, 4], vec![6, 6]]);
        assert_eq!(rust.1, Stop::Rounds);
        assert_eq!(process.failure(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_policy_process_stops_the_rollout_with_a_recorded_error_not_a_fallback() {
        let python = python3();
        let dir = scratch("quit");
        let script = dir.join("quit.py");
        std::fs::write(&script, "import sys\nsys.exit(3)\n").unwrap();
        let source = PolicySource::Process {
            program: script.clone(),
            interpreter: python,
            timeout: Duration::from_secs(20),
        };
        let mut policy = RolloutPolicy::start(&source, &dir).unwrap();
        let mut r = rollout(0.5, limits());
        let stop = r.plan_round_with(&mut policy).unwrap_err();
        let Stop::PolicyFailed(why) = &stop else { panic!("expected policy_failed, got {stop:?}") };
        assert!(why.contains("round 1"), "{why}");
        assert_eq!(r.round(), 0, "no round was planned: the built-in policy did not step in");
        assert_eq!(policy.failure().as_deref(), Some(why.as_str()));
        // A failed process stays failed: the next plan is the same stop, still no fallback.
        assert!(matches!(r.plan_round_with(&mut policy), Err(Stop::PolicyFailed(_))));

        let record = serde_json::to_value(RoundRecord::empty(0, &source.record()).closed(&stop)).unwrap();
        assert_eq!(record["reason"], "policy_failed");
        assert_eq!(record["policy_error"], serde_json::Value::String(why.clone()));
        assert_eq!(
            record["policy"],
            serde_json::json!({"kind": "process", "program": script.to_str().unwrap()})
        );
        // A policy that merely decides to stop is another reason and carries no error.
        let stopped = RoundRecord::empty(1, &PolicySource::Builtin.record()).closed(&Stop::PolicyStopped);
        let stopped = serde_json::to_value(stopped).unwrap();
        assert_eq!(stopped["reason"], "policy_stopped");
        assert!(stopped.get("policy_error").is_none());
        // A record without `policy` reads back as "not recorded", not as a guess.
        let old: RoundRecord =
            serde_json::from_str(r#"{"v":1,"round":1,"batch":[0],"children":[1]}"#).unwrap();
        assert_eq!(old.policy, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- accepting a task ---

    /// A service world: `out/`, `tasks/` with a curriculum and a template,
    /// `policies/` with one program that is not Python (no interpreter
    /// needed), and options pointing at them with a 20 USD ceiling.
    pub(super) fn world(name: &str) -> (PathBuf, Options) {
        let dir = scratch(name);
        for sub in ["out", "tasks/curricula", "tasks/prompts", "policies"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(
            dir.join("tasks/curricula/heldout.txt"),
            "# heldout\nputnam_1962_b2\n\nputnam_1985_a1\nputnam_1962_b2\nputnam_1999_a1\n",
        )
        .unwrap();
        std::fs::write(dir.join("tasks/prompts/putnam.j2"), "{{ statement }}\n").unwrap();
        std::fs::write(dir.join("tasks/prompts/empty.j2"), " \n").unwrap();
        std::fs::write(dir.join("policies/shell-policy"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("policies/policy.py"), "print()\n").unwrap();
        std::fs::write(dir.join("prover"), "").unwrap();
        let s = |p: PathBuf| p.to_str().unwrap().to_owned();
        let options = parse(&[
            "--out",
            &s(dir.join("out")),
            "--prover-executable",
            &s(dir.join("prover")),
            "--tasks-dir",
            &s(dir.join("tasks")),
            "--policies-dir",
            &s(dir.join("policies")),
            "--max-concurrency",
            "2",
            "--max-usd-per-problem",
            "20",
            // Any existing file passes as the interpreter of a `.py` policy.
            "--policy-interpreter",
            &s(dir.join("prover")),
        ])
        .unwrap();
        (dir, options)
    }

    pub(super) fn task() -> Task {
        Task {
            statements: None,
            v: 1,
            name: "gen2-heldout".into(),
            problems: None,
            problems_file: Some("curricula/heldout.txt".into()),
            range: None,
            prompt_template: None,
            prompt_template_file: Some("prompts/putnam.j2".into()),
            policy: Some("shell-policy".into()),
            concurrency: 2,
            limits: TaskLimits::default(),
            worker_network: None,
            timeout_ms: None,
        }
    }

    fn refused(task: &Task, options: &Options) -> Error {
        resolve_task(task, options, &options.out).unwrap_err()
    }

    #[test]
    fn a_task_resolves_its_files_under_the_service_directories() {
        let (dir, options) = world("resolve");
        let plan = resolve_task(&task(), &options, &options.out).unwrap();
        assert_eq!(plan.name, "gen2-heldout");
        assert_eq!(plan.dir, dir.join("out/gen2-heldout"));
        assert_eq!(plan.workers_dir, dir.join("out/gen2-heldout.workers"), "beside the journal, never in it");
        assert_eq!(
            plan.problems,
            ["putnam_1962_b2", "putnam_1985_a1", "putnam_1999_a1"],
            "de-duplicated, in order"
        );
        assert_eq!(plan.prompt_template, "{{ statement }}\n", "verbatim");
        assert_eq!(
            plan.policy,
            PolicySource::Process {
                program: dir.canonicalize().unwrap().join("policies/shell-policy"),
                interpreter: dir.join("prover"),
                timeout: Duration::from_millis(DEFAULT_POLICY_TIMEOUT_MS),
            }
        );
        assert_eq!(plan.concurrency, 2);
        assert_eq!(
            plan.limits,
            Limits {
                w: 2,
                k1: 12,
                fanout: 3,
                calls_per_continuation: 3,
                collection_calls: 0,
                model_calls: 8,
                max_tokens: 0,
                max_usd_micro: 20_000_000,
            }
        );
        // The normalized document has the read problems and template inline.
        assert_eq!(plan.document.problems.as_deref(), Some(&plan.problems[..]));
        assert_eq!(plan.document.prompt_template.as_deref(), Some("{{ statement }}\n"));
        assert_eq!(plan.document.problems_file, None);
        assert_eq!(plan.document.limits.max_usd_per_problem.as_deref(), Some("20"));
        assert!(!plan.dir.exists() && !plan.workers_dir.exists(), "resolving creates nothing");

        // A range after de-duplication; inline sources; the built-in policy; a `.py` policy.
        let sliced = Task { range: Some([1, 2]), ..task() };
        assert_eq!(resolve_task(&sliced, &options, &options.out).unwrap().problems, ["putnam_1985_a1"]);
        let inline = Task {
            problems: Some(vec!["putnam_1962_b2".into()]),
            problems_file: None,
            prompt_template: Some("inline {{ problem }}".into()),
            prompt_template_file: None,
            policy: None,
            ..task()
        };
        let plan = resolve_task(&inline, &options, &options.out).unwrap();
        assert_eq!(
            (plan.problems.as_slice(), plan.prompt_template.as_str()),
            (&["putnam_1962_b2".to_owned()][..], "inline {{ problem }}")
        );
        assert_eq!(plan.policy, PolicySource::Builtin);
        let python = Task { policy: Some("policy.py".into()), ..task() };
        let PolicySource::Process { program, interpreter, .. } =
            resolve_task(&python, &options, &options.out).unwrap().policy
        else {
            panic!("a process policy")
        };
        assert!(program.ends_with("policies/policy.py"));
        assert_eq!(interpreter, dir.join("prover"));
        // Statements the task supplies: the problems, and recorded in the
        // document, so a resume re-reads exactly them.
        let own = "import Mathlib\n\ntheorem kmp_ok : True := by\n  sorry\n";
        let supplied = Task { statements: Some(vec![statement("kmp_ok", own)]), ..inline_ids(&[]) };
        let plan = resolve_task(&supplied, &options, &options.out).unwrap();
        assert_eq!(plan.problems, ["kmp_ok"]);
        assert_eq!(plan.statements.get("kmp_ok").map(String::as_str), Some(own));
        assert_eq!(plan.document.problems, None);
        let recorded: Task = serde_json::from_str(&serde_json::to_string(&plan.document).unwrap()).unwrap();
        assert_eq!(
            resolve_task_mode(&recorded, &options, &options.out, &options.ceilings(), true)
                .unwrap()
                .statements,
            plan.statements
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub(super) fn statement(id: &str, source: &str) -> discovery::Statement {
        discovery::Statement { id: id.into(), source: source.into() }
    }

    /// `task()` with inline problem ids and an inline template.
    fn inline_ids(ids: &[&str]) -> Task {
        Task {
            problems: (!ids.is_empty()).then(|| ids.iter().map(|s| (*s).to_owned()).collect()),
            problems_file: None,
            prompt_template: Some("{{ statement }}".into()),
            prompt_template_file: None,
            policy: None,
            ..task()
        }
    }

    #[test]
    fn a_task_is_refused_whole_on_the_first_broken_rule() {
        let (dir, options) = world("refuse");
        let message = |task: &Task| {
            let e = refused(task, &options);
            assert_eq!(e.code, ErrorCode::InvalidInput, "{}", e.message);
            e.message
        };
        // The document's own rules, against the ceilings.
        assert!(message(&Task { v: 2, ..task() }).contains("version 2"));
        assert!(message(&Task { concurrency: 3, ..task() }).contains("concurrency"));
        let over = TaskLimits { max_usd_per_problem: Some("20.5".into()), ..TaskLimits::default() };
        assert!(message(&Task { limits: over, ..task() }).contains("exceeds the ceiling 20"));
        let uncapped = TaskLimits { max_usd_per_problem: Some("0".into()), ..TaskLimits::default() };
        assert!(message(&Task { limits: uncapped, ..task() }).contains("no cap"));
        let wide = TaskLimits { w: Some(DEFAULT_MAX_WIDTH + 1), ..TaskLimits::default() };
        assert!(message(&Task { limits: wide, ..task() }).contains("width ceiling"));
        // Files: missing, a directory, an escape by `..` (syntactic) and by symlink (at access).
        assert!(
            message(&Task { problems_file: Some("curricula/none.txt".into()), ..task() })
                .contains("problems_file")
        );
        assert!(message(&Task { problems_file: Some("curricula".into()), ..task() }).contains("not a file"));
        assert!(message(&Task { prompt_template_file: Some("../prover".into()), ..task() }).contains("`..`"));
        std::os::unix::fs::symlink(dir.join("prover"), dir.join("tasks/prompts/escape.j2")).unwrap();
        let escape = Task { prompt_template_file: Some("prompts/escape.j2".into()), ..task() };
        assert!(message(&escape).contains("resolves outside"), "{}", message(&escape));
        assert!(
            message(&Task { prompt_template_file: Some("prompts/empty.j2".into()), ..task() })
                .contains("empty")
        );
        // A template that does not parse is refused with the task, inline or
        // from a file, exactly as the prover would refuse to render it; an
        // undefined variable is not a parse error and is the worker's to find.
        std::fs::write(dir.join("tasks/prompts/broken.j2"), "{% if mode %}no end\n").unwrap();
        let broken = message(&Task { prompt_template_file: Some("prompts/broken.j2".into()), ..task() });
        assert!(broken.contains("prompt_template does not parse"), "{broken}");
        let inline_broken =
            Task { prompt_template: Some("{{ statement ".into()), prompt_template_file: None, ..task() };
        assert!(message(&inline_broken).contains("does not parse"));
        let undefined =
            Task { prompt_template: Some("{{ nope }}".into()), prompt_template_file: None, ..task() };
        assert!(resolve_task(&undefined, &options, &options.out).is_ok(), "parsing is not rendering");
        assert!(check_template_syntax("{% include 'x' %}").is_err(), "no other templates exist");
        // More problems than the service allows per task, counted after the range.
        let two = Options { max_problems_per_task: 2, ..options.clone() };
        let too_many = resolve_task(&task(), &two, &two.out).unwrap_err();
        assert_eq!(too_many.code, ErrorCode::InvalidInput);
        assert!(too_many.message.contains("3 problems; at most 2 per task"), "{}", too_many.message);
        assert!(resolve_task(&Task { range: Some([0, 2]), ..task() }, &two, &two.out).is_ok());
        let bad_ids = dir.join("tasks/curricula/bad.txt");
        std::fs::write(&bad_ids, "Putnam_1962_B2\n").unwrap();
        assert!(
            message(&Task { problems_file: Some("curricula/bad.txt".into()), ..task() })
                .contains("problem id")
        );
        std::fs::write(&bad_ids, "# nothing\n").unwrap();
        assert!(
            message(&Task { problems_file: Some("curricula/bad.txt".into()), ..task() }).contains("empty")
        );
        let out_of_range = Task { range: Some([5, 9]), ..task() };
        assert!(message(&out_of_range).contains("empty"));
        // Supplied statements: exactly one problem source; Lean's frozen-file
        // rule (a `sorry` to prove, the size bound) before anything runs.
        let own = |source: &str| Task { statements: Some(vec![statement("kmp", source)]), ..inline_ids(&[]) };
        let both = Task { statements: own("").statements, ..inline_ids(&["kmp"]) };
        assert!(message(&both).contains("exactly one of problems, problems_file and statements"));
        let proved = own("import Mathlib\n\ntheorem kmp : True := by\n  trivial\n");
        assert!(message(&proved).contains("proved by `sorry`"), "{}", message(&proved));
        let huge =
            own(&format!("theorem kmp : True := sorry\n{}", "-".repeat(discovery::MAX_STATEMENT_BYTES)));
        assert!(message(&huge).contains("bytes"), "{}", message(&huge));
        // The policy: missing, or outside the policies directory.
        assert!(message(&Task { policy: Some("none".into()), ..task() }).contains("policy"));
        assert!(
            message(&Task { policy: Some("/etc/passwd".into()), ..task() }).contains("must not start with /")
        );
        // A directory that is not there is the service's problem, not the task's.
        let elsewhere = Options { tasks_dir: dir.join("nowhere"), ..options.clone() };
        assert_eq!(refused(&task(), &elsewhere).code, ErrorCode::ServiceError);
        // A taken name: the directory exists — either of the two.
        std::fs::create_dir(dir.join("out/gen2-heldout.workers")).unwrap();
        assert!(message(&task()).contains("already exists"));
        std::fs::remove_dir(dir.join("out/gen2-heldout.workers")).unwrap();
        std::fs::create_dir(dir.join("out/gen2-heldout")).unwrap();
        assert!(message(&task()).contains("already exists"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materializing_a_run_creates_its_directory_once_with_the_task_and_the_workers_dir_beside() {
        let (dir, options) = world("materialize");
        let plan = resolve_task(&task(), &options, &options.out).unwrap();
        materialize(&plan, "dynamic.1").unwrap();
        let text = std::fs::read_to_string(plan.dir.join(TASK_FILE)).unwrap();
        let recorded: Task = serde_json::from_str(&text).unwrap();
        assert_eq!(recorded, plan.document);
        assert_eq!(
            recorded.validate(&options.ceilings()).unwrap().problems,
            ProblemSet::Inline(plan.problems.clone())
        );
        assert_eq!(std::fs::read_to_string(plan.dir.join(EVENTS_FILE)).unwrap(), "");
        assert!(plan.workers_dir.is_dir());
        assert!(!plan.dir.join("workers").exists(), "nothing but problems will be subdirectories");
        assert_eq!(std::fs::read_to_string(plan.workers_dir.join(OWNER_FILE)).unwrap(), "dynamic.1");
        let again = materialize(&plan, "dynamic.2").unwrap_err();
        assert_eq!(again.code, ErrorCode::InvalidInput);
        assert!(again.message.contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(plan.dir.join(OWNER_FILE)).unwrap(),
            "dynamic.1",
            "not taken over"
        );
        // A workers' directory someone else made fails the start, which
        // removes the run directory it made and leaves that one alone.
        let other = resolve_task(&Task { name: "other".into(), ..task() }, &options, &options.out).unwrap();
        std::fs::create_dir(&other.workers_dir).unwrap();
        std::fs::write(other.workers_dir.join("theirs"), "").unwrap();
        assert_eq!(materialize(&other, "dynamic.3").unwrap_err().code, ErrorCode::ServiceError);
        assert!(!other.dir.exists() && other.workers_dir.join("theirs").is_file());
        assert_eq!(refused(&task(), &options).code, ErrorCode::InvalidInput, "and resolving refuses it too");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- a run's plan, as tests of its progress and of the registry use it ---

    pub(super) fn plan(name: &str, problems: &[&str], concurrency: usize) -> RunPlan {
        let problems: Vec<String> = problems.iter().map(|s| (*s).to_owned()).collect();
        let document = Task {
            statements: None,
            v: 1,
            name: name.into(),
            problems: Some(problems.clone()),
            problems_file: None,
            range: None,
            prompt_template: Some(TEMPLATE.into()),
            prompt_template_file: None,
            policy: None,
            concurrency,
            limits: TaskLimits::default(),
            worker_network: None,
            timeout_ms: None,
        };
        RunPlan {
            statements: BTreeMap::new(),
            name: name.into(),
            dir: PathBuf::from("/out").join(name),
            workers_dir: PathBuf::from("/out").join(format!("{name}{WORKERS_SUFFIX}")),
            problems,
            prompt_template: TEMPLATE.into(),
            policy: PolicySource::Builtin,
            concurrency,
            limits: limits(),
            worker_network: false,
            document,
        }
    }

    #[test]
    fn a_round_lost_to_the_provider_is_retried_without_charging_k1() {
        let lost = |why: &str| Failed { task: "t1".into(), why: why.into() };
        let log = |failed: Vec<Failed>| RoundLog { batch: vec![0], children: Vec::new(), failed };
        let credit = "exited_without_submitting: model_error: ServiceError: provider reports insufficient credit (http 402)";
        assert!(provider_failure_round(&log(vec![lost(credit), lost(credit)])));
        assert!(!provider_failure_round(&log(vec![
            lost(credit),
            lost("exited_without_submitting: max_attempts")
        ])));
        assert!(!provider_failure_round(&log(vec![lost(
            "exited_without_submitting: model_budget: PermissionDenied"
        )])));
        assert!(!provider_failure_round(&log(Vec::new())), "a round nobody failed is not the provider's");
        let mut child_too = log(vec![lost(credit)]);
        child_too.children.push(1);
        assert!(!provider_failure_round(&child_too), "a child was produced: the round counted");

        // One round allowed: a retried round keeps its number and does not use it up.
        let mut r = rollout(0.5, Limits { k1: 1, ..limits() });
        assert!(r.plan_round(&mut InitialPolicy).is_ok());
        r.finish_round();
        r.note_provider_retry();
        assert!(r.plan_round(&mut InitialPolicy).is_ok(), "round 2 replaces the lost round 1");
        r.finish_round();
        assert_eq!(r.plan_round(&mut InitialPolicy), Err(Stop::Rounds));
        assert_eq!(Stop::ProviderUnavailable.reason(), "provider_unavailable");
    }

    #[test]
    fn a_submission_that_changes_the_frozen_preamble_is_refused_before_lean_and_not_spent() {
        let frozen =
            "import Mathlib\n\ndef f (n : Nat) : Nat := n\n\ntheorem demo : f 0 = 0 := by\n  sorry\n";
        let mut r = Rollout::new("demo", frozen.into(), TEMPLATE.into(), eval(0.5), limits());
        r.plan_round(&mut InitialPolicy).unwrap();
        r.open_task("t000001", 0, ProcessId(7)).unwrap();
        let t1 = ResourcePath::new(["run", "demo", "t000001"]).unwrap();
        // The statement is kept verbatim; the definition it uses is not.
        let redefined = frozen.replace(":= n\n", ":= 0\n").replace("sorry", "rfl");
        let why = r
            .accept_submission(&t1, ProcessId(7), Submission { file: redefined, ..submission("x") })
            .unwrap_err();
        assert_eq!(why.code, ErrorCode::InvalidInput);
        assert!(why.message.contains("preamble"), "{}", why.message);
        let kept = Submission { file: frozen.replace("sorry", "rfl"), ..submission("x") };
        assert!(matches!(r.accept_submission(&t1, ProcessId(7), kept).unwrap(), Served::Accepted { .. }));
    }
}
