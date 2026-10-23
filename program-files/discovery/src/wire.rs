//! The private interface between the discovery service and the process of
//! each run, version [`PROTOCOL`].
//!
//! The service (D) never waits on a run. It spawns every run as its own
//! child process (R): this program in [`RUN_MODE`], started with
//! [`RunCommand::args`], holding only its run's narrowing of D's authority.
//! Two endpoints connect them, both allocated by D with `create_endpoint`:
//!
//! - **R's endpoint**, `dynamic.<n>`, one per run, which R provides with
//!   [`run_interface`]. D calls `admit`, `begin` and `forward` on it. D keeps
//!   the endpoint's only invocation handle, so every call R receives there
//!   comes from D, and D rebinds that handle once R provides.
//! - **The report endpoint**, `dynamic.<m>`, one per service, which D
//!   provides with [`report_interface`]. Every R calls `report` on it, on the
//!   resource `[key]` its non-delegable grant is scoped to, and D checks the
//!   caller is the process it spawned with that key. D answers every report
//!   at once and holds none, so a report never waits behind a call D holds (a
//!   worker's call at `explore`, a `start` at `discovery`): that is why it
//!   has an endpoint of its own.
//!
//! **A run has a key** ([`key_of`]): its name and a digest of its project
//! and name. Runs are named within their project, so two projects' runs may
//! share a name; every resource that leaves the service's own interface names
//! the key instead: the run's `lean` registrations, its workers' `explore`
//! calls and its reports. The key never changes, so `lean` still refuses
//! another text for a statement it holds for the run, after a resume too.
//! Every call names it as its resource's first segment: `[key]` for `admit`,
//! `begin` and `report`, the worker's own `[key, problem, task]` for
//! `forward`. Every payload is one message of this module, encoded with
//! [`encode`] and read with [`decode`].
//!
//! # A run's life, message by message
//!
//! 1. D allocates R's endpoint, spawns R and holds the `start` or `resume`
//!    it serves.
//! 2. R provides its endpoint and sends its first [`Report`], without a
//!    status: it is ready. D rebinds and calls [`Admit`], with the caller's
//!    time as the deadline.
//! 3. R answers [`Admitted`], or refuses with the error the caller gets and
//!    exits. D answers the held call either way.
//! 4. While fewer than `--max-running-runs` runs run, D calls [`Begin`] on
//!    the oldest admitted run. R starts its rollouts and sends a [`Report`]
//!    after every change.
//! 5. D forwards every call a worker makes at `explore` ([`Forward`]); R's
//!    answer is the worker's.
//! 6. R's last report carries `done` or `failed`, and R exits. `stop` is D's
//!    kernel `kill` of R; R then closes its journals on `Terminate` and sends
//!    nothing more.
//!
//! # Errors, deadlines and cancel
//!
//! A refusal travels as the call's error. R answers a forward with the error
//! the worker gets, and D relays it; the kernel passes only `invalid_input`,
//! `permission_denied`, `unknown_outcome` and `service_error` from a provider
//! (any other code arrives as `service_error`). A forward the kernel cut
//! short (R ended, the deadline passed) reaches D as the kernel's error; D
//! relays `unknown_outcome` for `submit` and `publish_plan`, and a
//! `service_error` for `assignment` and `read_plans`, which change nothing.
//!
//! D gives a forward the worker's remaining time and an `admit` the time its
//! caller has left for the `start` or `resume`; R works within its dispatch's
//! `remaining_ms` and answers inside it.
//!
//! There is no cancel message: D cancels a forward or an `admit` with the
//! kernel's own `Cancel` when its caller gives up, and R receives
//! `CancelDispatch`. The kernel keeps a cancelled dispatch in its provider's
//! in-flight count until the provider replies, so D replies at once to a
//! worker's cancelled call. R does not act on `CancelDispatch`: it serves
//! every forward in the order it came, a cancelled one too, and its reply is
//! dropped; a submission whose worker gave up waiting is evaluated and
//! recorded all the same. D abandons an admission by killing R.
//!
//! # Versions
//!
//! D and R are the same program, but the file on disk can be replaced while
//! D runs. D puts [`PROTOCOL`] on R's command line and in every message; R
//! refuses a command line of another version by sending a report that
//! carries only its own `v`, and exits. Each side reads a message's `v`
//! before anything else ([`decode`]) and refuses another version with
//! [`mismatch`], which D passes to the caller. Three things are therefore
//! fixed in every version: the command line's first three arguments (`run
//! --protocol <n>`), the `report` action on the resource of R's grant, and
//! a top-level `v` in every message.
//!
//! Every message rejects unknown fields and is bounded, encoded, by
//! [`MAX_MESSAGE_BYTES`] ([`MAX_REPORT_BYTES`] for a report). The nested
//! records are the discovery interface's (`Task`, `RunStatus`) and the
//! journal's ([`RoundRecord`]), bounded by their own rules and by the
//! message's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nucleus_discovery_interface::{self as discovery, RunState, RunStatus, Task};
use nucleus_explore_interface as explore;
use nucleus_protocol::{
    ActionDecl, ActionName, CREATED_ENDPOINT_PREFIX, EffectClass, EndpointName, Error, ErrorCode,
    InterfaceDecl, Payload, ProcessId,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Options, RoundRecord};

