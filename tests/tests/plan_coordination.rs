//! Plan coordination through real confined processes (the explore interface's
//! `publish_plan` and `read_plans`): the discovery service, two prover workers
//! of one round in the shell tool loop, the real `lean` and `shell` services,
//! and a scripted model behind the real OpenRouter adapter. What is under test
//! is the service's side: a round of `w` = 2 is granted the plan tools, each
//! worker publishes into its own slot, and the service assigns owner, time,
//! order and revision, so the second worker's board shows the first's proposal
//! before its own. The model's decisions are scripted; no paid API.
//!
//! Opt-in: needs `PB_LEAN` (the PutnamBench `lean4` workspace) and
//! `NUCLEUS_REPL`, as the discovery tests do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nucleus_e2e::http::{FakeHttp, Received, Scripted, tool_request};
use nucleus_e2e::{
    DiscoveryService, Scenario, TIMEOUT, cap, discovery_grants, endpoint, program, prompts_dir,
};
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";
const RUN: &str = "plan-coordination";
const PROBLEM: &str = "demo_plan";
const PUBLISH: &str = "explore_publish_plan";
const READ: &str = "explore_read_plans";
const FROZEN: &str = "import Mathlib\n\n@[irreducible] def demo_plan_f (n : Nat) : Nat := n + 0\n\ntheorem demo_plan (n : Nat) : demo_plan_f n = n :=\nsorry\n";

fn lean_env() -> PathBuf {
    let pb = PathBuf::from(
        std::env::var_os("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"),
    );
    let repl = PathBuf::from(std::env::var_os("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL"));
    assert!(repl.is_file(), "NUCLEUS_REPL={} is not a file", repl.display());
    pb
}

fn plan(approach: &str) -> Value {
    json!({"goal": "Prove the frozen identity from its defining equation.", "approach": approach,
           "next_step": "Unfold the definition and inspect the arithmetic."})
}

/// The tool result the model was given for `call_id`, as JSON.
fn tool_result(request: &Received, call_id: &str) -> Value {
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("no result for {call_id}"))
        .clone();
    // A harness hint may follow the JSON result.
    serde_json::Deserializer::from_str(message["content"].as_str().unwrap())
        .into_iter::<Value>()
        .next()
        .unwrap()
        .unwrap()
}

fn tool_names(request: &Received) -> Vec<String> {
    let body: Value = serde_json::from_str(&request.body).unwrap();
    body["tools"].as_array().map_or_else(Vec::new, |tools| {
        tools.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_owned)).collect()
    })
}

/// Stops a worker: the provider refuses for want of credit.
fn funding_stop() -> Scripted {
    Scripted::json(402, &json!({"error": {"code": 402, "message": "fixture stop: insufficient credits"}}))
}

/// What each worker saw, in the order the callbacks ran.
#[derive(Default)]
struct Seen {
    first_published: bool,
    boards: Vec<(String, Value)>,
    tools: Vec<Vec<String>>,
    read: Option<Value>,
}

