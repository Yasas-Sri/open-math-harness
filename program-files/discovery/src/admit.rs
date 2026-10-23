//! A run's statements and the `lean` service's `register`: admitting a run
//! registers every statement its task supplies before anything of the run
//! exists, and every later `lean` call of the run's process names that
//! registration.
//!
//! The standard a run's candidates are judged against is the text
//! registered here, keyed by `[key, problem]` with the run's key
//! ([`crate::wire::key_of`]): no worker, and no `lean` call of the run's
//! process, ever sends frozen text (module doc, "The loop", 1). `start` and
//! `resume` register every supplied statement in task order, each within the
//! smaller of [`STATEMENT_DEADLINE_MS`] and the time the call has left: its
//! `timeout_ms`, never past the caller's own deadline, counted from when the
//! call was taken up (the SDK hands a dispatch over with the time its caller
//! has left then, however long it waited behind other work). A statement Lean
//! refuses refuses the call with Lean's own words; a call that runs out of
//! time is refused cleanly. Nothing is created until every statement is
//! registered, and nothing once the time is up.
//!
//! Registering can take minutes. It happens in the run's own process,
//! so nothing of another run waits behind it, and a `stop` of the run being
//! admitted kills that process.
//!
//! A call on a run's registration that finds it gone (`not_registered`, after
//! a restart of the `lean` service) registers the statement again and is
//! made once more ([`judge`]).

use nucleus_lean_interface::{self as lean, RegisterRequest, RegisterResponse, StatementResponse};
use nucleus_protocol::{Error, ErrorCode, Payload, ResourcePath};
use nucleus_sdk::ChannelError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::{Duration, Instant};

use super::{EVAL_DEADLINE_MS, RunPlan, invalid, materialize};

/// The longest one statement's registration may take: one cold Lean run of
/// the file.
pub const STATEMENT_DEADLINE_MS: u64 = 300_000;
/// Kept back from the caller's own deadline, so a refusal for want of time
/// reaches the caller inside it.
const REPLY_MARGIN_MS: u64 = 2_000;

/// The `lean` calls of a run's process. `Err` is a channel failure (the
/// process's end among them); `Ok(Err)` the kernel's or the service's
/// refusal.
pub(super) trait LeanService {
    fn call(
        &mut self,
        action: &str,
        resource: &ResourcePath,
        request: &Payload,
        deadline_ms: u64,
    ) -> Result<Result<Payload, Error>, ChannelError>;

