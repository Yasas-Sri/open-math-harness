//! A run's life in a process of its own: what one run's process cannot do to
//! another run, a stop, a round whose worker never submits, and a resume
//! after a restart.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nucleus_e2e::http::{Received, tool_request};

/// The service's live run processes, each with its live workers.
fn live_tree(
    supervisor: &Supervisor,
    scenario: &Scenario,
    harness: ProcessId,
) -> BTreeMap<ProcessId, BTreeSet<ProcessId>> {
    let live = |p: &ProcessId| supervisor.is_live(*p);
    let runs = scenario.children_of(harness).into_iter().filter(live);
    runs.map(|r| (r, scenario.children_of(r).into_iter().filter(live).collect())).collect()
}

/// `run state` of every run a `list` answer shows, in its order.
fn listed(line: &Value) -> String {
    let runs = ok_value(line).unwrap_or_else(|| panic!("list: {line}"))["runs"].as_array().unwrap();
    let run = |r: &Value| format!("{} {}", r["run"].as_str().unwrap(), r["state"].as_str().unwrap());
    runs.iter().map(run).collect::<Vec<_>>().join(", ")
}

/// One run's process never waits for another run, and ending one takes down
/// nothing else, with the service allowed two running runs (the kernel's limits
/// in [`boot`] leave room for it). Runs `a` and `c` run side by side, their
/// workers waiting on the model, while run `b` is admitted: its process
/// registers a statement Lean takes half a minute over.
///
/// 1. While `b` is admitted, `a`'s workers publish their plans through the
///    service, and `status a` and `list` are answered within 2 s.
/// 2. `stop b` during that admission answers `b`'s `start` that it did not
///    happen, and of the service's processes and theirs exactly `b`'s is
///    gone, leaving nothing on disk.
/// 3. The operator terminates `a`'s process: `a` is `failed`, and `c` still
///    ends `done`.
/// 4. After a restart of the whole Nucleus, `list` shows `a` and `c` from
///    disk, and `resume a` runs `a` on to its end.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_runs_process_neither_waits_for_nor_takes_down_another_run() {
    let (pb, _repl) = lean_env();
    let server = FakeHttp::start();
    // The replies wait for what the rows need: `a`'s workers plan once `b` is
    // being admitted, and `c`'s worker proves once `a`'s process is gone.
    // Every request is noted as it arrives.
    let (admitting, terminated) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let asked: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let held = |label: &str, gate: &Arc<AtomicBool>, reply: Scripted| {
        let (label, gate, asked) = (label.to_owned(), Arc::clone(gate), Arc::clone(&asked));
        move |request: &Received| {
            asked.lock().unwrap().push((label, request.body.clone()));
            wait_for(Duration::from_secs(300), || gate.load(Ordering::Acquire));
            reply
        }
    };
    let asked_for = |label: &str| asked.lock().unwrap().iter().filter(|(l, _)| l == label).count();
    let refusal = Scripted::json(400, &json!({"error": {"code": 400, "message": "fixture stop"}}));
    // A request carrying a plan's result matches its call id before `a`'s statement.
    for call in ["a-plan-1", "a-plan-2"] {
        server.script_with(call, held("a board", &terminated, refusal.clone()));
    }
    for (call, approach) in [("a-plan-1", "unfold"), ("a-plan-2", "induction")] {
        let plan = json!({"goal": "Prove it.", "approach": approach, "next_step": "Unfold the definition."});
        let publish = Scripted::json(200, &tool_request(MODEL, &[(call, "explore_publish_plan", plan)]));
        server.script_with(&key("demo_a"), held("a plans", &admitting, publish));
    }
    let write = format!("cat > demo_b.lean <<'EOF'\n{}EOF", proof("demo_b", "0 + n"));
    let write = tool_request(MODEL, &[("c-write", "shell_exec", json!({ "command": write }))]);
    server.script_with(&key("demo_b"), held("c proves", &terminated, Scripted::json(200, &write)));
    let submit = tool_request(MODEL, &[("c-submit", "explore_submit", json!({"name": "demo_b.lean"}))]);
    server.script_for(&key("demo_b"), Scripted::json(200, &submit));

    let policies = tempfile::tempdir().unwrap();
    // A round lost to the provider ends its rollout at once.
    let extra = ["--max-running-runs", "2", "--provider-retries", "0"];
    let mut scenario = world_with(&pb, &server, policies.path().to_path_buf(), &extra);
    std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
        .unwrap();
    scenario.add_shell("shell", &pb);
    let agent = |mut run: Value, w: u64| {
        run["prompt_template_file"] = json!("prompts/prover-agent.j2");
        run["concurrency"] = json!(1);
        run["limits"] = json!({"w": w, "k1": 1, "calls_per_continuation": 2, "fanout": w,
                               "model_calls_per_problem": 64, "max_usd_per_problem": CEILING_USD});
        run
    };
    let slow = "import Mathlib\n\nlemma slow_pause : True := by\n  sleep 30000\n  trivial\n\ntheorem slow_b : True := by\n  sorry\n";
    scenario.add_agent(
        "operator",
        &json!({"turns": [{"tool_calls": [
            discovery_call("start", None, agent(task("a", &["demo_a"], None), 2)),
            discovery_call("start", None, agent(task("c", &["demo_b"], None), 1)),
        ]}]}),
        None,
    );
    let b = agent(supplied("b", "slow_b", slow), 1);
    scenario.add_agent(
        "admitter",
        &json!({"turns": [{"tool_calls": [discovery_call("start", None, b)]}]}),
        None,
    );
    let status = |run: &str| discovery_call("status", Some(run), json!({}));
    scenario.add_agent(
        "observer",
        &json!({"turns": [{"tool_calls": [
            {"tool": "await_operator", "signal": "observe"},          // 0
            status("a"),                                              // 1
            discovery_call("list", None, json!({})),                  // 2
            {"tool": "await_operator", "signal": "stop"},             // 3
            discovery_call("stop", Some("b"), json!({})),             // 4
            status("b"),                                              // 5
            {"tool": "await_operator", "signal": "terminated"},       // 6
            status("a"),                                              // 7
            {"tool": "await_operator", "signal": "finished"},         // 8
            discovery_call("list", None, json!({})),                  // 9
        ]}]}),
        None,
    );
    let (mut supervisor, harness) = boot(&scenario);
    for caller in ["operator", "observer"] {
        supervisor.boot(program(caller), discovery_caller_grants()).unwrap();
    }
    assert!(
        wait_for(Duration::from_secs(300), || asked_for("a plans") == 2 && asked_for("c proves") == 1),
        "a's two workers and c's one wait on the model: {:?}",
        scenario.transcript("operator")
    );
    let (r_a, r_c) = (run_process(&scenario, harness, "a"), run_process(&scenario, harness, "c"));
    supervisor.boot(program("admitter"), discovery_caller_grants()).unwrap();

    // Row 1: b's process waits on Lean; a's workers, `status` and `list` do not wait for it.
    let registering = || {
        let probes = std::fs::read_dir(scenario.host("lean-scratch")).into_iter().flatten().flatten();
        probes.into_iter().any(|e| std::fs::read_to_string(e.path()).is_ok_and(|p| p.contains("slow_pause")))
    };
    assert!(wait_for(Duration::from_secs(120), registering), "b's statement reached Lean");
    let r_b = run_process(&scenario, harness, "b");
    admitting.store(true, Ordering::Release);
    assert!(wait_for(Duration::from_secs(60), || asked_for("a board") == 2), "a's workers published");
    for (_, body) in asked.lock().unwrap().iter().filter(|(label, _)| label == "a board") {
        assert!(body.contains("published_at_ms"), "the plan was taken: {body}");
    }
    scenario.signal("observer", "observe");
    assert!(
        wait_for(Duration::from_secs(2), || scenario.transcript("observer").len() >= 3),
        "status and list waited for b's admission: {:?}",
        scenario.transcript("observer")
    );
    let seen = scenario.transcript("observer");
    assert_eq!(ok_value(&seen[1]).map(|s| &s["state"]), Some(&json!("running")), "{:?}", seen[1]);
    assert_eq!(listed(&seen[2]), "a running, c running", "a start being admitted is not listed");
    assert!(scenario.transcript("admitter").is_empty() && supervisor.is_live(r_b), "b was still admitted");

    // Row 2: `stop b` while its process waits on Lean.
    let mut tree = live_tree(&supervisor, &scenario, harness);
    scenario.signal("observer", "stop");
    let admitted = scenario.wait_done_within("admitter", Duration::from_secs(60));
    let why = admitted[0]["result"]["error"]["message"].as_str().unwrap_or_default();
    assert!(error_code(&admitted[0]) == Some("service_error") && why.contains("not queued"), "{why}");
    let seen = scenario.wait_transcript("observer", 6);
    assert_eq!(ok_value(&seen[4]), Some(&json!({})), "{:?}", seen[4]);
    assert_eq!(error_code(&seen[5]), Some("invalid_input"), "a start that did not happen: {:?}", seen[5]);
    assert_eq!(tree.remove(&r_b), Some(BTreeSet::new()), "b's process had started nothing");
    assert_eq!(live_tree(&supervisor, &scenario, harness), tree, "exactly b's process is gone");
    assert_eq!((tree[&r_a].len(), tree[&r_c].len()), (2, 1), "{tree:?}");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.cleanup_confirmed(r_b)));
    assert!(!scenario.run_dir("discovery", "b").exists() && !scenario.workers_dir("discovery", "b").exists());

    // Row 3: the operator terminates a's process, and c goes on.
    supervisor.terminate(r_a);
    tree.remove(&r_a);
    assert_eq!(live_tree(&supervisor, &scenario, harness), tree, "a's process went with its workers");
    scenario.signal("observer", "terminated");
    let seen = scenario.wait_transcript("observer", 8);
    let a = ok_value(&seen[7]).expect("status a");
    assert_eq!((&a["state"], &a["error"]), (&json!("failed"), &json!("the run's process ended")), "{a}");
    terminated.store(true, Ordering::Release);
    assert!(supervisor.wait_until(Duration::from_secs(300), |s| !s.is_live(r_c)), "c's process ended");
    let c_rounds = lines(&scenario.run_dir("discovery", "c").join("demo_b/rounds.jsonl"));
    assert_eq!(c_rounds.last().map(|r| &r["reason"]), Some(&json!("solved")), "{c_rounds:?}");
    scenario.signal("observer", "finished");
    let seen = scenario.wait_done_within("observer", Duration::from_secs(60));
    assert_eq!(listed(&seen[9]), "a failed, c done");
    assert!(supervisor.shutdown().is_clean());

    // Row 4: after a restart the runs are read from disk, and a resumes.
    for _ in 0..2 {
        server.script_for(&key("demo_a"), refusal.clone());
    }
    scenario.add_agent(
        "resumer",
        &json!({"turns": [{"tool_calls": [
            discovery_call("list", None, json!({})),
            discovery_call("resume", Some("a"), json!({})),
        ]}]}),
        None,
    );
    let (mut supervisor, harness) = boot(&scenario);
    supervisor.boot(program("resumer"), vec![cap("discovery", &["list", "resume"], &[])]).unwrap();
    let resumed = scenario.wait_done_within("resumer", Duration::from_secs(60));
    assert_eq!(listed(&resumed[0]), "a stopped, c done", "b left nothing");
    // a's two workers died mid-call: what they spent is unknown, so each is
    // charged its whole reservation of 2 + 5 collection calls.
    assert_eq!(
        ok_value(&resumed[1]),
        Some(&json!({"start": {"run": "a", "problems": 1, "queued_ahead": 0}, "uncertain_model_calls": 14})),
        "{:?}",
        resumed[1]
    );
    let r_a = run_process(&scenario, harness, "a");
    assert!(supervisor.wait_until(Duration::from_secs(300), |s| !s.is_live(r_a)), "a ran to its end");
    let a_rounds = lines(&scenario.run_dir("discovery", "a").join("demo_a/rounds.jsonl"));
    let last = a_rounds.last().unwrap();
    assert_eq!(
        (&last["round"], &last["reason"]),
        (&json!(2), &json!("provider_unavailable")),
        "{a_rounds:?}"
    );
    assert!(supervisor.shutdown().is_clean());
}