/// Version of this interface: on R's command line, in every message, and as
/// the version of both endpoints' declarations.
pub const PROTOCOL: u32 = 2;

/// The first argument that starts this program as a run's process.
pub const RUN_MODE: &str = "run";

/// Actions on R's endpoint, called by D.
pub const ACTION_ADMIT: &str = "admit";
pub const ACTION_BEGIN: &str = "begin";
pub const ACTION_FORWARD: &str = "forward";
/// Every action of R's endpoint: what D allocates it with.
pub const RUN_ACTIONS: [&str; 3] = [ACTION_ADMIT, ACTION_BEGIN, ACTION_FORWARD];
/// The action on D's report endpoint, called by every R.
pub const ACTION_REPORT: &str = "report";

/// The most bytes any message takes, encoded: above a task document's
/// sixteen 64 KiB statements and its template with a problem list of
/// thousands of ids; far below the kernel's frame limit.
pub const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
/// The most bytes a [`Report`] takes, encoded. R keeps under it by leaving
/// out the `failed` entries of live rounds first and then the live rounds;
/// D then shows what the journal has.
pub const MAX_REPORT_BYTES: usize = 256 * 1024;
/// Live rounds of one problem: the round held back for its stop reason and
/// the open one.
pub const MAX_LIVE_ROUNDS: usize = 2;

/// What R provides on its endpoint `endpoint`. The actions are internal:
/// no model sees them as tools.
pub fn run_interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: PROTOCOL,
        description: String::new(),
        actions: BTreeMap::from([
            // Creates the run's directories and registers its statements.
            (ActionName::new(ACTION_ADMIT)?, ActionDecl::internal(EffectClass::Irreversible)),
            // Starts spending the run's budget.
            (ActionName::new(ACTION_BEGIN)?, ActionDecl::internal(EffectClass::Irreversible)),
            // The strongest effect of the worker actions it carries (`submit`
            // records a node), so an interrupted forward is an unknown outcome.
            (ActionName::new(ACTION_FORWARD)?, ActionDecl::internal(EffectClass::Compensatable)),
        ]),
    })
}

/// What D provides on its report endpoint `endpoint`.
pub fn report_interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: PROTOCOL,
        description: String::new(),
        actions: BTreeMap::from([(
            ActionName::new(ACTION_REPORT)?,
            // Replaces D's view of the run; the next report replaces it again.
            ActionDecl::internal(EffectClass::ReversibleMutation),
        )]),
    })
}

// --- Messages -------------------------------------------------------------------

