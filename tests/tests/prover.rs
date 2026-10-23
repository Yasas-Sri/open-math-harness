//! The prover agent end to end and keyless, as a discovery worker: a scripted
//! fake model, the real OpenRouter adapter, the real confined `lean` service,
//! the real kernel.
//!
//! Opt-in (`--ignored`): needs `PB_LEAN` (Lean + Mathlib) and `NUCLEUS_REPL`;
//! fails loudly when run without them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use nucleus_e2e::http::{FakeHttp, Scripted};
use nucleus_e2e::{
    DiscoveryService, Scenario, TIMEOUT, cap, discovery_grants, endpoint, program, prompts_dir,
};
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";
const KEY: &str = "sk-or-test-PROVER-0123456789";

/// The PutnamBench `lean4` directory; these tests are `#[ignore]`d without it.
fn pb_lean() -> PathBuf {
    PathBuf::from(std::env::var_os("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"))
}

fn lines(path: PathBuf) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A continuation below the root starts from its parent's file, and its
/// `lean verify` is still judged against the frozen statement, which the
/// `lean` service takes from the call's resource: no worker sends it, and a
/// `frozen` the model writes into its call is dropped. `demo_a_f` is
/// irreducible, so the `lean` service's tactic suites neither finish the
/// partial proof nor prove the theorem on their own. On the way, a
/// submission of a file without the statement is refused by the worker
/// itself, and each continuation gets its branch index, also where its
/// parent left a link to the proof file under that name.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_worker_below_the_root_verifies_against_the_frozen_statement() {
    use nucleus_e2e::http::tool_request;
    let pb = pb_lean();
    let repl = std::env::var_os("NUCLEUS_REPL").map(PathBuf::from);
    assert!(
        repl.is_some_and(|r| r.is_file()),
        "NUCLEUS_REPL must name the Lean REPL (discovery scores on it)"
    );
    let head = "import Mathlib\n\n@[irreducible] def demo_a_f (n : Nat) : Nat := n + 0\n\n";
    let theorem = "theorem demo_a (n : Nat) : demo_a_f n = n :=";
    let partial = format!(
        "{head}{theorem} by\n  have h : demo_a_f (n + 1) = 1 + n := by sorry\n  unfold demo_a_f\n  omega\n"
    );
    let proof = format!("{head}{theorem}\n by simp [demo_a_f]\n");
    let server = FakeHttp::start();
    let tool = |id: &str, name: &str, arguments: Value| {
        server.script(Scripted::json(200, &tool_request(MODEL, &[(id, name, arguments)])));
    };
    // Round 1 continues the root: its one search call writes the partial
    // proof and a scratch file without the theorem, and turns its branch
    // index into a link to the proof file; its first collection call offers
    // the scratch file and is refused, its second submits the partial proof
    // as node 1.
    let write = format!(
        "cat > demo_a.lean <<'EOF'\n{partial}EOF\nprintf 'import Mathlib\\n' > scratch.lean\n\
         ln -sf demo_a.lean BRANCHES.json"
    );
    tool("write", "shell_exec", json!({"command": write}));
    tool("scratch", "explore_submit", json!({"name": "scratch.lean"}));
    tool("submit", "explore_submit", json!({}));
    // Round 2 continues node 1, whose file is the partial proof: the model
    // reads it, and verifies the proof. It also sends the partial proof as
    // `frozen`, the standard to judge against: the worker drops it, so the
    // verify is judged against the run's statement.
    let verify = json!({"problem": "demo_a", "candidate": proof, "frozen": partial});
    server.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[
                ("read", "shell_exec", json!({"command": "cat demo_a.lean"})),
                ("verify", "lean_verify", verify),
            ],
        ),
    ));

    let mut scenario = Scenario::new();
    scenario.write("problems/demo_a.lean", &format!("{head}{theorem}\nsorry\n"));
    scenario.write(
        "tasks/prompts/prover-agent.j2",
        &std::fs::read_to_string(prompts_dir().join("prover-agent.j2")).unwrap(),
    );
    scenario.add_lean("lean", &pb, "problems");
    scenario.add_shell("shell", &pb);
    // A call past the script is answered 500. Neither the adapter nor the
    // harness retries it, so a refused verify fails this test in seconds
    // instead of after their backoffs.
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    let words = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let service = DiscoveryService {
        tasks: scenario.host("tasks"),
        policies: scenario.host("policies"),
        prover_args: words(&["--model", MODEL, "--max-attempts", "3", "--reasoning-effort", "off"]),
        // One run of one worker, within the kernel's default limits.
        extra: words(&[
            "--max-concurrency",
            "1",
            "--max-width",
            "1",
            "--max-queued-runs",
            "1",
            "--max-usd-per-problem",
            "20",
            "--provider-retries",
            "0",
        ]),
    };
    scenario.add_discovery("discovery", &service);
    let task = json!({"v": 1, "name": "deep", "problems": ["demo_a"], "concurrency": 1,
        "prompt_template_file": "prompts/prover-agent.j2",
        "limits": {"w": 1, "k1": 2, "calls_per_continuation": 1, "fanout": 1,
                   "model_calls_per_problem": 16, "max_usd_per_problem": "20"}});
    scenario.add_discovery_client("client", &task, 200, &[]);

    let mut supervisor = scenario.start();
    scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
    scenario.boot_service(&supervisor, "shell", "shell");
    scenario.boot_service(&supervisor, "openrouter", "model");
    let mut grants = discovery_grants(16);
    grants.push(cap("shell", &["exec"], &[]));
    let harness = supervisor.boot(program("discovery"), grants).unwrap();
    for name in ["discovery", "explore"] {
        let target = endpoint(name);
        assert!(
            supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == harness))
        );
    }
    let client =
        supervisor.boot(program("client"), vec![cap("discovery", &["start", "status"], &[])]).unwrap();
    assert!(supervisor.wait_until(Duration::from_secs(600), |s| !s.is_live(client)), "the run ended");

    let workers = scenario.workers_dir("discovery", "deep").join("demo_a");
    let result: Value =
        serde_json::from_str(&std::fs::read_to_string(workers.join("t000002/result.json")).unwrap()).unwrap();
    assert_eq!(result["stopped"], "complete", "its own verify accepted the proof: {result}");
    let submits: Vec<Value> = lines(workers.join("t000001/attempts.jsonl"))
        .into_iter()
        .filter(|step| step["tool"] == "explore_submit")
        .collect();
    assert!(
        submits[0]["detail"].as_str().unwrap().contains("does not contain the frozen theorem statement"),
        "the worker refuses a file without the statement before the harness sees it: {submits:?}"
    );
    // The link node 1 left in place of its index was replaced in round 2's
    // clone, not written through: the model found its parent's proof file.
    let read = lines(workers.join("t000002/transcript.jsonl"))
        .into_iter()
        .find(|line| line["message"]["tool_call_id"] == "read")
        .expect("the model read its file");
    let seen = read["message"]["content"].as_str().unwrap();
    assert!(seen.contains("have h : demo_a_f (n + 1) = 1 + n"), "the proof file is node 1's: {seen}");
    // A run's workspaces are its project's: `<project>/runs/<run>/` at the shell.
    let index_file = scenario.host("shell-work/default/runs/deep/demo_a/t000002/BRANCHES.json");
    assert!(std::fs::symlink_metadata(&index_file).unwrap().is_file(), "the index is a file of its own");
    let index: Value = serde_json::from_str(&std::fs::read_to_string(index_file).unwrap()).unwrap();
    assert_eq!(index["branches"], json!([]), "the assignment's branch references, written for the model");
    let nodes = lines(scenario.run_dir("discovery", "deep").join("demo_a").join("nodes.jsonl"));
    let parents: Vec<Value> = nodes.iter().map(|n| n["parent"].clone()).collect();
    assert_eq!(parents, [json!(null), json!(0), json!(1)], "the worker was at depth 2");
    assert!(supervisor.shutdown().is_clean());
}
