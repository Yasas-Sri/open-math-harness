//! The Dream-RSI online loop under the real kernel: the resident `discovery`
//! service provides `discovery` and `explore`; a run is the payload of
//! `discovery start` — a task document naming the problems, the prompt
//! template, the policy and the limits — issued by a client holding that
//! capability (a fixture agent playing the operator, or the headless
//! `nucleus-discovery-client`). The service starts every run as a process of
//! its own, which owns one tree per problem, registers each problem's statement
//! with the `lean` service for the run, and for every `CONTINUE(v)` starts one
//! confined prover with `spawn_executable`, its whole authority narrowed to
//! that one task; the run's process — never a worker — scores each submission
//! with the real confined `lean` service (`check` on the resident REPL, then
//! the strict `verify`) and journals the tree under `<out>/<project>/<run>/`.
//! The service forwards each worker's `explore` calls to its run's process. The
//! model is a scripted fake behind the real OpenRouter adapter; no key, no
//! network beyond loopback.
//!
//! The same two-problem run, concurrency 2, executes under the built-in
//! exploration policy, started by the fixture agent, which first has a task
//! above the service's USD ceiling and one naming an unknown problem refused,
//! then a second `start` of the same name refused, and reads `status`, `list`,
//! `problems` and `tree` at the end — the observation actions the window's
//! Runs views consume, checked against the journal they are read from; and with
//! the task naming the repository's `pi0.py`, then its `pi_doubling.py`,
//! started by the headless client, which the confined run process runs as a
//! Python process inside its own sandbox (its bubblewrap namespaces,
//! `/usr/bin/python3` from the read-only `/usr`, the program from the read-only
//! `/policies` bind). Every run must grow the same trees. A third test names a
//! policy program that exits at once: every rollout must stop as
//! `policy_failed`, with no substitution of the built-in policy and no model
//! call, and the run still ends `done`. `runs` covers a run's life beside other
//! runs, its stop and its resume; `tool_loop` the workers with a shell.
//!
//! Opt-in: needs `PB_LEAN` (the PutnamBench `lean4` workspace) **and**
//! `NUCLEUS_REPL` (the Lean REPL executable, which `check` runs on), so every
//! test is `#[ignore]`d and fails loudly when run without them. The tool
//! listing of a `discovery` holder needs no Lean and lives with the keyless
//! system tests (`discovery_tools.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "discovery/runs.rs"]
mod runs;
#[path = "discovery/tool_loop.rs"]
mod tool_loop;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nucleus_e2e::http::{FakeHttp, Scripted, completion};
use nucleus_e2e::{
    DiscoveryService, Scenario, TIMEOUT, cap, discovery_call, discovery_call_on, discovery_caller_grants,
    discovery_grants, endpoint, error_code, ok_value, program, prompts_dir, wait_for,
};
use nucleus_kernel::{CapSpec, SecurityEvent};
use nucleus_platform::Supervisor;
use nucleus_protocol::{ErrorCode, ProcessId};
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";
const KEY: &str = "sk-or-test-DISCOVERY-0123456789";
/// The curriculum, in order.
const PROBLEMS: [&str; 2] = ["demo_a", "demo_b"];
/// The run every scripted loop starts.
const RUN: &str = "demo";
/// The service's `--max-usd-per-problem`: a task's default cap and the most it may ask for.
const CEILING_USD: &str = "20";

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn stream_polls_do_not_spend_the_parents_generation_credit() {
    let (pb, _) = lean_env();
    let server = FakeHttp::start();
    let proof = proof("demo_a", "n + 0");
    server.script(Scripted::stream(MODEL, &["```lean\n", &proof, "\n```"], Duration::from_secs(1)));
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    let mut run_task = task(RUN, &["demo_a"], None);
    run_task["concurrency"] = json!(1);
    run_task["limits"] = json!({
        "w": 1, "k1": 1, "calls_per_continuation": 1, "fanout": 1,
        "model_calls_per_problem": 1, "max_usd_per_problem": CEILING_USD
    });
    scenario.add_discovery_client("client", &run_task, 500, &[]);
    let mut supervisor = scenario.start();
    scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
    scenario.boot_service(&supervisor, "openrouter", "model");
    let mut grants = discovery_grants(1);
    let generation = grants.iter_mut().find(|g| g.endpoint.as_str() == "model").unwrap();
    generation.actions.insert(nucleus_protocol::ActionName::new("start").unwrap());
    grants.push(cap("model", &["next", "cancel"], &["deepseek"]));
    let harness = supervisor.boot(program("discovery"), grants).unwrap();
    let service = endpoint("discovery");
    assert!(supervisor.wait_until(TIMEOUT, |k| k.provider_of(&service).is_some_and(|(p, _)| p == harness)));
    let mut start = cap("discovery", &["start", "status"], &[]);
    start.delegable = false;
    let client = supervisor.boot(program("client"), vec![start]).unwrap();
    assert!(supervisor.wait_until(Duration::from_secs(120), |k| !k.is_live(client)));
    let nodes = lines(&scenario.run_dir("discovery", RUN).join("demo_a/nodes.jsonl"));
    assert!(
        nodes.iter().any(|n| n["verified"] == true),
        "one generation must survive multiple polls: {nodes:?}"
    );
    let requests = server.received();
    assert_eq!(requests.len(), 1, "exactly one paid generation");
    let body: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(body["stream"], true, "the provider receives a streaming request");
    // The adapter also uses HTTP streaming for `complete`, so its wire body
    // alone cannot establish that discovery selected the IPC streaming path.
    // Audit what the run's process delegated to its worker: a fallback to
    // complete-only would pass the HTTP assertion above.
    let process = run_process(&scenario, harness, RUN);
    let model_delegations: Vec<Vec<String>> = scenario
        .events()
        .into_iter()
        .filter_map(|event| match event {
            SecurityEvent::Delegated { from, endpoint, actions, .. }
                if from == process && endpoint.as_str() == "model" =>
            {
                Some(actions.into_iter().map(|a| a.as_str().to_owned()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        model_delegations,
        vec![vec!["complete", "start"], vec!["cancel", "next"]],
        "the worker receives both generation and separate stream-control authority"
    );
    assert!(supervisor.shutdown().is_clean());
}
/// A demo problem's own function, `{name}_f n = {expr}`, **irreducible** so
/// that no tactic of the `lean` service's fixed suites proves the theorem
/// `{name}_f n = n` outright: under task-scoring protocol v3
/// `complete` would otherwise turn every partial proof of a `simp`-provable
/// theorem into a solve and `analyze` would find every leaf hollow, and the
/// scripted rounds below would never happen. The proof has to unfold it.
fn function(name: &str, expr: &str) -> String {
    format!("import Mathlib\n\n@[irreducible] def {name}_f (n : Nat) : Nat := {expr}\n\n")
}

/// The frozen formalization: the function, the statement, proof `sorry`.
fn frozen(name: &str, expr: &str) -> String {
    format!("{}theorem {name} (n : Nat) : {name}_f n = n :=\nsorry\n", function(name, expr))
}

/// A partial proof that elaborates with one `sorry` left: a `have` whose
/// goal the suites can neither close (the function is opaque to them) nor
/// derive the theorem from (not hollow), so the node scores 0.5 with no
/// credited step and π₀ v3 may continue it.
fn partial(name: &str, expr: &str) -> String {
    format!(
        "{}theorem {name} (n : Nat) : {name}_f n = n := by\n  have h : {name}_f (n + 1) = 1 + n := by sorry\n  \
         unfold {name}_f\n  omega\n",
        function(name, expr)
    )
}

/// The complete proof; `verify` accepts it.
fn proof(name: &str, expr: &str) -> String {
    format!("{}theorem {name} (n : Nat) : {name}_f n = n :=\n by simp [{name}_f]\n", function(name, expr))
}

/// The model's reply carrying `file` in the ```lean block the worker extracts.
fn reply(file: &str) -> Scripted {
    Scripted::json(200, &completion(MODEL, &format!("Here is the complete file.\n```lean\n{file}```")))
}

/// What a worker's request body says about its problem: the rendered system
/// prompt quotes the statement, so `theorem <problem>` is in every request of
/// that problem's workers and in no other's. The fake model answers by it.
fn key(problem: &str) -> String {
    format!("theorem {problem}")
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ids(values: &Value) -> Vec<u64> {
    values.as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect()
}

/// The `batch` of every round record, in order.
fn batches(rounds: &[Value]) -> Vec<Vec<u64>> {
    rounds.iter().map(|r| ids(&r["batch"])).collect()
}

/// A worker's `result.json` (`<run>.workers/<problem>/<task>/`, beside the
/// journal directory), once it exists.
fn worker_result(scenario: &Scenario, run: &str, problem: &str, task: &str) -> Value {
    let path = scenario.workers_dir("discovery", run).join(problem).join(task).join("result.json");
    assert!(wait_for(TIMEOUT, || path.is_file()), "no result.json for {problem}/{task}");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// `PB_LEAN` and `NUCLEUS_REPL`; panics when either is missing, since every
/// test here is `#[ignore]`d for want of them.
fn lean_env() -> (PathBuf, PathBuf) {
    let pb = PathBuf::from(
        std::env::var_os("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"),
    );
    let repl = PathBuf::from(
        std::env::var_os("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL (check runs on it)"),
    );
    assert!(repl.is_file(), "NUCLEUS_REPL={} is not a file", repl.display());
    (pb, repl)
}

/// The repository's policy programs (host path), as the VM runtime ships
/// them: what a scenario binds at `/policies` to run them.
fn shipped_policies() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../program-files/discovery/policies").canonicalize().unwrap()
}

/// A task document (version 1) for `problems` under `policy` (`None`: the
/// built-in policy), with the loop bounds every test here uses and the prover's
/// default template as a file under the tasks directory.
fn task(name: &str, problems: &[&str], policy: Option<&str>) -> Value {
    let mut task = json!({
        "v": 1, "name": name, "problems": problems, "prompt_template_file": "prompts/putnam.j2",
        "concurrency": 2,
        "limits": {
            "w": 2, "k1": 3, "calls_per_continuation": 2, "fanout": 2,
            // Round 1 reserves 2 × 2 calls and is charged the workers' reported 4;
            // round 2 needs another 2 × 2 affordable: 10 funds both, and a third.
            "model_calls_per_problem": 10, "max_usd_per_problem": CEILING_USD,
        },
    });
    if let Some(policy) = policy {
        task["policy"] = json!(policy);
    }
    task
}

/// A task supplying its own statement: `id`'s theorem in `source`.
fn supplied(name: &str, id: &str, source: &str) -> Value {
    let mut task = task(name, &[], None);
    let object = task.as_object_mut().unwrap();
    object.remove("problems");
    object.insert("statements".into(), json!([{"id": id, "source": source}]));
    task
}

/// The service's deployment facts: every worker is the prover in CONTINUE
/// mode on the scripted model; one run at a time and three admitted at
/// most, each with at most two problems at once and two workers each; the
/// USD ceiling; then `extra` options. Scripted provider replies report the
/// actual cost. The kernel's default limits (4 calls in flight at an
/// endpoint, 16 pending per caller) leave room for these options.
fn service(scenario: &Scenario, policies: PathBuf, extra: &[&str]) -> DiscoveryService {
    let words = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_owned()).collect() };
    let limits = ["--max-concurrency", "2", "--max-width", "2", "--max-queued-runs", "3"];
    let mut options = words(&limits);
    options.extend(words(&["--max-usd-per-problem", CEILING_USD]));
    options.extend(words(extra));
    DiscoveryService {
        tasks: scenario.host("tasks"),
        policies,
        prover_args: words(&["--model", MODEL, "--max-attempts", "3", "--reasoning-effort", "off"]),
        extra: options,
    }
}

/// The world of every test here: the two frozen problems; the real `lean`
/// service on the PutnamBench workspace `pb` (with the REPL from
/// `NUCLEUS_REPL`); the scripted model on `server` behind the real OpenRouter
/// adapter; the prover's default template under the service's tasks
/// directory; and the discovery service with `policies` at its `/policies`.
fn world(pb: &Path, server: &FakeHttp, policies: PathBuf) -> Scenario {
    world_with(pb, server, policies, &[])
}

/// [`world`] with further service options.
fn world_with(pb: &Path, server: &FakeHttp, policies: PathBuf, extra: &[&str]) -> Scenario {
    let mut scenario = Scenario::new();
    scenario.write("problems/demo_a.lean", &frozen("demo_a", "n + 0"));
    scenario.write("problems/demo_b.lean", &frozen("demo_b", "0 + n"));
    std::fs::create_dir_all(scenario.host("tasks/prompts")).unwrap();
    std::fs::copy(prompts_dir().join("putnam.j2"), scenario.host("tasks/prompts/putnam.j2")).unwrap();
    scenario.add_lean("lean", pb, "problems");
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    let service = service(&scenario, policies, extra);
    scenario.add_discovery("discovery", &service);
    scenario
}

/// What the discovery program is granted here: [`discovery_grants`], whose
/// model capability is the hard cap the kernel shares across every worker of
/// every run (16 leaves no doubt that the task's own budget, not the
/// kernel's, is what bounds a run), and `shell exec` for the workers when
/// the scenario has the `shell` service.
fn service_grants(scenario: &Scenario) -> Vec<CapSpec> {
    let mut grants = discovery_grants(16);
    if scenario.config(|_| {}, 64).programs.contains_key(&program("shell")) {
        grants.push(cap("shell", &["exec"], &[]));
    }
    grants
}

/// Starts the supervisor, the services (`shell` too, when the scenario has it)
/// and the discovery service; returns once the latter provides both `discovery`
/// and `explore`. 16 calls in flight at an endpoint leave room for two running
/// runs as well.
fn boot(scenario: &Scenario) -> (Supervisor, ProcessId) {
    let supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    // The lean service imports Mathlib into its resident REPL before it provides.
    scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
    scenario.boot_service(&supervisor, "openrouter", "model");
    let grants = service_grants(scenario);
    if grants.iter().any(|g| g.endpoint.as_str() == "shell") {
        scenario.boot_service(&supervisor, "shell", "shell");
    }
    let harness = supervisor.boot(program("discovery"), grants).unwrap();
    for name in ["discovery", "explore"] {
        let target = endpoint(name);
        assert!(
            supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == harness)),
            "the service provides `{name}`"
        );
    }
    (supervisor, harness)
}

/// The process the service started last for `run`, and the run's key: the
/// process it delegated `explore` on `[key]` to, `<run>-<digest>` (the
/// run's key, 20 hexadecimal digits after the name).
fn run_process_key(scenario: &Scenario, harness: ProcessId, run: &str) -> (ProcessId, String) {
    let of_run = |key: &str| {
        key.strip_prefix(run)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|d| d.len() == 20 && d.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let started = scenario.events().into_iter().rev().find_map(|event| match event {
        SecurityEvent::Delegated { from, to, endpoint, resource, .. }
            if from == harness
                && endpoint.as_str() == "explore"
                && matches!(resource.segments(), [key] if of_run(key)) =>
        {
            Some((to, resource.segments()[0].clone()))
        }
        _ => None,
    });
    started.unwrap_or_else(|| panic!("the service started no process for run {run}"))
}

/// The process the service started last for `run`.
fn run_process(scenario: &Scenario, harness: ProcessId, run: &str) -> ProcessId {
    run_process_key(scenario, harness, run).0
}

/// The batches the built-in policy (π₀ v3) issues on the scripted run, per
/// problem: demo_a — `W` copies of the root in round 1, then the reserved slot
/// for the `sorries` leaf with the most credited steps (leaf 1, the earlier in
/// the ordering) and the rest by (score desc, depth asc, id asc), the root;
/// demo_b — `W` copies of the root, solved at once.
fn builtin_batches() -> (Vec<Vec<u64>>, Vec<Vec<u64>>) {
    (vec![vec![0, 0], vec![1, 0]], vec![vec![0, 0]])
}

/// The last record of every problem's `rounds.jsonl` carries a `reason`: the
/// run's rollouts have all ended.
fn rollouts_ended(run_dir: &Path) -> bool {
    PROBLEMS.iter().all(|p| {
        lines(&run_dir.join(p).join("rounds.jsonl")).last().is_some_and(|r| r.get("reason").is_some())
    })
}

/// The `rounds.jsonl` records of one run, per problem in curriculum order.
struct Rounds {
    a: Vec<Value>,
    b: Vec<Value>,
}

impl Rounds {
    fn all(&self) -> impl Iterator<Item = &Value> {
        self.a.iter().chain(&self.b)
    }
}

/// Who issues `discovery start`.
enum Driver {
    /// A fixture agent through the generic `call` verb: the refusals first,
    /// then the run, then `status` and `list`.
    Operator,
    /// The headless `nucleus-discovery-client` on a task file, polling
    /// `status` until the run ends.
    Client,
}

/// The scripted loop over both problems under `policy` (`None`: the built-in
/// policy; a path under `policies`), with every assertion about the task's
/// acceptance, the trees, the scores, the workers, the intruder and the
/// shutdown. The policy decides only which nodes to continue, so the run is
/// the same under any policy that makes the built-in decisions; what the
/// journal says about the policy is left to the caller.
fn scripted_loop(pb: &Path, policy: Option<&str>, policies: PathBuf, driver: &Driver) -> Rounds {
    let started = Instant::now();

    // The fake model's script is keyed by problem (the workers of both
    // problems run at once under concurrency 2) and, within a problem, laid
    // out per round so that the order in which its W = 2 workers — separate
    // processes racing for the queue — ask cannot matter: identical replies
    // where both must behave alike, and where they differ, a layout in which
    // whichever worker asks first stops after one call and the other consumes
    // the rest.
    let server = FakeHttp::start();
    let (a_partial, a_proof) = (partial("demo_a", "n + 0"), proof("demo_a", "n + 0"));
    let b_proof = proof("demo_b", "0 + n");
    // demo_a, round 1: two workers from the root, two calls each, every reply
    // the same partial proof — both submit it (a `sorry` left: 0.5 each).
    for _ in 0..4 {
        server.script_for(&key("demo_a"), reply(&a_partial));
    }
    // demo_a, round 2: the policy continues the root and the first leaf. The
    // worker that asks first gets the proof (complete after one call, so it
    // makes no second call); the other gets the partial proof twice.
    let winning_parent = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed_parent = std::sync::Arc::clone(&winning_parent);
    let scripted_proof = a_proof.clone();
    server.script_with(&key("demo_a"), move |request| {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap();
        let parent = system
            .split("continuing from node ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!([0, 1].contains(&parent), "only the scheduled parents may request this proof");
        *observed_parent.lock().unwrap() = Some(parent);
        reply(&scripted_proof)
    });
    for _ in 0..2 {
        server.script_for(&key("demo_a"), reply(&a_partial));
    }
    // demo_b, round 1: both workers get the proof at once.
    for _ in 0..2 {
        server.script_for(&key("demo_b"), reply(&b_proof));
    }

    let mut scenario = world(pb, &server, policies);
    let run_task = task(RUN, &PROBLEMS, policy);
    match driver {
        Driver::Operator => {
            let mut too_rich = task("too-rich", &PROBLEMS, policy);
            too_rich["limits"]["max_usd_per_problem"] = json!("20.5");
            scenario.add_agent(
                "operator",
                &json!({"turns": [{"tool_calls": [
                    discovery_call("start", None, too_rich),                     // 0: above the ceiling
                    discovery_call("start", None, task("unknown", &["demo_a", "demo_zzz"], policy)), // 1
                    {"tool": "await_operator", "signal": "go"},                  // 2
                    discovery_call("start", None, run_task.clone()),             // 3: the run
                    discovery_call("start", None, run_task),                     // 4: the name is taken
                    {"tool": "await_operator", "signal": "finished"},            // 5
                    discovery_call("status", Some(RUN), json!({})),              // 6
                    discovery_call("list", None, json!({})),                     // 7
                    discovery_call("status", Some("nope"), json!({})),           // 8: no such run
                    discovery_call("problems", Some(RUN), json!({})),            // 9
                    discovery_call_on("tree", &[RUN, "demo_a"], json!({})),      // 10
                    discovery_call_on("tree", &[RUN, "demo_zzz"], json!({})),    // 11: no such problem
                    discovery_call("problems", Some("nope"), json!({})),         // 12: no such run
                    discovery_call_on("tree", &[RUN], json!({})),                // 13: not [run, problem]
                ]}]}),
                None,
            );
        }
        Driver::Client => scenario.add_discovery_client("client", &run_task, 500, &[]),
    }

    // The negative case: a process holding `explore` on exactly [key, demo_a,
    // t000001] tries the other task, the other problem, and a wider path. Each
    // is refused by the kernel before the service is reached. Its in-scope call
    // reaches the run's process, which binds a task to the worker it spawned
    // for it and refuses anyone else — so nothing an outsider sends can become
    // a node. The run's key is known once its process is started, so the
    // intruder is made then.
    let forged = json!({"file": proof("demo_a", "n + 0"), "model_calls": 0,
                        "input_tokens": 0, "output_tokens": 0, "notes": "forged"});
    let submit = |path: &[&str]| {
        json!({"tool": "call", "endpoint": "explore", "action": "submit", "path": path,
               "payload": forged, "deadline_ms": 240_000})
    };
    let intruder_calls = |key: &str| {
        json!({"turns": [{"tool_calls": [
            submit(&[key, "demo_a", "t000002"]), // 0: another task of the same problem
            submit(&[key, "demo_b", "t000001"]), // 1: another problem
            {"tool": "call", "endpoint": "explore", "action": "assignment", "path": [key, "demo_a"],
             "payload": {}, "deadline_ms": 240_000}, // 2: a wider path than granted
            submit(&[key, "demo_a", "t000001"]), // 3: in scope for the kernel; not this task's worker
        ]}]})
    };
    scenario.add_agent("intruder", &intruder_calls("unknown-0"), None);

    let (mut supervisor, harness) = boot(&scenario);
    let run_dir = scenario.run_dir("discovery", RUN);
    let client = match driver {
        Driver::Operator => {
            let operator = supervisor.boot(program("operator"), discovery_caller_grants()).unwrap();
            // --- Refused before anything starts -------------------------------------
            let refused = scenario.wait_transcript("operator", 2);
            for (line, why) in refused.iter().zip(["exceeds the ceiling", "demo_zzz"]) {
                assert_eq!(error_code(line), Some("invalid_input"), "{line}");
                let message = line["result"]["error"]["message"].as_str().unwrap();
                assert!(message.starts_with("task: ") && message.contains(why), "{line}");
            }
            // The ceiling is the service's to check; the unknown problem is
            // Lean's, for the process the service started for that task and
            // stopped once it refused it.
            let processes = scenario.children_of(harness);
            assert_eq!(processes.len(), 1, "one process, for the task Lean refused");
            assert!(supervisor.wait_until(TIMEOUT, |s| s.cleanup_confirmed(processes[0])), "and it is gone");
            let project = scenario.out_dir("discovery").join(nucleus_e2e::DEFAULT_PROJECT);
            let entries: Vec<_> = std::fs::read_dir(project).into_iter().flatten().collect();
            assert!(entries.is_empty(), "no directory for a refused task: {entries:?}");
            scenario.signal("operator", "go");
            // The second `start` is answered once the run's process has
            // resolved its problems through Lean.
            assert!(wait_for(Duration::from_secs(300), || scenario.transcript("operator").len() >= 5));
            let started = scenario.transcript("operator");
            assert_eq!(
                ok_value(&started[3]),
                Some(&json!({"run": RUN, "problems": 2, "queued_ahead": 0})),
                "{:?}",
                started[3]
            );
            assert_eq!(error_code(&started[4]), Some("invalid_input"), "{:?}", started[4]);
            assert!(
                started[4]["result"]["error"]["message"].as_str().unwrap().contains("already exists"),
                "{:?}",
                started[4]
            );
            operator
        }
        Driver::Client => {
            // Exactly what the headless composition grants: `start`, and `status` to wait.
            let mut grant = cap("discovery", &["start", "status"], &[]);
            grant.delegable = false;
            supervisor.boot(program("client"), vec![grant]).unwrap()
        }
    };
    assert!(
        wait_for(Duration::from_secs(300), || scenario.run_dir("discovery", RUN).exists()),
        "the run was admitted"
    );
    let (_, key) = run_process_key(&scenario, harness, RUN);
    scenario.write("fixture/intruder.json", &intruder_calls(&key).to_string());
    let mut narrowed = cap("explore", &["assignment", "submit"], &[&key, "demo_a", "t000001"]);
    narrowed.delegable = false;
    let intruder = supervisor.boot(program("intruder"), vec![narrowed]).unwrap();

    // The run ends when every rollout has its final record; the service stays.
    assert!(wait_for(Duration::from_secs(600), || rollouts_ended(&run_dir)), "the run's rollouts ended");
    let loop_time = started.elapsed();
    assert!(supervisor.is_live(harness), "the service is resident: it does not exit with the run");

    // --- The accepted task, normalized -------------------------------------------
    let recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("task.json")).unwrap()).unwrap();
    assert_eq!((recorded["v"].as_u64(), recorded["name"].as_str()), (Some(1), Some(RUN)));
    assert_eq!(recorded["problems"], json!(PROBLEMS), "the problem list is inline: {recorded}");
    assert_eq!(
        recorded["prompt_template"].as_str().map(str::to_owned),
        Some(std::fs::read_to_string(prompts_dir().join("putnam.j2")).unwrap()),
        "the template is inline, verbatim"
    );
    assert!(recorded.get("prompt_template_file").is_none() && recorded.get("problems_file").is_none());
    assert_eq!(recorded["policy"], policy.map_or(Value::Null, Value::from), "{recorded}");
    assert_eq!(
        recorded["limits"],
        json!({"w": 2, "k1": 3, "calls_per_continuation": 2, "fanout": 2, "model_calls_per_problem": 10,
               "max_tokens_per_problem": 0, "max_usd_per_problem": CEILING_USD}),
        "every limit written out"
    );

    // --- The journal, per problem: nodes.jsonl and rounds.jsonl -------------
    for problem in PROBLEMS {
        let nodes = lines(&run_dir.join(problem).join("nodes.jsonl"));
        assert!(!nodes.is_empty(), "{problem}: nodes.jsonl exists and is not empty");
        let root = &nodes[0];
        assert_eq!(root["v"], 1);
        assert_eq!(
            (root["id"].as_u64(), root["parent"].is_null(), root["round"].as_u64()),
            (Some(0), true, Some(0))
        );
        assert!(
            root["task"].is_null() && root["worker"].is_null(),
            "the root has no task and no worker: {root}"
        );
        assert_eq!(root["score"], 0.5, "the frozen file elaborates with `sorry`: {root}");
        assert_eq!(root["check"]["status"], "sorries");
        assert_eq!(root["check"]["sorries"].as_array().unwrap().len(), 1);
        assert!(root["verified"].is_null(), "no verify without a complete check: {root}");
        assert_eq!(root["file"], frozen(problem, if problem == "demo_a" { "n + 0" } else { "0 + n" }));
        let mut last_ms = 0;
        for (i, node) in nodes.iter().enumerate().skip(1) {
            assert_eq!(node["id"].as_u64(), Some(i as u64), "ids are creation order: {node}");
            let parent = node["parent"].as_u64().unwrap();
            assert!(parent < i as u64, "a child's parent precedes it: {node}");
            assert!(node["task"].as_str().unwrap().starts_with('t'), "{node}");
            assert!(node["worker"]["process"].is_u64(), "{node}");
            assert!(node["round"].as_u64().unwrap() >= 1);
            assert!(
                node["ms_since_start"].as_u64().unwrap() >= last_ms,
                "ms_since_start is monotone: {nodes:?}"
            );
            last_ms = node["ms_since_start"].as_u64().unwrap();
        }
        let rounds = lines(&run_dir.join(problem).join("rounds.jsonl"));
        assert!(!rounds.is_empty(), "{problem}: rounds.jsonl");
        assert!(rounds.iter().all(|r| r["v"] == 1));
        assert!(rounds[..rounds.len() - 1].iter().all(|r| r.get("reason").is_none()), "{rounds:?}");
        assert_eq!(rounds.last().unwrap()["reason"], "solved", "{problem}: {rounds:?}");
        assert!(rounds.iter().all(|r| r.get("policy_error").is_none()), "{problem}: {rounds:?}");
        // Every child a round lists is a node created in that round, and vice versa.
        for round in &rounds {
            let r = round["round"].as_u64().unwrap();
            let listed: BTreeSet<u64> = ids(&round["children"]).into_iter().collect();
            let created: BTreeSet<u64> = nodes
                .iter()
                .filter(|n| n["round"].as_u64() == Some(r) && !n["parent"].is_null())
                .map(|n| n["id"].as_u64().unwrap())
                .collect();
            assert_eq!(listed, created, "{problem} round {r}");
            // Every continuation yields a child, except in the round that
            // solved the problem: the answer is in hand, so the round's other
            // workers are cut there and then rather than run to their caps.
            match round["reason"].as_str() {
                Some("solved") => {
                    for f in round.get("failed").and_then(|f| f.as_array()).unwrap_or(&vec![]) {
                        assert_eq!(f["why"], "solved_elsewhere", "{round}");
                    }
                }
                _ => assert!(round.get("failed").is_none(), "every continuation yielded a child: {round}"),
            }
        }
    }

    // demo_a: two rounds. Round 1 fans out W = 2 copies of the root; both
    // children are the partial proof (0.5, status `sorries`, not verified).
    let a_nodes = lines(&run_dir.join("demo_a").join("nodes.jsonl"));
    let a_rounds = lines(&run_dir.join("demo_a").join("rounds.jsonl"));
    // Four nodes, not five: round 2 solved it with its first continuation,
    // so the second was cut instead of producing a child of its own.
    assert_eq!((a_nodes.len(), a_rounds.len()), (4, 2), "{a_rounds:?}");
    assert_eq!(
        (a_rounds[0]["round"].as_u64(), ids(&a_rounds[0]["batch"]), ids(&a_rounds[0]["children"])),
        (Some(1), vec![0, 0], vec![1, 2])
    );
    for node in &a_nodes[1..3] {
        assert_eq!(
            (node["parent"].as_u64(), node["round"].as_u64(), node["score"].as_f64()),
            (Some(0), Some(1), Some(0.5)),
            "{node}"
        );
        assert_eq!(node["check"]["status"], "sorries");
        assert_eq!(node["check"]["sorries"].as_array().unwrap().len(), 1, "one goal left: {node}");
        assert!(
            node["check"]["sorries"][0]["goal"].as_str().unwrap().contains("⊢ demo_a_f (n + 1) = 1 + n"),
            "{node}"
        );
        assert!(node["verified"].is_null());
        assert_eq!(node["file"], a_partial.trim());
        assert_eq!(node["worker"]["model_calls"], 2, "both budgeted calls were spent: {node}");
    }
    // Round 2: π₀ v3 reserves its first slot for the `sorries` leaf with the
    // most credited steps — leaf 1 (or 2; the earlier in the ordering) — and
    // fills the rest by (score desc, depth asc, id asc): the root. One of the
    // two workers (whichever asked the model first) submitted the proof, which
    // `check` found complete and `verify` accepted: 1.0, the solve. The other
    // was cut where it stood rather than finishing a candidate nothing would
    // continue, and the loop stopped: `solved`.
    assert_eq!((a_rounds[1]["round"].as_u64(), ids(&a_rounds[1]["batch"])), (Some(2), vec![1, 0]));
    assert_eq!(ids(&a_rounds[1]["children"]).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([3]));
    let round2: Vec<&Value> = a_nodes[3..].iter().collect();
    let parents: BTreeSet<u64> = round2.iter().map(|n| n["parent"].as_u64().unwrap()).collect();
    assert_eq!(
        parents,
        BTreeSet::from([winning_parent.lock().unwrap().unwrap()]),
        "the solved node belongs to the request that received the proof"
    );
    let solved: Vec<&&Value> = round2.iter().filter(|n| n["score"] == 1.0).collect();
    assert_eq!(solved.len(), 1, "{round2:?}");
    let a_solved = solved[0];
    assert_eq!(
        (a_solved["check"]["status"].as_str(), a_solved["verified"].as_bool()),
        (Some("complete"), Some(true))
    );
    assert!(a_solved.get("verify_rejection").is_none(), "{a_solved}");
    assert_eq!(a_solved["file"], a_proof.trim(), "the winning node's file is the correct proof");
    assert_eq!(a_solved["worker"]["model_calls"], 1, "complete after one call: {a_solved}");

    assert_eq!(a_rounds[1]["reason"], "solved");

    // demo_b: one round. Both continuations of the root would have proved it,
    // but the first to submit ends the round, so the tree holds the root and
    // one verified child.
    let b_nodes = lines(&run_dir.join("demo_b").join("nodes.jsonl"));
    let b_rounds = lines(&run_dir.join("demo_b").join("rounds.jsonl"));
    assert_eq!((b_nodes.len(), b_rounds.len()), (2, 1), "{b_rounds:?}");
    assert_eq!(
        (ids(&b_rounds[0]["batch"]), ids(&b_rounds[0]["children"]), b_rounds[0]["reason"].as_str()),
        (vec![0, 0], vec![1], Some("solved"))
    );
    for node in &b_nodes[1..] {
        assert_eq!(
            (node["parent"].as_u64(), node["score"].as_f64(), node["verified"].as_bool()),
            (Some(0), Some(1.0), Some(true)),
            "{node}"
        );
        assert_eq!(node["check"]["status"], "complete");
        assert_eq!(node["file"], b_proof.trim());
        assert_eq!(node["worker"]["model_calls"], 1);
    }
    // The decisions, as one sequence: what the built-in policy issues here.
    assert_eq!((batches(&a_rounds), batches(&b_rounds)), builtin_batches());

    // --- The workers: one confined process per continuation ------------------
    let events_log = lines(&run_dir.join("discovery.jsonl"));
    let mut spawned: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in events_log.iter().filter(|r| r["event"] == "spawned") {
        spawned
            .entry(record["problem"].as_str().unwrap().to_owned())
            .or_default()
            .push(record["task"].as_str().unwrap().to_owned());
    }
    // Both rollouts ran at once, so which problem got which id is timing; the
    // ids are one sequence for the run and four of them are demo_a's.
    let all_tasks: BTreeSet<&str> = spawned.values().flatten().map(String::as_str).collect();
    assert_eq!(
        all_tasks,
        BTreeSet::from(["t000001", "t000002", "t000003", "t000004", "t000005", "t000006"]),
        "one task id per continuation, one sequence for the run: {events_log:?}"
    );
    assert_eq!((spawned["demo_a"].len(), spawned["demo_b"].len()), (4, 2), "{spawned:?}");
    assert!(
        spawned["demo_a"].iter().any(|t| t == "t000001"),
        "demo_a's rollout is stepped first (the intruder's scope assumes it): {spawned:?}"
    );
    assert!(events_log.iter().all(|r| r["event"] != "spawn_refused"), "{events_log:?}");
    let events: Vec<&str> = events_log.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert!(
        events.iter().all(|e| matches!(*e, "spawned" | "exited" | "killed" | "kill_refused")),
        "{events:?}"
    );
    // A worker exits as soon as its submission is answered, and the run's
    // process kills only a worker the kernel still lists as live once the
    // round is over, so a `killed` here would mean a worker that lingered.
    eprintln!("discovery.jsonl events: {events:?}");
    let process = run_process(&scenario, harness, RUN);
    assert_eq!(scenario.children_of(process).len(), 6, "six workers, kernel processes of the run's process");
    // Every worker that was left to finish submitted exactly once and says
    // so. A worker cut because its round-mate solved the problem is killed
    // where it stands and writes no result of its own; the round record says
    // `solved_elsewhere` for it, which is where that is accounted.
    let cut: BTreeSet<String> = ["demo_a", "demo_b"]
        .iter()
        .flat_map(|p| lines(&run_dir.join(p).join("rounds.jsonl")))
        .flat_map(|r| {
            r.get("failed")
                .and_then(|f| f.as_array())
                .map(|fs| fs.iter().filter_map(|f| f["task"].as_str().map(str::to_owned)).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    assert!(!cut.is_empty(), "the solve cut at least one round-mate");
    for (problem, tasks) in &spawned {
        for task in tasks.iter().filter(|t| !cut.contains(*t)) {
            let result = worker_result(&scenario, RUN, problem, task);
            assert_eq!(
                (result["mode"].as_str(), result["problem"].as_str(), result["task"].as_str()),
                (Some("continue"), Some(problem.as_str()), Some(task.as_str())),
                "{result}"
            );
            assert_eq!(result["submitted"], true, "{result}");
            assert!(result.get("error").is_none(), "{result}");
        }
    }
    let winner = worker_result(&scenario, RUN, "demo_a", a_solved["task"].as_str().unwrap());
    assert_eq!(
        (winner["stopped"].as_str(), winner["best_status"].as_str(), winner["attempts"].as_u64()),
        (Some("complete"), Some("complete"), Some(1))
    );
    assert_eq!(winner["file"], a_solved["file"]);
    // A worker's task and problem are its authority, delegated by the run's
    // process and narrowed to one task.
    let task_grants = scenario
        .events()
        .iter()
        .filter(|e| matches!(e, SecurityEvent::Delegated { from, endpoint, .. } if *from == process && endpoint.as_str() == "explore"))
        .count();
    assert_eq!(task_grants, 6, "one `explore` delegation per worker");
    assert_eq!(server.received().len(), 9, "exactly the scripted model calls were made");

    // --- What the client sees ---------------------------------------------------
    match driver {
        Driver::Operator => {
            scenario.signal("operator", "finished");
            let transcript = scenario.wait_done_within("operator", Duration::from_secs(60));
            assert_eq!(transcript.len(), 14, "{transcript:?}");
            let status = ok_value(&transcript[6]).expect("status").clone();
            // Every completion reports 9 prompt and 4 completion tokens; six
            // calls over both rollouts, summed by the service. A solve ends its
            // own round, so a cut round-mate makes none of its three calls.
            assert_eq!(
                status,
                json!({"run": RUN, "state": "done", "problems": 2, "finished": 2, "solved": 2, "active": [],
                       "tokens_in": 54, "tokens_out": 24, "usd": status["usd"],
                       "task": status["task"], "started_at_ms": status["started_at_ms"],
                       "failed_continuations": status["failed_continuations"]}),
                "{status}"
            );
            assert!(status["usd"].as_f64().is_some_and(|usd| usd > 0.0), "priced: {status}");
            // The task's shape travels with the status: the limits as recorded
            // in task.json, the policy program (null for the built-in), the
            // concurrency, whether its workers have network (the service's
            // default, off, since the task did not choose); and when the run
            // began, on the harness's clock.
            let recorded: Value =
                serde_json::from_str(&std::fs::read_to_string(run_dir.join("task.json")).unwrap()).unwrap();
            assert_eq!(recorded["worker_network"], false, "the run records what it ran with");
            assert_eq!(
                status["task"],
                json!({"policy": recorded["policy"], "concurrency": 2, "limits": recorded["limits"],
                       "worker_network": false}),
                "{status}"
            );
            assert!(status["started_at_ms"].as_u64().is_some_and(|ms| ms > 1_700_000_000_000), "{status}");
            let listed = ok_value(&transcript[7]).expect("list");
            let runs = listed["runs"].as_array().unwrap();
            assert_eq!(runs.len(), 1, "{listed}");
            assert!(runs[0].get("active").is_none(), "a summary has no active list: {listed}");
            let mut summary = status.clone();
            for field in ["active", "task", "started_at_ms"] {
                summary.as_object_mut().unwrap().remove(field);
            }
            summary["project"] = json!(nucleus_e2e::DEFAULT_PROJECT);
            assert_eq!(
                runs[0], summary,
                "a summary is the status without the active list and the task, and names its project: {listed}"
            );
            assert_eq!(error_code(&transcript[8]), Some("invalid_input"), "no such run: {:?}", transcript[8]);

            // --- `problems`: one row per problem, in task order, from the journal ---
            // demo_a spent 7 of the 9 calls (4 in round 1, 3 in round 2), demo_b 2;
            // every call reports 9 prompt and 4 completion tokens.
            let problems = ok_value(&transcript[9]).expect("problems").clone();
            assert_eq!(problems["run"], RUN);
            assert_eq!(
                problems["problems"],
                json!([
                    {"problem": "demo_a", "state": "done", "best": 1.0, "rounds": 2, "nodes": 3,
                     "tokens_in": 45, "tokens_out": 20, "stop": "solved", "solved": true},
                    {"problem": "demo_b", "state": "done", "best": 1.0, "rounds": 1, "nodes": 1,
                     "tokens_in": 9, "tokens_out": 4, "stop": "solved", "solved": true},
                ]),
                "{problems}"
            );

            // --- `tree`: the recorded tree of demo_a, node for node and round for round ---
            let tree = ok_value(&transcript[10]).expect("tree").clone();
            assert_eq!(
                (tree["run"].as_str(), tree["problem"].as_str(), tree["state"].as_str(), &tree["truncated"]),
                (Some(RUN), Some("demo_a"), Some("done"), &json!(false)),
                "{tree}"
            );
            assert_eq!(tree["policy"], a_rounds[0]["policy"], "the policy that produced the tree");
            assert_eq!(
                tree["policy"]["kind"],
                if policy.is_some() { "process" } else { "builtin" },
                "{tree}"
            );
            let t_nodes = tree["nodes"].as_array().unwrap();
            assert_eq!(t_nodes.len(), a_nodes.len(), "{tree}");
            for (node, recorded) in t_nodes.iter().zip(&a_nodes) {
                for field in ["id", "parent", "round", "score", "verified", "ms_since_start"] {
                    assert_eq!(node[field], recorded[field], "{field} of {node} vs {recorded}");
                }
                assert!(node.get("file").is_none(), "a tree node carries an excerpt, not the file: {node}");
                assert_eq!(node["check"]["status"], recorded["check"]["status"]);
                assert_eq!(
                    node["check"]["sorries"].as_u64(),
                    recorded["check"]["sorries"].as_array().map(|s| s.len() as u64)
                );
                assert_eq!(node["check"]["errors"], 0);
                if recorded["worker"].is_null() {
                    assert!(node["worker"].is_null(), "{node}");
                } else {
                    assert_eq!(node["worker"]["task"], recorded["task"], "{node}");
                    for field in ["process", "model_calls", "input_tokens", "output_tokens"] {
                        assert_eq!(node["worker"][field], recorded["worker"][field], "{field}: {node}");
                    }
                }
                assert!(node["excerpt"].as_str().unwrap().chars().count() <= 200, "{node}");
            }
            // The root's excerpt is the head of the frozen statement; a child's
            // starts at the first line that differs from its parent.
            assert_eq!(t_nodes[0]["excerpt"], frozen("demo_a", "n + 0").trim_end());
            for node in &t_nodes[1..3] {
                assert_eq!(
                    node["excerpt"],
                    "theorem demo_a (n : Nat) : demo_a_f n = n := by\n  have h : demo_a_f (n + 1) = 1 + n := by sorry\n  \
                     unfold demo_a_f\n  omega",
                    "{node}"
                );
            }
            let t_solved = t_nodes.iter().find(|n| n["score"] == 1.0).unwrap();
            assert_eq!(
                (t_solved["verified"].as_bool(), t_solved["check"]["status"].as_str()),
                (Some(true), Some("complete"))
            );
            assert!(
                t_solved["excerpt"].as_str().unwrap().ends_with(" by simp [demo_a_f]")
                    && !t_solved["excerpt"].as_str().unwrap().contains("import"),
                "{t_solved}"
            );
            let t_rounds = tree["rounds"].as_array().unwrap();
            assert_eq!(t_rounds.len(), 2, "{tree}");
            for (round, recorded) in t_rounds.iter().zip(&a_rounds) {
                for field in ["round", "batch", "children", "tokens_in", "tokens_out", "usd"] {
                    assert_eq!(round[field], recorded[field], "{field} of {round} vs {recorded}");
                }
                // The live view agrees with the journal, cuts included: the
                // round that solved the problem lists the round-mate it
                // stopped.
                assert_eq!(round["failed"], recorded.get("failed").cloned().unwrap_or(json!([])));
                assert!(round["policy_error"].is_null() && round.get("policy").is_none(), "{round}");
            }
            assert_eq!((t_rounds[0]["round"].as_u64(), ids(&t_rounds[0]["batch"])), (Some(1), vec![0, 0]));
            assert!(t_rounds[0]["reason"].is_null(), "{}", t_rounds[0]);
            assert_eq!(
                (ids(&t_rounds[1]["batch"]), t_rounds[1]["reason"].as_str()),
                (vec![1, 0], Some("solved"))
            );
            assert_eq!(
                (&t_rounds[0]["tokens_in"], &t_rounds[1]["tokens_in"]),
                (&json!(36), &json!(45)),
                "cumulative; round 2 stopped as soon as it had the proof"
            );

            // --- Refusals: structured, before anything is read ---
            for (i, why) in [(11, "demo_zzz"), (12, "no such run"), (13, "[project, run, problem]")] {
                assert_eq!(error_code(&transcript[i]), Some("invalid_input"), "{:?}", transcript[i]);
                let message = transcript[i]["result"]["error"]["message"].as_str().unwrap();
                assert!(message.contains(why), "{i}: {message}");
            }
        }
        Driver::Client => {
            // The client polls `status` until `done`, then exits.
            assert!(
                supervisor.wait_until(Duration::from_secs(60), |s| !s.is_live(client)),
                "the client exits"
            );
        }
    }
    let run_dirs: BTreeSet<String> =
        std::fs::read_dir(scenario.out_dir("discovery").join(nucleus_e2e::DEFAULT_PROJECT))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
    assert_eq!(
        run_dirs,
        BTreeSet::from([RUN.to_owned(), format!("{RUN}.workers")]),
        "only the accepted run has directories: its journal and, beside it, its workers'"
    );
    let journal_subdirs: BTreeSet<String> = std::fs::read_dir(&run_dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        journal_subdirs,
        PROBLEMS.iter().map(|p| (*p).to_owned()).collect(),
        "the journal directory's subdirectories are the problems and nothing else"
    );

    // --- The negative case -----------------------------------------------------
    let refused = scenario.wait_done_within("intruder", Duration::from_secs(300));
    assert_eq!(refused.len(), 4, "{refused:?}");
    for line in &refused[..3] {
        assert_eq!(error_code(line), Some("permission_denied"), "{line}");
    }
    let denied_by_kernel = scenario
        .events()
        .iter()
        .filter(|e| matches!(e, SecurityEvent::Denied { process, endpoint, reason, .. }
            if *process == intruder && endpoint.as_ref().is_some_and(|ep| ep.as_str() == "explore") && *reason == ErrorCode::PermissionDenied))
        .count();
    assert_eq!(denied_by_kernel, 3, "the kernel refused the out-of-scope calls before the service saw them");
    // In scope for the kernel, but the service binds t000001 to the process it
    // spawned for it (or the task is already closed): refused either way.
    assert!(
        matches!(error_code(&refused[3]), Some("permission_denied") | Some("invalid_input")),
        "an outsider cannot submit into another worker's task: {:?}",
        refused[3]
    );
    assert!(
        a_nodes.iter().all(|n| n["worker"]["process"].as_u64() != Some(intruder.0)),
        "nothing the intruder sent became a node"
    );
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(intruder)));

    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
    eprintln!("discovery loop: {loop_time:?} from start to the run's last record");
    Rounds { a: a_rounds, b: b_rounds }
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_started_task_grows_a_scored_tree_per_problem_and_stops_when_solved() {
    let (pb, _repl) = lean_env();
    let policies = tempfile::tempdir().unwrap();
    let rounds = scripted_loop(&pb, None, policies.path().to_path_buf(), &Driver::Operator);
    // Every round record names the policy that produced the tree.
    for round in rounds.all() {
        assert_eq!(round["policy"], json!({"kind": "builtin", "program": null}), "{round}");
    }
}

/// The task names a shipped policy program under the service's policies
/// directory, and the headless client starts it: the service runs the
/// exploration policy as a Python process inside its own bubblewrap sandbox —
/// `/usr/bin/python3` from the backend's read-only `/usr`, the program from
/// the read-only `/policies` bind, an empty environment, no kernel connection
/// — asks it every decision over the line protocol, and journals it as the
/// policy of the tree. `pi0.py` makes the built-in decisions; so does
/// `pi_doubling.py` at this width (one π₀ slot plus one fresh root), which
/// imports `pi0.py` from beside itself inside that sandbox. The trees are the
/// same.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn the_headless_client_runs_a_task_whose_policy_is_a_confined_python_process() {
    let (pb, _repl) = lean_env();
    let policies = shipped_policies();
    for name in ["pi0.py", "pi_doubling.py"] {
        let rounds = scripted_loop(&pb, Some(name), policies.clone(), &Driver::Client);
        // The journal names the program as the confined service saw it: the
        // in-sandbox path (`/policies/<name>` under bubblewrap), never the host path.
        let program = format!("/policies/{name}");
        for round in rounds.all() {
            assert_eq!(round["policy"], json!({"kind": "process", "program": program}), "{name}: {round}");
        }
        assert_eq!(
            (batches(&rounds.a), batches(&rounds.b)),
            builtin_batches(),
            "{name}: the policy process issues exactly the built-in policy's batches"
        );
    }
}

/// A task naming a policy program that exits at once: every rollout stops
/// honestly as `policy_failed`, the built-in policy is not substituted, no
/// worker is spawned and no model call is made; the run still ends `done`
/// with nothing solved, and the service goes on serving.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_failing_policy_process_stops_every_rollout_honestly() {
    let (pb, _repl) = lean_env();
    // Nothing is scripted: no worker should ever ask the model.
    let server = FakeHttp::start();
    let policies = tempfile::tempdir().unwrap();
    // Not a `.py`, so the service executes it directly; `/bin/sh` is the
    // sandbox's symlink into the read-only `/usr`.
    std::fs::write(policies.path().join("exit3.sh"), "#!/bin/sh\nexit 3\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(policies.path().join("exit3.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    scenario.add_agent(
        "operator",
        &json!({"turns": [{"tool_calls": [
            discovery_call("start", None, task(RUN, &PROBLEMS, Some("exit3.sh"))),
            {"tool": "await_operator", "signal": "finished"},
            discovery_call("status", Some(RUN), json!({})),
        ]}]}),
        None,
    );
    let program_seen = "/policies/exit3.sh".to_owned();

    let (mut supervisor, harness) = boot(&scenario);
    supervisor.boot(program("operator"), discovery_caller_grants()).unwrap();
    let run_dir = scenario.run_dir("discovery", RUN);
    assert!(wait_for(Duration::from_secs(300), || rollouts_ended(&run_dir)), "the run's rollouts ended");

    assert!(run_dir.join("task.json").is_file(), "the task was accepted");
    for problem in PROBLEMS {
        let nodes = lines(&run_dir.join(problem).join("nodes.jsonl"));
        assert_eq!(nodes.len(), 1, "{problem}: the root was evaluated and nothing else: {nodes:?}");
        assert_eq!((nodes[0]["id"].as_u64(), nodes[0]["score"].as_f64()), (Some(0), Some(0.5)), "{nodes:?}");
        let rounds = lines(&run_dir.join(problem).join("rounds.jsonl"));
        assert_eq!(rounds.len(), 1, "{problem}: one record, the honest stop: {rounds:?}");
        let last = &rounds[0];
        assert_eq!(last["v"], 1);
        assert_eq!(last["policy"], json!({"kind": "process", "program": program_seen}), "{last}");
        assert_eq!(
            (last["round"].as_u64(), ids(&last["batch"]), ids(&last["children"])),
            (Some(0), vec![], vec![]),
            "no decision was taken and no child recorded: {last}"
        );
        assert_eq!(last["reason"], "policy_failed", "{last}");
        let error = last["policy_error"].as_str().unwrap_or_default();
        assert!(!error.is_empty(), "how the policy failed is recorded: {last}");
        eprintln!("{problem}: policy_error = {error}");
    }
    // No worker: nothing spawned, no kernel child of the service, no model call.
    assert!(lines(&run_dir.join("discovery.jsonl")).is_empty(), "no worker was spawned");
    let process = run_process(&scenario, harness, RUN);
    assert!(scenario.children_of(process).is_empty(), "the run's process had no kernel children");
    assert_eq!(server.received().len(), 0, "no model call was made");

    scenario.signal("operator", "finished");
    let transcript = scenario.wait_done_within("operator", Duration::from_secs(60));
    let status = ok_value(&transcript[2]).expect("status").clone();
    assert_eq!(
        status,
        json!({"run": RUN, "state": "done", "problems": 2, "finished": 2, "solved": 0, "active": [],
               "tokens_in": 0, "tokens_out": 0,
               "task": status["task"], "started_at_ms": status["started_at_ms"],
                       "failed_continuations": status["failed_continuations"],
                       "failure_reasons": status["failure_reasons"]}),
        "{status}"
    );
    assert_eq!(status["task"]["policy"], json!("exit3.sh"), "the failing policy, named: {status}");
    // What keeps going wrong: one reason, the policy's failure, on both problems.
    let reasons = status["failure_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 1, "{status}");
    assert_eq!((&reasons[0]["kind"], &reasons[0]["count"]), (&json!("stop"), &json!(2)), "{status}");
    assert!(reasons[0]["reason"].as_str().unwrap().starts_with("policy_failed: "), "{status}");
    let mut problems: Vec<&str> =
        reasons[0]["problems"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    problems.sort_unstable();
    assert_eq!(problems, PROBLEMS, "{status}");
    assert!(supervisor.is_live(harness), "the service is still serving");
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

/// The standard a run is judged against is registered with the real `lean`
/// service before the run exists, and again on `resume`. A theorem declared
/// inside a namespace elaborates, but `verify`'s own report commands, which
/// registration appends after the whole file, cannot see it: `start` is
/// refused with Lean's own words and the hint, and no directory is created.
/// A good statement is accepted. Once its run is stopped, a `resume` whose
/// saved statement was changed on disk is refused as well: the service
/// registers again rather than trusting what it registered before.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn statements_are_registered_with_lean_before_a_run_starts_or_resumes() {
    let (pb, _repl) = lean_env();
    let server = FakeHttp::start();
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    let namespaced = "import Mathlib\n\nnamespace Demo\n\ntheorem demo_ns (n : Nat) : n + 0 = n := by\n  sorry\n\nend Demo\n";
    let good = frozen("demo_ok", "n + 0");
    scenario.add_agent(
        "operator",
        &json!({"turns": [{"tool_calls": [
            discovery_call("start", None, supplied("namespaced", "demo_ns", namespaced)), // 0: Lean refuses it
            discovery_call("start", None, supplied("kept", "demo_ok", &good)),            // 1: accepted
            discovery_call("stop", Some("kept"), json!({})),                               // 2
            {"tool": "await_operator", "signal": "tampered"},                              // 3
            discovery_call("resume", Some("kept"), json!({})),                             // 4: refused
        ]}]}),
        None,
    );
    let (mut supervisor, harness) = boot(&scenario);
    let mut grant = cap("discovery", &["start", "stop", "resume"], &[]);
    grant.delegable = false;
    supervisor.boot(program("operator"), vec![grant]).unwrap();

    // Each registration is one cold Lean run of the file.
    assert!(wait_for(Duration::from_secs(600), || scenario.transcript("operator").len() >= 3));
    let started = scenario.transcript("operator");
    assert_eq!(error_code(&started[0]), Some("invalid_input"), "{:?}", started[0]);
    let said = started[0]["result"]["error"]["message"].as_str().unwrap();
    assert!(said.starts_with("statement \"demo_ns\": Lean says:"), "{said}");
    assert!(said.to_lowercase().contains("unknown constant"), "Lean's own words: {said}");
    assert!(said.contains("at top level"), "the hint for an error on an appended line: {said}");
    assert!(!scenario.run_dir("discovery", "namespaced").exists(), "a refused start creates nothing");
    assert!(!scenario.workers_dir("discovery", "namespaced").exists());
    assert_eq!(ok_value(&started[1]).and_then(|r| r["run"].as_str()), Some("kept"), "{:?}", started[1]);
    assert!(ok_value(&started[2]).is_some(), "{:?}", started[2]);

    let task_file = scenario.run_dir("discovery", "kept").join("task.json");
    let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&task_file).unwrap()).unwrap();
    assert_eq!(saved["statements"][0]["source"], json!(good), "the task records the registered text");
    saved["statements"][0]["source"] = json!(good.replace("n + 0", "0 + n"));
    std::fs::write(&task_file, serde_json::to_string_pretty(&saved).unwrap()).unwrap();
    // A run is resumed once the kernel has confirmed its last process gone.
    let kept = run_process(&scenario, harness, "kept");
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s.cleanup_confirmed(kept)),
        "the stopped run's process is gone"
    );
    scenario.signal("operator", "tampered");
    let transcript = scenario.wait_done_within("operator", Duration::from_secs(300));
    assert_eq!(error_code(&transcript[4]), Some("invalid_input"), "{:?}", transcript[4]);
    assert!(
        transcript[4]["result"]["error"]["message"].as_str().unwrap().contains("statement \"demo_ok\""),
        "{:?}",
        transcript[4]
    );
    assert!(
        !scenario.run_dir("discovery", "kept").join("resume.jsonl").exists(),
        "a refused resume admits nothing"
    );
    assert!(supervisor.shutdown().is_clean());
}