/// One message of this interface: its version and its own rules.
pub trait Message: Serialize + DeserializeOwned {
    /// The most bytes it takes, encoded.
    const MAX_BYTES: usize = MAX_MESSAGE_BYTES;
    /// The protocol version its sender speaks.
    fn version(&self) -> u32;
    /// Its rules beyond its shape and size.
    fn check(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// The error for a message or command line of another version: the program
/// was replaced on disk while the service ran.
pub fn mismatch(other: u32) -> Error {
    Error::new(
        ErrorCode::ServiceError,
        format!(
            "the discovery program was updated while the service ran (protocol {other} met protocol \
             {PROTOCOL} between the service and a run's process); restart the discovery service"
        ),
    )
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

fn too_large(bytes: usize, max: usize) -> Error {
    invalid(format!("the message is {bytes} bytes; at most {max}"))
}

/// Encodes `message`, refusing one beyond its bounds or rules. The sender
/// sets `v` to [`PROTOCOL`].
pub fn encode<M: Message>(message: &M) -> Result<Payload, Error> {
    if message.version() != PROTOCOL {
        return Err(mismatch(message.version()));
    }
    message.check()?;
    let payload = Payload::encode(message)?;
    if payload.len() > M::MAX_BYTES {
        return Err(too_large(payload.len(), M::MAX_BYTES));
    }
    Ok(payload)
}

/// The version a message says it is in, whatever else it carries: the one
/// field every version of every message has.
pub fn version_of(payload: &Payload) -> Result<u32, Error> {
    #[derive(Deserialize)]
    struct Versioned {
        v: u32,
    }
    Ok(payload.decode::<Versioned>()?.v)
}

/// Decodes a message: its size, then its `v` alone (another version is
/// [`mismatch`], whatever else it carries), then its shape (unknown fields
/// refused) and rules.
pub fn decode<M: Message>(payload: &Payload) -> Result<M, Error> {
    if payload.len() > M::MAX_BYTES {
        return Err(too_large(payload.len(), M::MAX_BYTES));
    }
    let v = version_of(payload)?;
    if v != PROTOCOL {
        return Err(mismatch(v));
    }
    let message: M = payload.decode()?;
    message.check()?;
    Ok(message)
}

/// The key of `run` of `project`: `<run>-<digest>`, the first segment of
/// every resource of the run outside the discovery interface, which a
/// `lean` run segment may be (at most 61 characters). The digest is the
/// first 80 bits of SHA-256 of `<project>/<run>` in hex: no project can name
/// a run whose key is another project's run's, short of a second preimage.
pub fn key_of(project: &str, run: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{project}/{run}").as_bytes());
    let hex: String = digest[..KEY_DIGEST_BYTES].iter().map(|b| format!("{b:02x}")).collect();
    format!("{run}-{hex}")
}

/// Bytes of the digest in a run's key.
const KEY_DIGEST_BYTES: usize = 10;

/// D → R, `admit` on `[key]`: sent once, after R's first report.
///
/// R answers [`Admitted`] only once the run exists and is its own: for
/// `start`, every benchmark id resolved through `lean statement` and every
/// supplied statement registered on `[key, problem]`, then the run's
/// directories created and its lease held; for `resume`, its supplied
/// statements registered again, its lease held, a journal the run's previous
/// process left open closed as `terminated`, the checkpoint loaded and
/// `resume_admitted` recorded. It refuses with the
/// error the caller gets (Lean's own words for a statement Lean refuses) and
/// then exits; a refused `start` creates nothing. Both work within the
/// dispatch's `remaining_ms`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admit {
    pub v: u32,
    pub admission: Admission,
}

/// What an [`Admit`] asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Admission {
    /// `discovery start`: the task as D accepted it, normalized (the document
    /// `task.json` records: problems and template inline, every limit
    /// written out), named after R's run. R resolves it again under the same
    /// options before anything else.
    Start { task: Box<Task> },
    /// `discovery resume`: the run saved under `<out>/<run>` is continued.
    Resume,
}

impl Message for Admit {
    fn version(&self) -> u32 {
        self.v
    }
}

/// R → D, the answer to [`Admit`]: the run is admitted and waits for
/// [`Begin`]. D answers the held `start` or `resume` from it and serves the
/// run's `status` from `status` until a [`Report`] of R carries one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admitted {
    pub v: u32,
    /// The run as R holds it, in state `queued`.
    pub status: RunStatus,
    /// `resume`: the model calls whose outcome the checkpoint cannot tell
    /// (`ResumeResult::uncertain_model_calls`); 0 for `start`.
    pub uncertain_model_calls: u64,
}

impl Message for Admitted {
    fn version(&self) -> u32 {
        self.v
    }

    fn check(&self) -> Result<(), Error> {
        if self.status.state != RunState::Queued {
            return Err(invalid("an admitted run is queued"));
        }
        Ok(())
    }
}

