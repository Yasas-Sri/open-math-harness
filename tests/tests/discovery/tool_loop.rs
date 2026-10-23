//! The tool loop in the tree: workers with a `shell` workspace, the collection
//! phase that ends a search, what a continuation inherits from its parent, and
//! a verifier that fails or is shut down while it judges a submission.

use super::*;
use nucleus_e2e::http::tool_request;

/// The tool loop in the tree: with a `shell` workspace per worker, round 1's
/// worker writes a partial proof through `bash` and leaves a note in its
/// workspace; round 2 continues that node — the harness copies the workspace
/// (`cp -a`) and the transcript (`parent.jsonl`), the child's buffer is the
/// parent's messages plus one `resume` message, and its `cat` finds the note.
/// One problem, W 1, K1 2, two calls per continuation.
///
/// It also covers the handover: with the budget nearly spent both workers are
/// told, once, to submit; round 1's model ignores it and the harness sends its
/// workspace file, round 2's calls `explore submit` with a file that is **not**
/// the one on disk, and that is what the node records.
#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_continuation_appends_to_its_parents_transcript_and_works_in_a_copy_of_its_workspace() {
    collection_inheritance_case(true, false);
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn collection_exhaustion_is_bounded_and_its_dialogue_is_not_inherited() {
    collection_inheritance_case(false, false);
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn a_complete_normal_submission_ends_without_collection_calls() {
    normal_submission_case(false);
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn an_automatically_closable_sorry_is_still_refused_during_normal_search() {
    normal_submission_case(true);
}

fn normal_submission_case(with_hole: bool) {
    let (pb, _) = lean_env();
    let server = FakeHttp::start();
    let file = proof("demo_a", "n + 0");
    if with_hole {
        let incomplete =
            file.replace("by simp [demo_a_f]", "by\n  have h : n = n := by sorry\n  simp [demo_a_f]");
        assert!(incomplete.contains("sorry"));
        server.script(Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[
                    (
                        "write-partial",
                        "shell_exec",
                        json!({"command": format!("cat > demo_a.lean <<'EOF'\n{incomplete}EOF")}),
                    ),
                    ("early-submit", "explore_submit", json!({"name":"demo_a.lean"})),
                ],
            ),
        ));
        server.script(Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[
                    (
                        "write-complete",
                        "shell_exec",
                        json!({"command": format!("cat > demo_a.lean <<'EOF'\n{file}EOF")}),
                    ),
                    ("submit", "explore_submit", json!({"name":"demo_a.lean"})),
                ],
            ),
        ));
    } else {
        server.script(Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[(
                    "write",
                    "shell_exec",
                    json!({"command": format!("cat > demo_a.lean <<'EOF'\n{file}EOF")}),
                )],
            ),
        ));
        server.script(Scripted::json(
            200,
            &tool_request(MODEL, &[("submit", "explore_submit", json!({"name": "demo_a.lean"}))]),
        ));
    }
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
        .unwrap();
    scenario.add_shell("shell", &pb);
    let mut run_task = task(RUN, &["demo_a"], None);
    run_task["prompt_template_file"] = json!("prompts/prover-agent.j2");
    run_task["concurrency"] = json!(1);
    run_task["limits"] = json!({"w":1,"k1":1,"calls_per_continuation":3,"fanout":1,"model_calls_per_problem":8,"max_usd_per_problem":CEILING_USD});
    scenario.add_discovery_client("client", &run_task, 100, &[]);
    let (mut supervisor, _harness) = boot(&scenario);
    let client =
        supervisor.boot(program("client"), vec![cap("discovery", &["start", "status"], &[])]).unwrap();
    assert!(supervisor.wait_until(Duration::from_secs(180), |s| !s.is_live(client)));
    let nodes = lines(&scenario.run_dir("discovery", RUN).join("demo_a/nodes.jsonl"));
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[1]["verified"], true);
    assert_eq!(nodes[1]["worker"]["model_calls"], 2);
    assert_eq!(server.received().len(), 2, "accepted normal submission ends immediately");
    if with_hole {
        let body: Value = serde_json::from_str(&server.received()[1].body).unwrap();
        assert!(body["messages"].to_string().contains("complete verified proof is required"));
    }
    let worker = scenario.workers_dir("discovery", RUN).join("demo_a/t000001");
    assert!(!worker.join("submission.jsonl").exists(), "collection was never entered");
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn stream_failures_at_phase_boundaries_preserve_collection_and_force_its_last_submit() {
    collection_inheritance_case(true, true);
}