/// `stop` while the run's four workers wait on the model: the kernel stops
/// the run's process with its workers, every active rollout's last record
/// says `terminated`, the run is `stopped` with nothing finished, and a
/// second `stop` of an ended run changes nothing. Around it, the observation
/// actions on what is not on disk yet: a queued run's problems and trees are
/// empty and `queued`; a live problem's row and tree show the open round the
/// journal will only get when it closes; after the stop the rows say
/// `terminated`.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn stopping_a_run_stops_its_workers_and_journals_terminated() {
    let (pb, _repl) = lean_env();
    let server = FakeHttp::start();
    // The model takes a while: whichever worker asks first waits on this reply
    // and the others queue behind it, so all four are alive when `stop` lands.
    // Nothing else is scripted; a request answered after the kill is a 500
    // into a dead worker's adapter call.
    let hold = Duration::from_secs(15);
    server.script(reply(&partial("demo_a", "n + 0")).after(hold));
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    scenario.add_agent(
        "operator",
        &json!({"turns": [{"tool_calls": [
            discovery_call("start", None, task(RUN, &PROBLEMS, None)),                // 0
            discovery_call("start", None, task("later", &PROBLEMS, None)),            // 1: queued behind it
            discovery_call("problems", Some("later"), json!({})),                    // 2: all queued
            discovery_call_on("tree", &["later", "demo_a"], json!({})),              // 3: an empty tree
            discovery_call("stop", Some("later"), json!({})),                        // 4: never starts
            {"tool": "await_operator", "signal": "stop"},                           // 5
            discovery_call("problems", Some(RUN), json!({})),                        // 6: live rows
            discovery_call_on("tree", &[RUN, "demo_a"], json!({})),                  // 7: the open round
            discovery_call("stop", Some(RUN), json!({})),                            // 8
            discovery_call("status", Some(RUN), json!({})),                          // 9
            discovery_call("list", None, json!({})),                                 // 10
            discovery_call("stop", Some(RUN), json!({})),                            // 11
            {"tool": "await_operator", "signal": "closed"},                         // 12
            discovery_call("problems", Some(RUN), json!({})),                        // 13: terminated
        ]}]}),
        None,
    );

    scenario.add_agent(
        "resume-after-cleanup",
        &json!({"turns":[{"tool_calls":[discovery_call("resume", Some(RUN), json!({}))]}]}),
        None,
    );
    let (mut supervisor, harness) = boot(&scenario);
    supervisor.boot(program("operator"), discovery_caller_grants()).unwrap();
    let run_dir = scenario.run_dir("discovery", RUN);
    // Two rollouts at once, W = 2 each, in the run's own process.
    let started = || scenario.children_of(harness).first().map(|r| scenario.children_of(*r));
    assert!(
        wait_for(Duration::from_secs(300), || started().is_some_and(|workers| workers.len() == 4)),
        "four workers were started: {:?}",
        started()
    );
    let process = run_process(&scenario, harness, RUN);
    let workers = scenario.children_of(process);
    assert!(workers.iter().all(|w| supervisor.is_live(*w)), "the workers are alive when the run is stopped");
    // A worker is answered its assignment only after the service has the
    // run's report of the round it opened, which the operator reads next.
    assert!(wait_for(TIMEOUT, || !server.received().is_empty()), "a worker asked the model");
    // Round 1 is open: the journal has each root and no round record yet.
    for problem in PROBLEMS {
        assert_eq!(lines(&run_dir.join(problem).join("nodes.jsonl")).len(), 1, "{problem}: the root");
        assert!(lines(&run_dir.join(problem).join("rounds.jsonl")).is_empty(), "{problem}: no round yet");
    }
    scenario.signal("operator", "stop");
    scenario.wait_transcript("operator", 12);
    // The run's process closes its journals once the kernel has stopped its workers.
    let closed = |p: &str| {
        lines(&run_dir.join(p).join("rounds.jsonl")).last().is_some_and(|r| r["reason"] == "terminated")
    };
    assert!(wait_for(TIMEOUT, || PROBLEMS.iter().all(|p| closed(p))), "the run's journals were closed");
    scenario.signal("operator", "closed");
    let transcript = scenario.wait_done_within("operator", Duration::from_secs(60));
    assert_eq!(transcript.len(), 14, "{transcript:?}");

    // --- The queued run: nothing on disk but task.json, nothing to show ---
    assert_eq!(
        ok_value(&transcript[1]),
        Some(&json!({"run": "later", "problems": 2, "queued_ahead": 1})),
        "{:?}",
        transcript[1]
    );
    let queued_row = |problem: &str| {
        json!({"problem": problem, "state": "queued", "best": null, "rounds": 0, "nodes": 0,
               "tokens_in": 0, "tokens_out": 0, "stop": null, "solved": false})
    };
    assert_eq!(
        ok_value(&transcript[2]),
        Some(&json!({"run": "later", "problems": [queued_row("demo_a"), queued_row("demo_b")]})),
        "{:?}",
        transcript[2]
    );
    assert_eq!(
        ok_value(&transcript[3]),
        Some(&json!({"run": "later", "problem": "demo_a", "state": "queued", "nodes": [], "rounds": [],
                     "policy": null, "truncated": false})),
        "{:?}",
        transcript[3]
    );
    assert_eq!(ok_value(&transcript[4]), Some(&json!({})), "a queued run is stopped before it starts");

    // --- The live run: round 1 open on both problems, no child yet ---
    let live = ok_value(&transcript[6]).expect("problems").clone();
    for (row, problem) in live["problems"].as_array().unwrap().iter().zip(PROBLEMS) {
        assert_eq!(
            row,
            &json!({"problem": problem, "state": "active", "best": 0.5, "rounds": 1, "nodes": 0,
                    "tokens_in": 0, "tokens_out": 0, "stop": null, "solved": false}),
            "{live}"
        );
    }
    let tree = ok_value(&transcript[7]).expect("tree").clone();
    assert_eq!(
        (tree["state"].as_str(), tree["truncated"].as_bool()),
        (Some("active"), Some(false)),
        "{tree}"
    );
    assert_eq!(tree["policy"], json!({"kind": "builtin", "program": null}));
    let nodes = tree["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "the root only: {tree}");
    assert_eq!(
        (nodes[0]["id"].as_u64(), nodes[0]["score"].as_f64(), &nodes[0]["worker"]),
        (Some(0), Some(0.5), &Value::Null)
    );
    let rounds = tree["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "the open round, which the journal does not have yet: {tree}");
    assert_eq!(
        rounds[0],
        json!({"round": 1, "batch": [0, 0], "children": [], "failed": [], "reason": null,
               "policy_error": null, "tokens_in": 0, "tokens_out": 0}),
        "{tree}"
    );

    assert_eq!(ok_value(&transcript[8]), Some(&json!({})), "{:?}", transcript[8]);

    // No child survives the stop.
    assert!(
        supervisor.wait_until(TIMEOUT, |s| workers.iter().all(|w| !s.is_live(*w))),
        "every worker of the run is gone"
    );
    assert_eq!(scenario.children_of(process).len(), 4, "nothing was started after the stop");
    // The kernel stopped the four with the run's process, which kills none of
    // them itself and was never refused a spawn.
    let events = lines(&run_dir.join("discovery.jsonl"));
    let tasks_of = |event: &str| -> BTreeSet<String> {
        events
            .iter()
            .filter(|r| r["event"] == event)
            .map(|r| r["task"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(tasks_of("spawned").len(), 4, "{events:?}");
    assert!(["killed", "kill_refused", "spawn_refused"].iter().all(|e| tasks_of(e).is_empty()), "{events:?}");

    // Every active rollout was closed as `terminated`, with no child.
    for problem in PROBLEMS {
        let rounds = lines(&run_dir.join(problem).join("rounds.jsonl"));
        let last = rounds.last().unwrap_or_else(|| panic!("{problem}: a closing record"));
        assert_eq!(
            (last["round"].as_u64(), last["reason"].as_str()),
            (Some(1), Some("terminated")),
            "{last}"
        );
        assert_eq!((ids(&last["batch"]), ids(&last["children"])), (vec![0, 0], vec![]), "{last}");
        assert_eq!(lines(&run_dir.join(problem).join("nodes.jsonl")).len(), 1, "{problem}: the root only");
    }

    // The run is `stopped`, nothing finished, nothing active; a stopped run stays stopped.
    let status = ok_value(&transcript[9]).expect("status");
    assert_eq!(
        (status["state"].as_str(), status["finished"].as_u64(), status["solved"].as_u64(), &status["active"]),
        (Some("stopped"), Some(0), Some(0), &json!([])),
        "{status}"
    );
    assert!(status.get("error").is_none(), "a stop is not a failure: {status}");
    let runs = ok_value(&transcript[10]).expect("list")["runs"].as_array().unwrap().clone();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!((runs[0]["run"].as_str(), runs[0]["state"].as_str()), (Some(RUN), Some("stopped")));
    assert_eq!((runs[1]["run"].as_str(), runs[1]["state"].as_str()), (Some("later"), Some("stopped")));
    assert_eq!(ok_value(&transcript[11]), Some(&json!({})), "a second stop is accepted and changes nothing");
    // The rows of a stopped run: its started problems ended `terminated`,
    // in round 1, with the root as their only node.
    let ended = ok_value(&transcript[13]).expect("problems").clone();
    for (row, problem) in ended["problems"].as_array().unwrap().iter().zip(PROBLEMS) {
        assert_eq!(
            row,
            &json!({"problem": problem, "state": "done", "best": 0.5, "rounds": 1, "nodes": 0,
                    "tokens_in": 0, "tokens_out": 0, "stop": "terminated", "solved": false}),
            "{ended}"
        );
    }

    // A stopped run resumes without a service restart once the kernel has
    // confirmed its process and workers gone.
    assert!(supervisor.wait_until(TIMEOUT, |s| s.cleanup_confirmed(process)), "the run's process was reaped");
    supervisor
        .boot(
            program("resume-after-cleanup"),
            vec![cap("discovery", &["resume"], &[nucleus_e2e::DEFAULT_PROJECT, RUN])],
        )
        .unwrap();
    let resumed = scenario.wait_done("resume-after-cleanup");
    assert!(ok_value(&resumed[0]).is_some(), "a stopped run resumes: {resumed:?}");
    assert!(supervisor.is_live(harness), "the service outlives the run it stopped");
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

/// A worker that never submits: the fake model holds demo_a's only reply
/// far longer than the round's wall clock (`--worker-timeout-ms 4000`, one
/// call per worker), so the service kills the worker, records the
/// continuation as `timed_out` and the round as `no_children`, and the run
/// still ends `done` — a hung worker cannot hold a rollout, its run or the
/// queue behind it. The kernel's exit event carries no status, so the record
/// says what the service knows: the continuation timed out and was killed.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_round_whose_worker_never_submits_times_out_and_the_run_still_ends() {
    let (pb, _repl) = lean_env();
    let server = FakeHttp::start();
    server.script_for(&key("demo_a"), reply(&partial("demo_a", "n + 0")).after(Duration::from_secs(15)));
    let policies = tempfile::tempdir().unwrap();
    let mut scenario =
        world_with(&pb, &server, policies.path().to_path_buf(), &["--worker-timeout-ms", "4000"]);
    let mut hang = task("hang", &["demo_a"], None);
    hang["concurrency"] = json!(1);
    hang["limits"] = json!({"w": 1, "k1": 2, "calls_per_continuation": 1, "fanout": 1,
                            "model_calls_per_problem": 2, "max_usd_per_problem": CEILING_USD});
    scenario.add_agent(
        "operator",
        &json!({"turns": [{"tool_calls": [
            discovery_call("start", None, hang),
            {"tool": "await_operator", "signal": "finished"},
            discovery_call("status", Some("hang"), json!({})),
        ]}]}),
        None,
    );

    let (mut supervisor, harness) = boot(&scenario);
    supervisor.boot(program("operator"), discovery_caller_grants()).unwrap();
    let run_dir = scenario.run_dir("discovery", "hang");
    let started = Instant::now();
    let ended = || {
        lines(&run_dir.join("demo_a").join("rounds.jsonl")).last().is_some_and(|r| r.get("reason").is_some())
    };
    assert!(
        wait_for(Duration::from_secs(120), ended),
        "the rollout ended although its worker never submitted"
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(90), "the round's clock, not the model deadline, ended it: {took:?}");

    let rounds = lines(&run_dir.join("demo_a").join("rounds.jsonl"));
    assert_eq!(rounds.len(), 1, "{rounds:?}");
    let round = &rounds[0];
    assert_eq!(
        (round["round"].as_u64(), ids(&round["batch"]), ids(&round["children"])),
        (Some(1), vec![0], vec![])
    );
    assert_eq!(round["reason"], "no_children", "{round}");
    let failed = round["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{round}");
    assert_eq!(failed[0]["why"], "timed_out", "{round}");
    let task_id = failed[0]["task"].as_str().unwrap().to_owned();
    assert_eq!(lines(&run_dir.join("demo_a").join("nodes.jsonl")).len(), 1, "the root only");
    // The worker was spawned, then killed by the service when the clock ran out.
    let events = lines(&run_dir.join("discovery.jsonl"));
    let of = |event: &str| -> Vec<String> {
        events
            .iter()
            .filter(|r| r["event"] == event)
            .map(|r| r["task"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(of("spawned"), std::slice::from_ref(&task_id), "{events:?}");
    assert_eq!(of("killed"), std::slice::from_ref(&task_id), "{events:?}");
    assert!(of("kill_refused").is_empty(), "{events:?}");
    let workers = scenario.children_of(run_process(&scenario, harness, "hang"));
    assert!(
        supervisor.wait_until(TIMEOUT, |s| workers.iter().all(|w| !s.is_live(*w))),
        "no worker of the run is alive"
    );

    scenario.signal("operator", "finished");
    let transcript = scenario.wait_done_within("operator", Duration::from_secs(60));
    let status = ok_value(&transcript[2]).expect("status");
    assert_eq!(
        (status["state"].as_str(), status["finished"].as_u64(), status["solved"].as_u64(), &status["active"]),
        (Some("done"), Some(1), Some(0), &json!([])),
        "{status}"
    );
    assert!(status.get("error").is_none(), "a timed-out round is a failed continuation, not a failed run");
    assert!(supervisor.is_live(harness), "the service is still serving");
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

/// A real restart, not an in-memory retry: a second kernel boots only the
/// configured programs and grants, and `discovery-client --resume` resumes the
/// closed v1 checkpoint.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_resume_reopens_the_same_run_and_preserves_its_model_budget() {
    resume_restart(false);
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_resume_inherits_transcript_and_workspace_without_replaying_tools() {
    resume_restart(true);
}

fn resume_restart(with_shell: bool) {
    let (pb, _) = lean_env();
    let server = FakeHttp::start();
    if with_shell {
        let priced = |model: &str, calls: &[(&str, &str, Value)]| {
            let mut response = nucleus_e2e::http::tool_request(model, calls);
            response["usage"]["cost"] = json!(0.000_012);
            response
        };
        let file = partial("demo_a", "n + 0");
        server.script(Scripted::json(200, &priced(MODEL, &[("saved-write", "shell_exec", json!({"command":format!("cat > demo_a.lean <<'EOF'\n{file}EOF\necho once >> note.txt")}))])).after(Duration::from_secs(3)));
        server.script(Scripted::json(200, &priced(MODEL, &[("saved-submit", "explore_submit", json!({}))])));
        server.script(Scripted::json(
            200,
            &priced(
                MODEL,
                &[(
                    "resumed-read",
                    "shell_exec",
                    json!({"command":"cat note.txt; echo '-- continued' >> demo_a.lean"}),
                )],
            ),
        ));
        server
            .script(Scripted::json(200, &priced(MODEL, &[("resumed-submit", "explore_submit", json!({}))])));
    } else {
        server.script(reply(&partial("demo_a", "n + 0")).after(Duration::from_secs(3)));
        server.script(reply(&partial("demo_a", "n + 0")));
    }
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    let mut document = task("saved", &["demo_a"], None);
    document["concurrency"] = json!(1);
    document["limits"] = json!({"w":1,"k1":1,"fanout":1,"calls_per_continuation":1,"model_calls_per_problem":if with_shell {8} else {2},"max_usd_per_problem":CEILING_USD});
    if with_shell {
        std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
            .unwrap();
        document["prompt_template_file"] = json!("prompts/prover-agent.j2");
        scenario.add_shell("shell", &pb);
    }
    scenario.add_discovery_client("client", &document, 100, &[]);
    scenario.add_discovery_client("resumer", &document, 100, &["--resume", "saved"]);
    scenario.add_agent(
        "duplicate",
        &json!({"turns":[{"tool_calls":[
            discovery_call("resume", Some("saved"), json!({})),
            discovery_call("resume", Some("saved"), json!({}))
        ]}]}),
        None,
    );
    scenario.add_agent(
        "intruder",
        &json!({"turns":[{"tool_calls":[discovery_call("resume", Some("saved"), json!({}))]}]}),
        None,
    );
    let (mut supervisor, _harness) = boot(&scenario);
    let client =
        supervisor.boot(program("client"), vec![cap("discovery", &["start", "status"], &[])]).unwrap();
    assert!(wait_for(Duration::from_secs(180), || !server.received().is_empty()));
    let saved = [nucleus_e2e::DEFAULT_PROJECT, "saved"];
    supervisor.boot(program("duplicate"), vec![cap("discovery", &["resume"], &saved)]).unwrap();
    let duplicate = scenario.wait_done("duplicate");
    assert_eq!(error_code(&duplicate[0]), Some("invalid_input"));
    assert_eq!(error_code(&duplicate[1]), Some("invalid_input"));
    assert!(supervisor.wait_until(Duration::from_secs(180), |k| !k.is_live(client)));
    let run = scenario.run_dir("discovery", "saved");
    let old_nodes = std::fs::read(run.join("demo_a/nodes.jsonl")).unwrap();
    assert_eq!(server.received().len(), if with_shell { 2 } else { 1 });
    assert_eq!(lines(&run.join("demo_a/nodes.jsonl")).len(), 2, "one partial checkpoint");
    // No resume permission: denial occurs before touching the store.
    supervisor.boot(program("intruder"), vec![cap("discovery", &["status"], &saved)]).unwrap();
    assert_eq!(error_code(&scenario.wait_done("intruder")[0]), Some("permission_denied"));
    assert!(!run.join("resume.jsonl").exists());
    assert!(supervisor.shutdown().is_clean());

    // The same store and a fresh kernel: the client resumes the run. `lean_calls` caps each
    // of the service's `lean` grants.
    let resume = |lean_calls: Option<u64>| {
        let mut supervisor =
            scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
        scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
        scenario.boot_service(&supervisor, "openrouter", "model");
        if with_shell {
            scenario.boot_service(&supervisor, "shell", "shell");
        }
        let mut grants = service_grants(&scenario);
        for grant in grants.iter_mut().filter(|g| g.endpoint.as_str() == "lean") {
            grant.max_calls = lean_calls;
        }
        let harness = supervisor.boot(program("discovery"), grants).unwrap();
        let discovery = endpoint("discovery");
        assert!(
            supervisor.wait_until(TIMEOUT, |s| s.provider_of(&discovery).is_some_and(|(p, _)| p == harness))
        );
        let client = supervisor
            .boot(program("resumer"), vec![cap("discovery", &["resume", "status"], &saved)])
            .unwrap();
        assert!(supervisor.wait_until(Duration::from_secs(180), |k| !k.is_live(client)));
        assert!(supervisor.shutdown().is_clean());
    };
    resume(None);
    assert_eq!(
        server.received().len(),
        if with_shell { 4 } else { 2 },
        "one remaining search and its collection were available"
    );
    assert!(
        std::fs::read(run.join("demo_a/nodes.jsonl")).unwrap().starts_with(&old_nodes),
        "old checkpoint not rewritten"
    );
    assert!(scenario.workers_dir("discovery", "saved").join("demo_a/t000002/result.json").is_file());
    assert_eq!(lines(&run.join("resume.jsonl")).len(), 1);
    if with_shell {
        let workers = scenario.workers_dir("discovery", "saved").join("demo_a");
        assert_eq!(
            std::fs::read(workers.join("t000001/transcript.jsonl")).unwrap(),
            std::fs::read(workers.join("t000002/parent.jsonl")).unwrap()
        );
        let request: Value = serde_json::from_str(&server.received()[2].body).unwrap();
        assert!(request["messages"].as_array().unwrap().iter().any(|m| m["tool_call_id"] == "saved-write"));
        for task in ["t000001", "t000002"] {
            assert_eq!(
                std::fs::read_to_string(
                    scenario.host(&format!("shell-work/default/runs/saved/demo_a/{task}/note.txt"))
                )
                .unwrap(),
                "once\n",
                "old shell effect was not repeated"
            );
        }
        let nodes = lines(&run.join("demo_a/nodes.jsonl"));
        assert_eq!(nodes[2]["parent"], 1);
    }
    // No new work remains. Restoring existing scores must require only the
    // statement's read, on the delegable grant, and its registration for the
    // run, on `register`; never a historical check/analyze/complete/verify
    // call. Keep the required `lean` grants but allow each exactly its one
    // call, so one such call on the delegable grant is refused.
    resume(Some(1));
    assert_eq!(
        server.received().len(),
        if with_shell { 4 } else { 2 },
        "resume must not reset the cumulative generation budget"
    );
    assert_eq!(
        lines(&run.join("demo_a/rounds.jsonl")).last().unwrap()["reason"],
        "model_budget",
        "restoration must finish without any historical Lean reevaluation"
    );
    assert!(!scenario.workers_dir("discovery", "saved").join("demo_a/t000003").exists());
    assert_eq!(lines(&run.join("resume.jsonl")).len(), 2);
}