/// D → R, `begin` on `[key]`: at most once, after [`Admitted`], when a
/// running slot is free. R answers [`Ack`] at once and starts the run's
/// rollouts; it refuses a `begin` before it was admitted or a second one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Begin {
    pub v: u32,
}

impl Message for Begin {
    fn version(&self) -> u32 {
        self.v
    }
}

/// The answer that carries nothing but the version: R's to [`Begin`], D's to
/// [`Report`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub v: u32,
}

impl Message for Ack {
    fn version(&self) -> u32 {
        self.v
    }
}

/// D → R, `forward` on the worker's own `[key, problem, task]`, exactly as
/// the kernel checked it at `explore`: one call a worker made there.
///
/// D forwards only to a run that has begun, gives the call the worker's
/// remaining time, and relays R's answer to the worker unread. R serves it as
/// the harness serves `explore` (the task must be open and `caller` must be
/// the process R spawned for it) and answers with what the worker gets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    pub v: u32,
    /// The worker, as the kernel named the caller of its call to D. R trusts
    /// it because only D can call R's endpoint.
    pub caller: ProcessId,
    /// The worker's action: one of the `explore` interface's.
    pub action: ActionName,
    /// The worker's payload, which D does not read.
    pub payload: Payload,
}

impl Message for Forward {
    fn version(&self) -> u32 {
        self.v
    }

    fn check(&self) -> Result<(), Error> {
        let worker = [
            explore::ACTION_ASSIGNMENT,
            explore::ACTION_SUBMIT,
            explore::ACTION_PUBLISH_PLAN,
            explore::ACTION_READ_PLANS,
        ];
        if !worker.contains(&self.action.as_str()) {
            return Err(invalid(format!("{} is not an explore action", self.action)));
        }
        Ok(())
    }
}

/// R → D, `report` on `[key]`: what D serves `status`, `list`, `problems`
/// and `tree` from while the run's process lives.
///
/// R sends its first report, without `status`, once it provides its
/// endpoint; then one after every step of its loop that changed anything
/// once the run has begun; and a last one, in state `done` or `failed`,
/// before it exits. D answers [`Ack`] at once and refuses a report from any
/// process but the one it spawned for the run. What a report says replaces
/// what the one before it said.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub v: u32,
    /// The run, in state `running`, `done` or `failed`; absent in R's first
    /// report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    /// The problems started so far: the task's first `started` problems. A
    /// started problem not in `status.active` has ended; one not started is
    /// `queued`.
    #[serde(default)]
    pub started: u64,
    /// For each active problem, the rounds its journal does not have yet
    /// (what `discovery tree` adds for a live problem).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live: Vec<LiveRounds>,
}

/// The rounds of one active problem that are not in its journal yet: the
/// completed round held back for its stop reason and the open one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRounds {
    pub problem: String,
    pub rounds: Vec<RoundRecord>,
}

impl Report {
    /// R's first report: it provides its endpoint.
    #[must_use]
    pub fn ready() -> Self {
        Report { v: PROTOCOL, status: None, started: 0, live: Vec::new() }
    }
}

impl Message for Report {
    const MAX_BYTES: usize = MAX_REPORT_BYTES;

    fn version(&self) -> u32 {
        self.v
    }

    fn check(&self) -> Result<(), Error> {
        let Some(status) = &self.status else {
            if self.started != 0 || !self.live.is_empty() {
                return Err(invalid("the first report carries nothing but its version"));
            }
            return Ok(());
        };
        if !matches!(status.state, RunState::Running | RunState::Done | RunState::Failed) {
            return Err(invalid("a report's run is running, done or failed"));
        }
        if self.started > status.problems {
            return Err(invalid("a report starts no more problems than the run has"));
        }
        let mut seen = BTreeSet::new();
        for live in &self.live {
            if !status.active.contains(&live.problem) || !seen.insert(live.problem.as_str()) {
                return Err(invalid(format!("live rounds of {:?}, which is not active once", live.problem)));
            }
            if live.rounds.len() > MAX_LIVE_ROUNDS {
                return Err(invalid(format!("at most {MAX_LIVE_ROUNDS} live rounds per problem")));
            }
        }
        Ok(())
    }
}

// --- R's command line -------------------------------------------------------------