fn collection_inheritance_case(submit_final: bool, retry_boundaries: bool) {
    let (pb, _repl) = lean_env();
    let server = FakeHttp::start();
    let a_partial = partial("demo_a", "n + 0");
    let collected = format!("{a_partial}-- collection-file-survives\n");
    let handed_over = format!("{collected}-- child artifact\n");
    let tool = |id, name, arguments| {
        server.script(Scripted::json(200, &tool_request(MODEL, &[(id, name, arguments)])))
    };
    // Three normal calls: a real partial proof, a refused incomplete submission,
    // then continued work. Submission remains available, but cannot end search.
    tool(
        "search-write",
        "shell_exec",
        json!({"command": format!("cat > demo_a.lean <<'EOF'\n{a_partial}EOF\necho written > note.txt")}),
    );
    tool("search-submit", "explore_submit", json!({}));
    if retry_boundaries {
        server.script(Scripted::cut_stream(MODEL, &["discarded normal fragment"], Duration::ZERO));
    } else {
        server.script(Scripted::json(200, &completion(MODEL, "Continue working on the open goal.")));
    }
    // All five collection calls. Only their files, not their conversation, survive.
    tool(
        "collection-write",
        "shell_exec",
        json!({"command": format!("cat > demo_a.lean <<'EOF'\n{collected}EOF\necho retained > collection-file.txt")}),
    );
    for i in 0..3 {
        if retry_boundaries && i == 2 {
            server.script(Scripted::cut_stream(MODEL, &["discarded collection fragment"], Duration::ZERO));
        } else {
            server.script(Scripted::json(200, &completion(MODEL, "COLLECTION_ONLY_SENTINEL")));
        }
    }
    if submit_final {
        tool("collection-submit", "explore_submit", json!({}));
    } else {
        server.script(Scripted::json(200, &completion(MODEL, "COLLECTION_ONLY_SENTINEL")));
    }
    // The child can read the collection's files but must not see its instructions.
    tool(
        "child-write",
        "shell_exec",
        json!({"command": format!("cat note.txt collection-file.txt; cat > handed.lean <<'EOF'\n{handed_over}EOF")}),
    );
    tool("child-early-submit", "explore_submit", json!({"name":"handed.lean"}));
    server.script(Scripted::json(200, &completion(MODEL, "Continue solving.")));
    tool("child-final-submit", "explore_submit", json!({"name":"handed.lean"}));
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    if retry_boundaries {
        // Exercise worker recovery at the model-call boundary, not the
        // provider's internal retries of its blocking completion transport.
        scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    }
    std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
        .unwrap();
    scenario.add_shell("shell", &pb);
    let mut run_task = task(RUN, &["demo_a"], None);
    run_task["prompt_template_file"] = json!("prompts/prover-agent.j2");
    run_task["concurrency"] = json!(1);
    run_task["limits"] = json!({"w": 1, "k1": 2, "calls_per_continuation": 3, "fanout": 1, "model_calls_per_problem": 16, "max_usd_per_problem": CEILING_USD});
    scenario.add_discovery_client("client", &run_task, 500, &[]);

    let (mut supervisor, _harness) = boot(&scenario);
    let mut grant = cap("discovery", &["start", "status"], &[]);
    grant.delegable = false;
    supervisor.boot(program("client"), vec![grant]).unwrap();

    let run_dir = scenario.run_dir("discovery", RUN);
    let ended = || {
        lines(&run_dir.join("demo_a").join("rounds.jsonl")).last().is_some_and(|r| r.get("reason").is_some())
    };
    assert!(wait_for(Duration::from_secs(600), ended), "the rollout ended");
    let rounds = lines(&run_dir.join("demo_a").join("rounds.jsonl"));
    assert_eq!(rounds.last().unwrap()["reason"], "rounds", "{rounds:?}");
    assert_eq!(batches(&rounds), vec![vec![0], vec![1]], "round 2 continued node 1");

    let nodes = lines(&run_dir.join("demo_a/nodes.jsonl"));
    assert_eq!(nodes.len(), 3, "early partial submissions create no nodes: {nodes:?}");
    assert_eq!(nodes[1]["file"], collected);
    assert_eq!(nodes[2]["file"], handed_over);
    assert_eq!(nodes[2]["parent"], 1);
    assert_eq!(nodes[1]["worker"]["model_calls"], 8, "3 search + 5 collection calls charged");
    assert_eq!(nodes[2]["worker"]["model_calls"], 4, "collection exits on its first successful submission");
    let workers = scenario.workers_dir("discovery", RUN).join("demo_a");
    let parent = std::fs::read_to_string(workers.join("t000001/transcript.jsonl")).unwrap();
    assert_eq!(std::fs::read_to_string(workers.join("t000002/parent.jsonl")).unwrap(), parent);
    assert!(!parent.contains("COLLECTION_ONLY_SENTINEL"));
    assert!(!parent.contains("collection_started"));
    let audit = std::fs::read_to_string(workers.join("t000001/submission.jsonl")).unwrap();
    assert!(audit.contains("COLLECTION_ONLY_SENTINEL"), "collection audit is retained");
    assert!(audit.contains("collection-write"));
    let child = std::fs::read_to_string(workers.join("t000002/transcript.jsonl")).unwrap();
    assert!(!child.contains("COLLECTION_ONLY_SENTINEL"));
    assert!(child.contains("retained"), "collection's workspace file was inherited");
    assert!(!child.contains("collection-submit"));
    for task_id in ["t000001", "t000002"] {
        let result: Value =
            serde_json::from_slice(&std::fs::read(workers.join(task_id).join("result.json")).unwrap())
                .unwrap();
        let model_submitted = task_id == "t000002" || submit_final;
        assert_eq!(result["stopped"], if model_submitted { "submitted" } else { "max_attempts" });
        assert_eq!(result["agent"]["submitted_by_model"], model_submitted);
    }
    let received = server.received();
    assert_eq!(received.len(), 12, "no call after an accepted collection submission");
    let body = |index: usize| serde_json::from_str::<Value>(&received[index].body).unwrap();
    let normal = body(0);
    assert!(normal["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "explore_submit"));
    assert!(!normal.to_string().contains("collection phase"), "no premature collection instruction");
    assert!(body(2)["messages"].to_string().contains("complete verified proof is required"));
    let last = body(7);
    assert_eq!(last["tools"].as_array().unwrap().len(), 1);
    assert_eq!(last["tool_choice"]["function"]["name"], "explore_submit");
    let inherited = body(8);
    assert!(!inherited["messages"].to_string().contains("COLLECTION_ONLY_SENTINEL"));
    assert!(!inherited["messages"].to_string().contains("separate artifact collection phase"));
    assert!(!inherited["messages"].to_string().contains("collection-submit"));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn verifier_failure_does_not_consume_the_terminal_submission() {
    verifier_interruption_case(false);
}

#[test]
#[ignore = "needs PB_LEAN (Lean + Mathlib) and NUCLEUS_REPL"]
fn shutdown_during_verification_closes_the_round_without_restoring_authority() {
    verifier_interruption_case(true);
}

fn verifier_interruption_case(shutdown: bool) {
    let (pb, _) = lean_env();
    let server = FakeHttp::start();
    // The candidate's probe in the verifier's scratch proves the verifier is
    // inside this evaluation (a cold `import Mathlib`, seconds) when
    // interrupted: its proof text, since registering the statement at the
    // rollout's start writes a probe of the frozen file too. The candidate
    // cannot signal it itself: code that runs while Lean elaborates is
    // refused.
    let file = proof("demo_a", "n + 0");
    server.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[
                ("write", "shell_exec", json!({"command":format!("cat > demo_a.lean <<'EOF'\n{file}EOF")})),
                ("submit-0", "explore_submit", json!({"name":"demo_a.lean"})),
            ],
        ),
    ));
    for i in 1..8 {
        server.script(Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[(&format!("submit-{i}"), "explore_submit", json!({"name":"demo_a.lean"}))],
            ),
        ));
    }
    let policies = tempfile::tempdir().unwrap();
    let mut scenario = world(&pb, &server, policies.path().to_path_buf());
    std::fs::copy(prompts_dir().join("prover-agent.j2"), scenario.host("tasks/prompts/prover-agent.j2"))
        .unwrap();
    scenario.add_shell("shell", &pb);
    let mut run_task = task(RUN, &["demo_a"], None);
    run_task["prompt_template_file"] = json!("prompts/prover-agent.j2");
    run_task["concurrency"] = json!(1);
    run_task["limits"] = json!({"w":1,"k1":1,"calls_per_continuation":3,"fanout":1,"model_calls_per_problem":8,"max_usd_per_problem":CEILING_USD});
    scenario.add_discovery_client("client", &run_task, 100, &[]);
    let (mut supervisor, harness) = boot(&scenario);
    let lean_process = supervisor.provider_of(&endpoint("lean")).unwrap();
    let client =
        supervisor.boot(program("client"), vec![cap("discovery", &["start", "status"], &[])]).unwrap();
    assert!(
        wait_for(Duration::from_secs(120), || {
            std::fs::read_dir(scenario.host("lean-scratch")).is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    std::fs::read_to_string(e.path()).is_ok_and(|probe| {
                        probe.contains("simp [demo_a_f]") && probe.contains("#print axioms")
                    })
                })
            })
        }),
        "evaluation reached the injected interruption point"
    );
    let run_dir = scenario.run_dir("discovery", RUN);
    if shutdown {
        let report = supervisor.shutdown();
        assert!(report.is_clean(), "{report:?}");
        let rounds = lines(&run_dir.join("demo_a/rounds.jsonl"));
        assert_eq!(
            rounds.last().and_then(|r| r["reason"].as_str()),
            Some("terminated"),
            "shutdown must close even while a kernel call is cancelled: {rounds:?}"
        );
        // Neither the service nor the run's process.
        let closers = [vec![harness], scenario.children_of(harness)].concat();
        assert!(
            !scenario.events().iter().any(
                |e| matches!(e, SecurityEvent::ProtocolViolation { process, .. } if closers.contains(process))
            ),
            "cleanup must not issue calls under revoked authority"
        );
    } else {
        nucleus_e2e::sigkill(supervisor.host_pid(lean_process.0).unwrap());
        assert!(supervisor.wait_until(Duration::from_secs(120), |s| !s.is_live(client)));
        let received = server.received();
        assert_eq!(received.len(), 8, "retries remain bounded by search plus collection");
        let last: Value = serde_json::from_str(&received.last().unwrap().body).unwrap();
        let context = last["messages"].to_string();
        assert!(
            !context.contains("already submitted"),
            "verification failure must leave submission open: {context}"
        );
        assert!(
            context.contains("endpoint has no provider") || context.contains("provider went away"),
            "actual provider failure reaches the worker: {context}"
        );
        assert_eq!(lines(&run_dir.join("demo_a/nodes.jsonl")).len(), 1, "no successful artifact fabricated");
        let result = worker_result(&scenario, RUN, "demo_a", "t000001");
        assert_eq!(result["attempts"], 8);
        assert_eq!(result["submitted"], false);
        let report = supervisor.shutdown();
        assert!(report.is_clean(), "{report:?}");
    }
}