type Shared = Arc<(Mutex<Seen>, Condvar)>;

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn the_workers_of_a_round_publish_into_their_own_slots_and_see_earlier_plans_first() {
    let pb = lean_env();
    let server = FakeHttp::start();
    let seen = Shared::default();
    // Follow-ups are routed by the call id each worker's first reply gave
    // it; the empty key takes every first request, in arrival order.
    for (call, approach) in [("a-publish", "unfold"), ("b-publish", "induction")] {
        let shared = Arc::clone(&seen);
        server.script_with(call, move |request| {
            let board = tool_result(request, call);
            let (lock, changed) = &*shared;
            let mut state = lock.lock().unwrap();
            state.boards.push((approach.to_owned(), board));
            state.first_published = true;
            changed.notify_all();
            // The first worker then reads the board with an argument the tool
            // does not take, as some models add one to every call.
            if call == "a-publish" {
                return Scripted::json(200, &tool_request(MODEL, &[("a-read", READ, json!({"x": 1}))]));
            }
            funding_stop()
        });
    }
    let shared = Arc::clone(&seen);
    server.script_with("a-read", move |request| {
        shared.0.lock().unwrap().read = Some(tool_result(request, "a-read"));
        funding_stop()
    });
    for (call, approach) in [("a-publish", "unfold"), ("b-publish", "induction")] {
        let shared = Arc::clone(&seen);
        server.script_with("", move |request| {
            let (lock, changed) = &*shared;
            let mut state = lock.lock().unwrap();
            state.tools.push(tool_names(request));
            // The second worker publishes only once the first one's
            // proposal is on the board, so the order is not a race.
            if call == "b-publish" {
                state = changed.wait_timeout_while(state, TIMEOUT * 6, |s| !s.first_published).unwrap().0;
                assert!(state.first_published, "the first worker never published");
            }
            drop(state);
            Scripted::json(200, &tool_request(MODEL, &[(call, PUBLISH, plan(approach))]))
        });
    }

    let mut scenario = Scenario::new();
    scenario.write(&format!("problems/{PROBLEM}.lean"), FROZEN);
    std::fs::create_dir_all(scenario.host("tasks/prompts")).unwrap();
    std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
        .unwrap();
    scenario.add_lean("lean", &pb, "problems");
    scenario.add_shell("shell", &pb);
    scenario.add_openrouter("openrouter", &server.base_url(), "sk-or-test-PLANS-0123456789", &[], true);
    let words = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_owned()).collect() };
    let policies = tempfile::tempdir().unwrap();
    scenario.add_discovery(
        "discovery",
        &DiscoveryService {
            tasks: scenario.host("tasks"),
            policies: policies.path().to_path_buf(),
            prover_args: words(&["--model", MODEL, "--max-attempts", "3", "--model-retries", "0"]),
            // One run of two workers, well within the kernel limits below.
            extra: words(&[
                "--max-concurrency",
                "1",
                "--max-width",
                "2",
                "--max-queued-runs",
                "1",
                "--max-usd-per-problem",
                "20",
                "--provider-retries",
                "0",
            ]),
        },
    );
    scenario.add_discovery_client(
        "client",
        &json!({
            "v": 1, "name": RUN, "problems": [PROBLEM], "prompt_template_file": "prompts/prover-agent.j2",
            "concurrency": 1,
            "limits": {"w": 2, "k1": 1, "fanout": 2, "calls_per_continuation": 3,
                       "model_calls_per_problem": 16, "max_usd_per_problem": "20"}
        }),
        100,
        &[],
    );
    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
    scenario.boot_service(&supervisor, "shell", "shell");
    scenario.boot_service(&supervisor, "openrouter", "model");
    let mut grants = discovery_grants(16);
    grants.push(cap("shell", &["exec"], &[]));
    let harness = supervisor.boot(program("discovery"), grants).unwrap();
    let target = endpoint("discovery");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == harness)));
    let client =
        supervisor.boot(program("client"), vec![cap("discovery", &["start", "status"], &[])]).unwrap();
    assert!(supervisor.wait_until(Duration::from_secs(300), |s| !s.is_live(client)));

    let state = seen.0.lock().unwrap();
    assert_eq!(state.tools.len(), 2, "one first request per worker");
    // A worker's first request is its planning phase, which offers the
    // publish tool alone; the prover plans only when it holds both plan
    // tools, so the round was granted both.
    for tools in &state.tools {
        assert!(tools.iter().any(|t| t == PUBLISH), "{tools:?}");
    }
    assert_eq!(state.boards.len(), 2, "both publications answered: {:?}", state.boards);
    let (first, second) = (&state.boards[0], &state.boards[1]);
    assert_eq!(first.0, "unfold");
    let own = |board: &Value| board["own"].clone();
    // The service assigns the owner (the task in the publisher's resource),
    // the time, the order and the revision; nothing came from the model.
    assert!(own(&first.1)["task"].as_str().is_some_and(|t| t.starts_with('t')), "{first:?}");
    assert_eq!(own(&first.1)["revision"], 1);
    assert!(own(&first.1)["published_at_ms"].as_u64().is_some_and(|ms| ms > 0));
    assert_eq!(first.1["earlier"], json!([]), "nothing was published before the first");
    assert_ne!(own(&second.1)["task"], own(&first.1)["task"], "each worker has its own slot");
    assert_eq!(second.1["earlier"].as_array().unwrap().len(), 1, "{second:?}");
    let earlier = &second.1["earlier"][0];
    assert_eq!((&earlier["task"], &earlier["approach"]), (&own(&first.1)["task"], &json!("unfold")));
    assert!(earlier["sequence"].as_u64() < own(&second.1)["sequence"].as_u64());
    // That read is answered with the board, not refused and forced again.
    let read = state.read.clone().expect("the first worker read the board");
    assert!(read["earlier"].is_array() && own(&read)["task"] == own(&first.1)["task"], "{read}");
    drop(state);
    // The provider's refusal came while each worker was still planning: it
    // ends the worker as a model error, which discovery backs off on.
    for task in ["t000001", "t000002"] {
        let record = scenario.workers_dir("discovery", RUN).join(PROBLEM).join(task).join("result.json");
        let result: Value = serde_json::from_str(&std::fs::read_to_string(record).unwrap()).unwrap();
        assert_eq!(result["stopped"], "model_error", "{result}");
    }
    // Discovery's reason for each failure is the provider's message, not the
    // refusal of the unchanged file the harness offered after it.
    let rounds =
        std::fs::read_to_string(scenario.run_dir("discovery", RUN).join(PROBLEM).join("rounds.jsonl"));
    let round: Value = serde_json::from_str(rounds.unwrap().lines().next().unwrap()).unwrap();
    let why: Vec<&str> =
        round["failed"].as_array().unwrap().iter().filter_map(|f| f["why"].as_str()).collect();
    assert!(
        why.len() == 2
            && why.iter().all(|w| w.contains(
                "model_error: ServiceError: provider reports insufficient credit (http 402): fixture stop"
            )),
        "{why:?}"
    );
    assert!(supervisor.shutdown().is_clean());
}