/// What R is started with: `run --protocol <PROTOCOL> --run <name> --endpoint
/// <dynamic.n> --report-endpoint <dynamic.m>`, in that order, followed by the
/// service's own options exactly as the service was given them
/// ([`Options::args`]), so R's options are D's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCommand {
    pub run: String,
    /// R's own endpoint, which R provides.
    pub endpoint: EndpointName,
    /// D's report endpoint.
    pub report_endpoint: EndpointName,
    pub options: Options,
}

/// Why R refuses its command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// The service speaks protocol `service`: the program on disk is not the
    /// one the service runs. R reports this ([`Report`] with only `v`).
    Protocol {
        service: u32,
    },
    Invalid(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { service } => f.write_str(&mismatch(*service).message),
            Self::Invalid(why) => write!(f, "run command line: {why}"),
        }
    }
}

impl RunCommand {
    /// The arguments D starts R with, after the executable.
    #[must_use]
    pub fn args(
        run: &str,
        endpoint: &EndpointName,
        report_endpoint: &EndpointName,
        options: &Options,
    ) -> Vec<String> {
        let mut args = vec![
            RUN_MODE.to_owned(),
            "--protocol".to_owned(),
            PROTOCOL.to_string(),
            "--run".to_owned(),
            run.to_owned(),
            "--endpoint".to_owned(),
            endpoint.as_str().to_owned(),
            "--report-endpoint".to_owned(),
            report_endpoint.as_str().to_owned(),
        ];
        args.extend(options.args.iter().cloned());
        args
    }

    /// Parses R's arguments after the executable ([`Self::args`]). The
    /// protocol is checked before anything that may differ between versions.
    pub fn parse(args: Vec<String>) -> Result<Self, CommandError> {
        let bad = CommandError::Invalid;
        let mut args = args.into_iter();
        if args.next().as_deref() != Some(RUN_MODE) {
            return Err(bad(format!("the first argument must be {RUN_MODE}")));
        }
        let mut value = |flag: &str| match (args.next(), args.next()) {
            (Some(got), Some(value)) if got == flag => Ok(value),
            _ => Err(bad(format!("expected {flag} <value> here"))),
        };
        let protocol = value("--protocol")?;
        let service: u32 = protocol.parse().map_err(|_| bad(format!("--protocol {protocol:?}")))?;
        if service != PROTOCOL {
            return Err(CommandError::Protocol { service });
        }
        let (run, endpoint, report_endpoint) =
            (value("--run")?, value("--endpoint")?, value("--report-endpoint")?);
        if !discovery::valid_name(&run) {
            return Err(bad(format!("--run {run:?} is not a run name")));
        }
        // Both are allocated by the service: R never provides or calls a
        // well-known name.
        let allocated = |flag: &str, name: String| {
            EndpointName::new(name.as_str())
                .ok()
                .filter(|e| e.as_str().starts_with(CREATED_ENDPOINT_PREFIX))
                .ok_or_else(|| bad(format!("{flag} {name:?} is not an allocated endpoint")))
        };
        let endpoint = allocated("--endpoint", endpoint)?;
        let report_endpoint = allocated("--report-endpoint", report_endpoint)?;
        if endpoint == report_endpoint {
            return Err(bad("--endpoint and --report-endpoint must differ".to_owned()));
        }
        let options = Options::parse(args).map_err(bad)?;
        Ok(RunCommand { run, endpoint, report_endpoint, options })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{task, world};
    use nucleus_discovery_interface::{TaskLimits, TaskShape};
    use serde_json::{Value, json};

    fn status(state: RunState, active: &[&str]) -> RunStatus {
        RunStatus {
            run: "gen2".into(),
            state,
            problems: 3,
            finished: 0,
            solved: 0,
            active: active.iter().map(|p| (*p).to_owned()).collect(),
            tokens_in: 0,
            tokens_out: 0,
            usd: None,
            error: None,
            task: TaskShape {
                policy: None,
                concurrency: 2,
                limits: TaskLimits::default(),
                worker_network: None,
            },
            failed_continuations: 0,
            last_failure: None,
            started_at_ms: None,
            failure_reasons: vec![],
            other_failures: 0,
        }
    }

    fn open_round(round: u32) -> RoundRecord {
        RoundRecord {
            v: crate::JOURNAL_VERSION,
            round,
            policy: None,
            batch: vec![0, 0],
            children: vec![1],
            failed: vec![],
            tokens_in: 10,
            tokens_out: 5,
            usd: None,
            reason: None,
            policy_error: None,
        }
    }

    fn progress(active: &[&str], live: Vec<LiveRounds>) -> Report {
        Report { v: PROTOCOL, status: Some(status(RunState::Running, active)), started: 2, live }
    }

    fn forward(action: &str) -> Forward {
        Forward {
            v: PROTOCOL,
            caller: ProcessId(9),
            action: ActionName::new(action).unwrap(),
            payload: Payload::encode(&json!({})).unwrap(),
        }
    }

    /// Encodes `message`, lets `edit` change its JSON, and decodes the result.
    fn edited<M: Message>(message: &M, edit: impl FnOnce(&mut Value)) -> Result<M, Error> {
        let mut value: Value = encode(message).unwrap().decode().unwrap();
        edit(&mut value);
        decode(&Payload::encode(&value).unwrap())
    }

    fn row<M: Message + PartialEq + fmt::Debug>(message: M) {
        let payload = encode(&message).unwrap();
        assert_eq!(decode::<M>(&payload).unwrap(), message, "round trip");
        let unknown = edited(&message, |v| v["extra"] = json!(1)).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::InvalidInput, "unknown fields are refused");
        // Another build's message is refused by its version alone, whatever
        // else it carries, and the words say what to do.
        let other = edited(&message, |v| {
            v["v"] = json!(PROTOCOL + 1);
            v["from_a_later_build"] = json!(true);
        })
        .unwrap_err();
        assert_eq!(other.code, ErrorCode::ServiceError);
        assert!(other.message.contains("updated while the service ran"), "{}", other.message);
    }