    /// The clock an admission's deadline is measured on.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// [`LeanService::call`] with a typed request and reply.
fn typed<T: DeserializeOwned>(
    service: &mut dyn LeanService,
    action: &str,
    resource: &ResourcePath,
    request: &impl Serialize,
    deadline_ms: u64,
) -> Result<Result<T, Error>, ChannelError> {
    let request = match Payload::encode(request) {
        Ok(request) => request,
        Err(e) => return Ok(Err(e)),
    };
    Ok(service.call(action, resource, &request, deadline_ms)?.and_then(|reply| reply.decode()))
}

/// `lean statement` of the benchmark problem `problem`, `[problem]`.
pub(super) fn statement(
    service: &mut dyn LeanService,
    problem: &ResourcePath,
    deadline_ms: u64,
) -> Result<Result<StatementResponse, Error>, ChannelError> {
    typed(service, lean::ACTION_STATEMENT, problem, &serde_json::json!({}), deadline_ms)
}

/// `lean register` of `frozen` for `resource`, `[key, problem]`.
pub(super) fn register_statement(
    service: &mut dyn LeanService,
    resource: &ResourcePath,
    frozen: &str,
    deadline_ms: u64,
) -> Result<Result<RegisterResponse, Error>, ChannelError> {
    let request = RegisterRequest { frozen: frozen.to_owned() };
    typed(service, lean::ACTION_REGISTER, resource, &request, deadline_ms)
}

/// A `check`, `verify`, `analyze` or `complete` call of a rollout on
/// `resource`, `[key, problem]`: the candidate alone, judged against the
/// run's registration. When the `lean` service no longer holds it
/// (`not_registered`, as the kernel passes it on), `frozen` is registered
/// again and the call made once more.
pub(super) fn judge<T: DeserializeOwned>(
    service: &mut dyn LeanService,
    action: &str,
    resource: &ResourcePath,
    frozen: &str,
    request: &impl Serialize,
    deadline_ms: u64,
) -> Result<Result<T, Error>, ChannelError> {
    match typed::<T>(service, action, resource, request, deadline_ms)? {
        Err(e) if lean::is_not_registered(&e) => {
            match register_statement(service, resource, frozen, STATEMENT_DEADLINE_MS)? {
                Ok(registered) if registered.accepted => {
                    typed(service, action, resource, request, deadline_ms)
                }
                Ok(refused) => Ok(Err(Error::new(
                    ErrorCode::ServiceError,
                    format!(
                        "the statement is no longer registered, and Lean refused it again: {}",
                        refused.lean_output.unwrap_or_default()
                    ),
                ))),
                Err(again) => Ok(Err(again)),
            }
        }
        other => Ok(other),
    }
}

/// How long a `start` or `resume` may spend admitting its run: its
/// `timeout_ms` when given, never past the caller's own deadline
/// (`remaining_ms`, the kernel's) less a margin.
#[must_use]
pub(super) fn budget_ms(timeout_ms: Option<u64>, remaining_ms: u64) -> u64 {
    timeout_ms.unwrap_or(u64::MAX).min(remaining_ms.saturating_sub(REPLY_MARGIN_MS))
}

/// When a `start` or `resume` taken up at `taken`, with the caller's
/// `remaining_ms` as its dispatch was handed over, must be done admitting.
#[must_use]
pub(super) fn until(taken: Instant, timeout_ms: Option<u64>, remaining_ms: u64) -> Instant {
    taken + Duration::from_millis(budget_ms(timeout_ms, remaining_ms))
}

/// Why `register` refused `problem`'s statement, as the caller reads it.
#[must_use]
pub(super) fn refusal(problem: &str, response: &RegisterResponse) -> String {
    format!("statement {problem:?}: Lean says:\n{}", response.lean_output.as_deref().unwrap_or("(no output)"))
}

/// `discovery start` after the task itself was accepted: by `until`, every
/// benchmark id must resolve through `lean statement` (and a supplied
/// statement may not reuse one for another text) and every supplied
/// statement must be registered, and only then are the run's directories
/// created. A refusal leaves nothing behind in the discovery program. Lean
/// keeps the registrations it accepted before the refusal, and a registered
/// `[key, problem]` never changes, so a corrected start that changes one of
/// those texts needs another run name (Lean's refusal says so).
pub(super) fn create(
    plan: &RunPlan,
    key: &str,
    until: Instant,
    service: &mut dyn LeanService,
    owner: &str,
) -> Result<Result<(), Error>, ChannelError> {
    match admit(plan, key, until, true, service)? {
        Ok(()) => Ok(materialize(plan, owner)),
        Err(e) => Ok(Err(e)),
    }
}

/// `discovery resume` of `plan`, whose task was accepted again: every
/// statement it supplies registered again by `until`, as for [`create`].
pub(super) fn register(
    plan: &RunPlan,
    key: &str,
    until: Instant,
    service: &mut dyn LeanService,
) -> Result<Result<(), Error>, ChannelError> {
    admit(plan, key, until, false, service)
}

/// The `lean` part of admitting `plan` by `until`: with `ids` (`start`),
/// every problem id through `lean statement`; then the registration of every
/// statement the task supplies, in task order, on `[key, problem]`.
fn admit(
    plan: &RunPlan,
    key: &str,
    until: Instant,
    ids: bool,
    service: &mut dyn LeanService,
) -> Result<Result<(), Error>, ChannelError> {
    let supplied: Vec<(&String, &String)> =
        plan.problems.iter().filter_map(|p| plan.statements.get(p).map(|source| (p, source))).collect();
    let out_of_time = |doing: String| {
        Error::new(
            ErrorCode::Timeout,
            format!(
                "task: the call ran out of time {doing}; the run was not queued. Lean checks each supplied \
                 statement once, which can take minutes: pass a larger timeout_ms (at most {})",
                nucleus_discovery_interface::MAX_START_TIMEOUT_MS
            ),
        )
    };
    if ids {
        for problem in &plan.problems {
            let left = left_ms(service, until);
            if left == 0 {
                return Ok(Err(out_of_time(format!("resolving problem {problem:?}"))));
            }
            let resource = match ResourcePath::new([problem]) {
                Ok(r) => r,
                Err(e) => return Ok(Err(invalid(format!("task: problem {problem:?}: {}", e.message)))),
            };
            match (statement(service, &resource, left.min(EVAL_DEADLINE_MS))?, plan.statements.get(problem)) {
                (Err(e), _) if e.code == ErrorCode::Timeout || left_ms(service, until) == 0 => {
                    return Ok(Err(out_of_time(format!("resolving problem {problem:?}"))));
                }
                (Err(e), None) => {
                    return Ok(Err(invalid(format!(
                        "task: problem {problem:?}: {:?}: {}",
                        e.code, e.message
                    ))));
                }
                // The `lean` service would refuse to register it.
                (Ok(own), Some(source)) if own.statement != *source => {
                    return Ok(Err(invalid(format!(
                        "task: statement {problem:?}: the id names a benchmark problem with another statement"
                    ))));
                }
                _ => {}
            }
        }
    }
    for (done, (problem, frozen)) in supplied.iter().enumerate() {
        let registering = || {
            format!("registering statement {problem:?} with Lean ({done} of {} registered)", supplied.len())
        };
        let left = left_ms(service, until);
        if left == 0 {
            return Ok(Err(out_of_time(registering())));
        }
        let resource = match ResourcePath::new([key, problem.as_str()]) {
            Ok(r) => r,
            Err(e) => return Ok(Err(invalid(format!("task: statement {problem:?}: {}", e.message)))),
        };
        let deadline_ms = left.min(STATEMENT_DEADLINE_MS);
        let asked = service.now();
        match register_statement(service, &resource, frozen, deadline_ms)? {
            Ok(response) if response.accepted => {}
            Ok(response) => return Ok(Err(invalid(refusal(problem, &response)))),
            // Cut off by its deadline: `timeout` before the `lean` service
            // took it, `unknown_outcome` after (registering is a mutation).
            Err(e)
                if e.code == ErrorCode::Timeout
                    || service.now().saturating_duration_since(asked)
                        >= Duration::from_millis(deadline_ms) =>
            {
                return Ok(Err(out_of_time(registering())));
            }
            Err(e) => {
                return Ok(Err(Error::new(
                    e.code,
                    format!("task: statement {problem:?}: lean register: {}", e.message),
                )));
            }
        }
    }
    if left_ms(service, until) == 0 {
        return Ok(Err(out_of_time(format!("after registering all {} statements", supplied.len()))));
    }
    Ok(Ok(()))
}

/// Milliseconds until `until` on `service`'s clock; 0 once it has passed.
fn left_ms(service: &dyn LeanService, until: Instant) -> u64 {
    u64::try_from(until.saturating_duration_since(service.now()).as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::resolve_task;
    use crate::tests::{statement, world};
    use nucleus_discovery_interface::{Task, TaskLimits};
    use nucleus_lean_interface::{CheckRequest, CheckResponse, CheckStatus, RegisterStage};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// A `lean` service behind the kernel that knows `benchmark`, refuses to
    /// register any text containing `namespace`, and takes `delay` for each
    /// registration (`lookup_delay` for each `statement`) of its simulated
    /// clock, which stands still from its first reading except for those.
    /// With `forgotten`, the service has lost the registrations it had.
    #[derive(Default)]
    struct Stub {
        benchmark: BTreeMap<String, String>,
        delay: Duration,
        lookup_delay: Duration,
        forgotten: bool,
        calls: Vec<String>,
        registered: Vec<(Vec<String>, String, u64)>,
        clock: std::cell::Cell<Option<Instant>>,
    }

    impl LeanService for Stub {
        fn call(
            &mut self,
            action: &str,
            resource: &ResourcePath,
            request: &Payload,
            deadline_ms: u64,
        ) -> Result<Result<Payload, Error>, ChannelError> {
            self.calls.push(action.to_owned());
            let segments = resource.segments().to_vec();
            let reply = match action {
                lean::ACTION_STATEMENT => {
                    self.clock.set(Some(self.now() + self.lookup_delay));
                    match self.benchmark.get(&segments[0]) {
                        Some(text) => json!({"problem": segments[0], "statement": text}),
                        None => return Ok(Err(Error::new(ErrorCode::ServiceError, "no frozen statement"))),
                    }
                }
                lean::ACTION_REGISTER => {
                    self.clock.set(Some(self.now() + self.delay));
                    let frozen = request.decode::<RegisterRequest>().unwrap().frozen;
                    self.registered.push((segments.clone(), frozen.clone(), deadline_ms));
                    self.forgotten = false;
                    if frozen.contains("namespace") {
                        json!({"accepted": false, "stage": RegisterStage::Lean,
                               "lean_output": "<input>:9:15: error: Unknown constant `kmp_a`\nhint"})
                    } else {
                        json!({"accepted": true})
                    }
                }
                // A provider's error reaches the caller as the kernel passes
                // it on: `service_error` with the provider's message.
                lean::ACTION_CHECK if self.forgotten => {
                    let lost = lean::not_registered(&segments[0], &segments[1]);
                    return Ok(Err(Error::new(ErrorCode::ServiceError, lost.message)));
                }
                lean::ACTION_CHECK => {
                    json!({"status": "complete", "errors": [], "sorries": [], "elapsed_ms": 1})
                }
                _ => return Ok(Err(Error::new(ErrorCode::InvalidInput, "unexpected action"))),
            };
            Ok(Ok(Payload::encode(&reply).unwrap()))
        }

        fn now(&self) -> Instant {
            let now = self.clock.get().unwrap_or_else(Instant::now);
            self.clock.set(Some(now));
            now
        }
    }

    const GOOD: &str = "import Mathlib\n\ntheorem kmp_b : 1 = 1 := by\n  sorry\n";
    const NAMESPACED: &str = "import Mathlib\n\nnamespace X\ntheorem kmp_a : 1 = 1 := by\n  sorry\nend X\n";

    fn supplied(name: &str, statements: Vec<nucleus_discovery_interface::Statement>) -> Task {
        Task {
            name: name.into(),
            problems_file: None,
            policy: None,
            statements: Some(statements),
            limits: TaskLimits::default(),
            ..crate::tests::task()
        }
    }

    fn within(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn a_statement_lean_refuses_refuses_the_start_with_lean_s_words_and_creates_nothing() {
        let (dir, options) = world("admit_refused");
        let plan = resolve_task(
            &supplied("refused", vec![statement("kmp_b", GOOD), statement("kmp_a", NAMESPACED)]),
            &options,
            &options.out,
        )
        .unwrap();
        let mut lean = Stub::default();
        let error = create(&plan, "dynamic-1", within(600_000), &mut lean, "dynamic.1").unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidInput);
        assert!(error.message.starts_with("statement \"kmp_a\": Lean says:\n"), "{}", error.message);
        assert!(error.message.contains("Unknown constant `kmp_a`"), "{}", error.message);
        assert!(!plan.dir.exists() && !plan.workers_dir.exists(), "a refused start leaves no run behind");
        // Registered in task order, on exactly [key, problem], with the
        // caller's text and at most one statement's deadline each.
        let names: Vec<_> = lean.registered.iter().map(|(r, _, d)| (r.join("/"), *d)).collect();
        assert_eq!(names, [("dynamic-1/kmp_b".to_owned(), 300_000), ("dynamic-1/kmp_a".to_owned(), 300_000)]);
        assert_eq!(lean.registered[1].1, NAMESPACED);

        // Accepted, the run is created, and a benchmark id is only looked up.
        let mut task = supplied("accepted", vec![statement("kmp_b", GOOD)]);
        task.statements = None;
        task.problems = Some(vec!["putnam_1962_b2".into()]);
        let mut lean = Stub::default();
        lean.benchmark.insert("putnam_1962_b2".into(), "benchmark text".into());
        let plan = resolve_task(&task, &options, &options.out).unwrap();
        create(&plan, "dynamic-1", within(600_000), &mut lean, "dynamic.1").unwrap().unwrap();
        assert!(plan.dir.join(crate::TASK_FILE).is_file());
        assert!(lean.registered.is_empty(), "a benchmark statement is registered when its rollout starts");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_start_whose_registrations_run_out_of_time_is_refused_cleanly() {
        let (dir, options) = world("admit_time");
        let plan = resolve_task(
            &supplied(
                "slow",
                vec![statement("kmp_b", GOOD), statement("kmp_c", &GOOD.replace("kmp_b", "kmp_c"))],
            ),
            &options,
            &options.out,
        )
        .unwrap();
        let mut lean = Stub { delay: Duration::from_millis(120), ..Stub::default() };
        let deadline = lean.now() + Duration::from_millis(100);
        let error = create(&plan, "dynamic-1", deadline, &mut lean, "dynamic.1").unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(
            error.message.contains("1 of 2 registered") && error.message.contains("timeout_ms"),
            "{}",
            error.message
        );
        assert!(lean.registered[0].2 <= 100, "a statement gets no more than the time left");
        assert_eq!(lean.registered.len(), 1);
        assert!(!plan.dir.exists());
        // The time counts from when the call was taken up: lookups that use
        // it up leave none for registering.
        let mut lean = Stub { lookup_delay: Duration::from_millis(150), ..Stub::default() };
        let deadline = lean.now() + Duration::from_millis(100);
        let error = create(&plan, "dynamic-1", deadline, &mut lean, "dynamic.1").unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout, "{}", error.message);
        assert!(lean.registered.is_empty() && !plan.dir.exists());
        // A call whose caller's time was gone when it was taken up (its
        // dispatch waited behind other work) creates nothing, even when it
        // has nothing to register.
        let mut task = supplied("late", vec![]);
        task.statements = None;
        task.problems = Some(vec!["putnam_1962_b2".into()]);
        let late = resolve_task(&task, &options, &options.out).unwrap();
        let mut lean = Stub::default();
        lean.benchmark.insert("putnam_1962_b2".into(), "benchmark text".into());
        let taken = Instant::now();
        let error = create(&late, "dynamic-1", until(taken, None, 1_500), &mut lean, "dynamic.1")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout, "{}", error.message);
        assert!(lean.calls.is_empty() && !late.dir.exists());
        assert_eq!(budget_ms(Some(300_000), 120_000), 118_000, "never past the caller's own deadline");
        assert_eq!(budget_ms(None, 600_000), 598_000);
        assert_eq!(budget_ms(Some(5_000), 600_000), 5_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The `lean` service keeps registrations in memory: a call that finds
    /// the run's gone, as the kernel reports it, registers the statement
    /// again and is made once more; any other failure is the call's own.
    #[test]
    fn a_call_that_finds_the_registration_gone_registers_it_again_once() {
        let resource = ResourcePath::new(["run", "kmp_b"]).unwrap();
        let request = CheckRequest { candidate: GOOD.replace("sorry", "rfl") };
        let mut lean = Stub { forgotten: true, ..Stub::default() };
        let check: CheckResponse =
            judge(&mut lean, lean::ACTION_CHECK, &resource, GOOD, &request, 1_000).unwrap().unwrap();
        assert_eq!(check.status, CheckStatus::Complete);
        assert_eq!(lean.calls, [lean::ACTION_CHECK, lean::ACTION_REGISTER, lean::ACTION_CHECK]);
        assert_eq!(lean.registered, [(vec!["run".to_owned(), "kmp_b".to_owned()], GOOD.to_owned(), 300_000)]);
        let mut lean = Stub::default();
        let refused =
            judge::<CheckResponse>(&mut lean, lean::ACTION_VERIFY, &resource, GOOD, &request, 1_000);
        assert!(refused.unwrap().is_err());
        assert!(lean.registered.is_empty(), "only a lost registration is registered again");
    }
}