    /// A run's key is its name and a digest of its project and name: the
    /// same run keeps it, another project's run of the same name has
    /// another, and the longest is a `lean` run segment.
    #[test]
    fn a_runs_key_is_its_own_in_every_project_and_lean_reads_it() {
        let key = key_of("default", "gen2");
        assert_eq!(key, key_of("default", "gen2"));
        assert_ne!(key, key_of("other", "gen2"));
        let longest = key_of(&"p".repeat(40), &"r".repeat(40));
        let resource = nucleus_protocol::ResourcePath::new([longest.as_str(), "demo_a"]).unwrap();
        assert!(nucleus_lean_interface::statement_ref(&resource, |_| false).is_ok(), "{longest}");
    }

    #[test]
    fn every_message_round_trips_refuses_unknown_fields_and_names_another_build() {
        let admitted =
            Admitted { v: PROTOCOL, status: status(RunState::Queued, &[]), uncertain_model_calls: 3 };
        let live = LiveRounds { problem: "a".into(), rounds: vec![open_round(1), open_round(2)] };
        row(Admit { v: PROTOCOL, admission: Admission::Start { task: Box::new(task()) } });
        row(Admit { v: PROTOCOL, admission: Admission::Resume });
        row(admitted.clone());
        row(Begin { v: PROTOCOL });
        row(Ack { v: PROTOCOL });
        row(forward(explore::ACTION_SUBMIT));
        row(Report::ready());
        row(progress(&["a", "b"], vec![live]));
        // An `admission` naming anything else is refused too.
        let resume = Admit { v: PROTOCOL, admission: Admission::Resume };
        assert!(edited(&resume, |v| v["admission"] = json!({"resume": {"run": "other"}})).is_err());
        let start = Admit { v: PROTOCOL, admission: Admission::Start { task: Box::new(task()) } };
        assert!(edited(&start, |v| v["admission"]["start"]["run"] = json!("other")).is_err());
        // The sender cannot put another version on the wire either.
        assert_eq!(encode(&Ack { v: PROTOCOL + 1 }).unwrap_err().code, ErrorCode::ServiceError);
        let running = Admitted { status: status(RunState::Running, &[]), ..admitted };
        assert_eq!(encode(&running).unwrap_err().code, ErrorCode::InvalidInput, "admitted means queued");
    }

    #[test]
    fn a_report_is_bounded_and_says_only_what_the_run_holds() {
        let live = |problem: &str, rounds: u32| LiveRounds {
            problem: problem.into(),
            rounds: (1..=rounds).map(open_round).collect(),
        };
        encode(&progress(&["a"], vec![live("a", 2)])).unwrap();
        for (why, report) in [
            ("a first report carries nothing else", Report { started: 1, ..Report::ready() }),
            ("live rounds of a problem that is not active", progress(&["a"], vec![live("b", 1)])),
            ("a problem's live rounds twice", progress(&["a"], vec![live("a", 1), live("a", 1)])),
            ("more than the held-back and the open round", progress(&["a"], vec![live("a", 3)])),
            ("more problems started than the run has", Report { started: 4, ..progress(&["a"], vec![]) }),
            (
                "only the service says a run is stopped",
                Report { status: Some(status(RunState::Stopped, &[])), ..progress(&[], vec![]) },
            ),
        ] {
            assert_eq!(encode(&report).unwrap_err().code, ErrorCode::InvalidInput, "{why}");
        }
        let mut long = open_round(1);
        long.failed =
            (0..2000).map(|k| discovery::Failed { task: format!("t{k:06}"), why: "x".repeat(200) }).collect();
        let huge = progress(&["a"], vec![LiveRounds { problem: "a".into(), rounds: vec![long] }]);
        assert!(encode(&huge).unwrap_err().message.contains("at most"));
        assert!(decode::<Report>(&Payload::encode(&huge).unwrap()).is_err(), "and refused on arrival");
        // A forward carries a worker's action and nothing else.
        encode(&forward(explore::ACTION_READ_PLANS)).unwrap();
        assert!(encode(&forward(ACTION_REPORT)).is_err());
        assert!(encode(&forward("start")).is_err());
    }

    #[test]
    fn the_run_command_line_carries_the_service_options_and_refuses_another_protocol() {
        let (dir, options) = world("wire_command");
        let (endpoint, report) =
            (EndpointName::new("dynamic.7").unwrap(), EndpointName::new("dynamic.2").unwrap());
        let args = RunCommand::args("gen2", &endpoint, &report, &options);
        let command = RunCommand::parse(args.clone()).unwrap();
        assert_eq!(command.run, "gen2");
        assert_eq!((command.endpoint.as_str(), command.report_endpoint.as_str()), ("dynamic.7", "dynamic.2"));
        assert_eq!(command.options, options, "the run's options are the service's");
        // `args` with `flag`'s value replaced.
        let with = |changes: &[(&str, &str)]| {
            let mut changed = args.clone();
            for (flag, value) in changes {
                let at = changed.iter().position(|a| a == flag).unwrap();
                changed[at + 1] = (*value).to_owned();
            }
            changed
        };
        // Another protocol is told apart from every other mistake, and read first.
        let newer = (PROTOCOL + 1).to_string();
        let later = with(&[("--protocol", &newer), ("--run", "Not A Name")]);
        assert_eq!(RunCommand::parse(later).unwrap_err(), CommandError::Protocol { service: PROTOCOL + 1 });
        for broken in [
            with(&[("--protocol", "one")]),
            with(&[("--run", "Not A Name")]),
            with(&[("--endpoint", "explore")]),
            args[..args.len() - options.args.len()].to_vec(),
        ] {
            assert!(matches!(RunCommand::parse(broken.clone()), Err(CommandError::Invalid(_))), "{broken:?}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn both_endpoints_are_internal_and_an_interrupted_forward_is_an_unknown_outcome() {
        let run = run_interface(EndpointName::new("dynamic.7").unwrap()).unwrap();
        let actions: Vec<&str> = run.actions.keys().map(ActionName::as_str).collect();
        let mut expected = RUN_ACTIONS.to_vec();
        expected.sort_unstable();
        assert_eq!(actions, expected);
        assert!(run.actions.values().all(|a| a.tool.is_none()), "no model sees them");
        assert_eq!(run.actions[&ActionName::new(ACTION_FORWARD).unwrap()].effect, EffectClass::Compensatable);
        let report = report_interface(EndpointName::new("dynamic.2").unwrap()).unwrap();
        assert_eq!(report.actions.keys().map(ActionName::as_str).collect::<Vec<_>>(), [ACTION_REPORT]);
        assert!(report.actions.values().all(|a| a.tool.is_none()));
        assert_eq!((run.version, report.version), (PROTOCOL, PROTOCOL));
    }
}
