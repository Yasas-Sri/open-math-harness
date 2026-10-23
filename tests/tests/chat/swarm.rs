//! Swarms end to end, with real confined processes: a session's agent starts a
//! swarm through the swarm service, whose members are this chat program in
//! member mode, and one scripted model answers them all. What is checked is
//! what must be enforced, not advised: a member uses no other tool before it
//! holds a direction; a duplicate direction is refused, naming the one it
//! repeats; a member writes no other member's directory and holds none of the
//! agent's actions; a member's report wakes the session once, through `wake`,
//! whole; a report over its bound is refused, never cut; the agent's rejection
//! continues the member on its own transcript with the reason, and its accept
//! stops the other members; the agent reads the swarm's records and stops it
//! with its own actions; the swarm's records are its own file, never the
//! conversation, which a team's window reads through the UI service; a swarm a
//! restart cut short is recorded interrupted; a deleted project's swarms are
//! forgotten, records and all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;
use nucleus_e2e::http::Received;

/// The person's message, the members' tasks, the duplicate check and the
/// wake-up each carry their own words: the model's answers are scripted by them.
const START: &str = "SWARM-START: work this with a swarm of two";
const M1: &str = "You are member m1 of swarm";
const M2: &str = "You are member m2 of swarm";
const CHECK: &str = "ultramath_duplicate_verdict";
const JUDGE: &str = "submitted a solution under direction";
const RETRY: &str = "The judge rejected your solution under direction d1";
/// The swarm's phases take several model calls of several processes each.
const PHASE: Duration = Duration::from_secs(60);

/// The member's directory as its task names it: `.ultramath/<id>/<member>`.
fn dir_of(received: &Received, member: &str) -> String {
    let marker = "Your own directory is ";
    let at =
        received.body.find(&format!("{marker}.ultramath/")).expect("a member's task names its directory")
            + marker.len();
    let rest = &received.body[at..];
    let end = rest.find(&format!("/{member}/")).expect("its directory ends with its name");
    format!("{}/{member}", &rest[..end])
}

fn call(id: &str, name: &str, arguments: Value) -> Scripted {
    Scripted::json(200, &tool_request(MODEL, &[(id, name, arguments)]))
}

/// Every record of session s1's swarms, from their files in the swarm
/// service's state, each file in order.
fn records(state: &std::path::Path) -> Vec<Value> {
    let Ok(files) = std::fs::read_dir(state.join("records/default/s1")) else { return Vec::new() };
    let mut files: Vec<_> = files.flatten().map(|f| f.path()).collect();
    files.sort();
    files
        .iter()
        .flat_map(|file| {
            std::fs::read_to_string(file).unwrap_or_default().lines().map(str::to_owned).collect::<Vec<_>>()
        })
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn swarm_records(records: &[Value], event: &str) -> Vec<Value> {
    records.iter().filter(|r| r["event"] == event).cloned().collect()
}

fn offered(received: &Received) -> Vec<String> {
    let body: Value = serde_json::from_str(&received.body).unwrap();
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

/// What a model request asks of the provider's reasoning.
fn effort_asked(received: &Received) -> Value {
    serde_json::from_str::<Value>(&received.body).unwrap()["reasoning"].clone()
}

/// Waits until `done` holds of what `read` reads.
fn wait_for(what: &str, read: impl Fn() -> Vec<Value>, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + PHASE;
    loop {
        let read = read();
        if done(&read) {
            return read;
        }
        assert!(Instant::now() < deadline, "{what} never happened: {read:#?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn woken(log: &[Value]) -> Vec<Value> {
    log.iter().filter(|r| r["kind"] == "reminder_set" && r["source"]["action"] == "wake").cloned().collect()
}

/// The VM's swarm path: files and a shell over the projects, the model, the
/// conversation, the sessions service handing each agent the agent actions
/// on its session, the swarm service, and a labelled window. A swarm a
/// previous service left running is in its state.
fn boot(server: &FakeHttp) -> (Scenario, Supervisor, std::path::PathBuf) {
    boot_options(server, &[])
}

fn boot_options(server: &FakeHttp, extra: &[&str]) -> (Scenario, Supervisor, std::path::PathBuf) {
    let mut scenario = Scenario::new();
    scenario.write("data/projects/default/repo/README.md", "The problem.");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_shell_over("shell", "data");
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    let mut args = vec!["--max-steps", "8"];
    args.extend_from_slice(extra);
    scenario.add_chat_agent("chat", MODEL, &args);
    scenario.add_sessions("sessions", "chat");
    scenario.add_args("sessions", &["--session-scope", "swarm"]);
    scenario.add_swarm("swarm", MODEL, &["--max-output-tokens", "4096"]);
    scenario.add_labelled_ui("ui");
    // The UI service's part when it deletes a project.
    let forget =
        json!({"tool": "call", "endpoint": "swarm", "action": "forget", "path": ["default"], "payload": {}});
    scenario.add_agent("deleter", &json!({"turns": [{"tool_calls": [forget]}]}), None);
    // A swarm a previous service left running: the restart marks it interrupted.
    let state = scenario.swarm_state("swarm");
    let left = json!({"v": 1, "swarm": "sold", "project": "default", "session": "s1"});
    std::fs::write(state.join("sold.json"), left.to_string()).unwrap();

    let supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "fs", "fs");
    scenario.boot_service(&supervisor, "shell", "shell");
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let mut provide = cap("kernel", &["provide"], &["endpoint", "swarm"]);
    let mut spawn = cap("kernel", &["spawn_executable"], &["executable", "programs", "nucleus-chat-agent"]);
    let mut talk = cap("conversation", &["wake", "ultramath_ended"], &[]);
    for grant in [&mut provide, &mut spawn, &mut talk] {
        grant.delegable = false;
    }
    let swarm_grants = vec![
        provide,
        spawn,
        talk,
        cap("model", &["complete"], &["deepseek"]),
        cap("fs", &["read", "list", "create", "append", "edit"], &["projects"]),
        cap("shell", &["exec"], &["projects"]),
        cap("swarm", &["board", "register", "abandon", "post", "submit", "progress"], &[]),
    ];
    supervisor.boot(program("swarm"), swarm_grants).unwrap();
    let target = endpoint("swarm");
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some()),
        "the swarm service provides"
    );
    let mut sessions_provide = cap("kernel", &["provide"], &["endpoint", "sessions"]);
    let mut clear = cap("conversation", &["clear_reminders", "list"], &[]);
    sessions_provide.delegable = false;
    clear.delegable = false;
    let sessions_grants = vec![
        sessions_provide,
        nucleus_e2e::session_spawn(),
        cap("conversation", &["receive", "post", "history", "remind", "reminders", "cancel_reminder"], &[]),
        clear,
        cap("model", &["complete", "start"], &["deepseek"]),
        cap("fs", &["read", "list"], &["projects", "default", "repo"]),
        cap("swarm", &["start", "status", "verdict", "stop", "records"], &[]),
    ];
    supervisor.boot(program("sessions"), sessions_grants).unwrap();
    let target = endpoint("sessions");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some()));
    // A team's window reads a session's swarms, and nothing else of them.
    let mut read = cap("swarm", &["status", "records"], &[]);
    read.delegable = false;
    supervisor
        .boot(
            program("ui"),
            [
                ui_grants(&[], true),
                vec![read, CapSpec { delegable: false, ..cap("sessions", &["interrupt"], &[]) }],
            ]
            .concat(),
        )
        .unwrap();
    (scenario, supervisor, state)
}

#[test]
fn a_swarm_claims_directions_judges_a_submission_and_stops_the_others() {
    let server = FakeHttp::start();
    // The duplicate checks, in order: the second registration repeats d1, the third does not.
    server.script_for(
        CHECK,
        call("v1", CHECK, json!({"duplicate_of": "d1", "reason": "the same induction, restated"})),
    );
    server
        .script_for(CHECK, call("v2", CHECK, json!({"duplicate_of": null, "reason": "a different method"})));
    // m1 tries a file before it holds a direction, registers, writes, posts and submits.
    server.script_for(M1, call("c1", "fs_read", json!({"path": "README.md"})));
    server.script_for(
        M1,
        call(
            "c2",
            "ultramath_register",
            json!({"direction": "Induction on n with a strengthened hypothesis"}),
        ),
    );
    server.script_with(M1, |received| {
        let (own, other) = (dir_of(received, "m1"), dir_of(received, "m1").replace("/m1", "/m2"));
        Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[
                    ("c3", "fs_create", json!({"path": format!("{other}/stolen.md"), "content": "x"})),
                    ("c4", "fs_create", json!({"path": format!("{own}/proof.md"), "content": "Q.E.D."})),
                ],
            ),
        )
    });
    server.script_with(M1, |received| {
        let paths = [format!("{}/proof.md", dir_of(received, "m1"))];
        call("c5", "ultramath_post", json!({"kind": "lemma", "text": "f(2) = 4", "paths": paths}))
    });
    // m1 submits only once m2 holds d2, and m2 answers only once m1 holds d1:
    // the order of the registrations, and that m2 runs when m1 is accepted.
    let (second, first) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let wait = |gate: &AtomicBool| {
        let deadline = Instant::now() + PHASE;
        while !gate.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let open = Arc::clone(&second);
    server.script_with(M1, move |received| {
        wait(&open);
        let paths = [format!("{}/proof.md", dir_of(received, "m1"))];
        call("c6", "ultramath_submit", json!({"report": "By induction on n: ... Q.E.D.", "paths": paths}))
    });
    let open = Arc::clone(&first);
    server.script_with(M2, move |_| {
        wait(&open);
        call("e1", "ultramath_register", json!({"direction": "Strong induction on n"}))
    });
    server.script_for(
        M2,
        call("e2", "ultramath_register", json!({"direction": "A parity argument on residues mod 4"})),
    );
    server.script_for(M2, call("e3", "shell_exec", json!({"command": "sleep 120", "timeout_ms": 150_000})));
    // Woken by each report, the agent judges it, reading the swarm's records
    // first: the first is rejected, and m1, continued with the reason,
    // submits again.
    server.script_for(JUDGE, call("j0", "ultramath_records", json!({"limit": 2})));
    let reject = json!({"member": "m1", "accept": false, "reason": "Step 3 assumes n odd."});
    let accept = json!({"member": "m1", "accept": true, "reason": "Checked every step."});
    for (verdict, said) in [(reject, "m1 tries again."), (accept, "m1 solved it.")] {
        server.script_for(JUDGE, call("j1", "ultramath_status", json!({"direction": "d1"})));
        server.script_for(JUDGE, call("j2", "ultramath_verdict", verdict));
        server.script_for(JUDGE, Scripted::json(200, &completion(MODEL, said)));
    }
    server.script_for(
        RETRY,
        call("r1", "ultramath_submit", json!({"report": "By strong induction: ... Q.E.D."})),
    );
    let task = "Find every n for which n^2 + n + 41 is composite, and prove it.";
    server.script_for(
        START,
        call("s1", "ultramath_start", json!({"task": task, "members": 2, "member_steps": 10, "minutes": 30})),
    );
    server.script_for(START, Scripted::json(200, &completion(MODEL, "Two members are on it.")));

    let (scenario, mut supervisor, state) = boot(&server);
    let mut ui = scenario.connect_ui_as("ui", Some("Lei"));

    // The restart's leftover is recorded interrupted in its own records, and
    // forgotten.
    let read = || records(&state);
    wait_for("the interrupted swarm's record", read, |records| {
        swarm_records(records, "ended").iter().any(|r| r["swarm"] == "sold" && r["outcome"] == "interrupted")
    });
    assert!(!state.join("sold.json").exists());

    let created = ui.request(json!({"op": "sessions.create", "session": "s1"})).unwrap();
    let granted = created["grants"].to_string();
    assert!(granted.contains(r#"["default","s1"]"#) && granted.contains("verdict"), "{granted}");
    ui.say("s1", "u1", START).unwrap();
    wait_for("m1's direction", read, |log| {
        swarm_records(log, "direction").iter().any(|r| r["member"] == "m1" && r["direction"] == "d1")
    });
    first.store(true, Ordering::Release);
    wait_for("m2's direction", read, |log| {
        swarm_records(log, "direction").iter().any(|r| r["member"] == "m2" && r["direction"] == "d2")
    });
    second.store(true, Ordering::Release);
    let log = wait_for("the swarm's end", read, |log| {
        !swarm_records(log, "ended").iter().all(|r| r["swarm"] == "sold")
    });
    let conversation = wait_for(
        "the completion reference",
        || scenario.conversation_log("s1"),
        |log| log.iter().any(|r| r["kind"] == "ultramath_ended"),
    );
    let completions: Vec<_> = conversation.iter().filter(|r| r["kind"] == "ultramath_ended").collect();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0]["swarm"], swarm_records(&log, "started")[0]["swarm"]);
    assert!(completions[0]["posted_by"].is_u64());

    let started = &swarm_records(&log, "started")[0];
    let dir = started["dir"].as_str().unwrap().to_owned();
    assert_eq!(started["members"], json!(["m1", "m2"]));
    // Its start named no reasoning effort: its members think at the highest.
    assert_eq!(started["reasoning_effort"], "max", "{started}");
    let received = server.received();
    let of = |key: &str| received.iter().filter(|r| r.body.contains(key)).collect::<Vec<_>>();
    // The agent starts, judges and stops swarms; a member holds none of that.
    let agent_tools = offered(of(START)[0]);
    for tool in
        ["ultramath_start", "ultramath_status", "ultramath_verdict", "ultramath_stop", "ultramath_records"]
    {
        assert!(agent_tools.contains(&tool.to_owned()), "{agent_tools:?}");
    }
    for request in of(M1).into_iter().chain(of(M2)) {
        assert_eq!(effort_asked(request), json!({"effort": "max"}));
        let tools = offered(request);
        assert!(tools.contains(&"ultramath_register".to_owned()), "{tools:?}");
        assert!(
            !tools.iter().any(|t| [
                "ultramath_verdict",
                "ultramath_start",
                "ultramath_stop",
                "ultramath_status",
                "ultramath_records"
            ]
            .contains(&t.as_str())),
            "{tools:?}"
        );
    }
    // Before it held a direction, m1's file call was refused by its own loop.
    assert!(of(M1)[1].body.contains("you hold no direction"), "{}", of(M1)[1].body);
    // A duplicate is refused naming the direction it repeats; another is taken.
    let refused = swarm_records(&log, "refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(
        (refused[0]["member"].as_str(), refused[0]["duplicate_of"].as_str()),
        (Some("m2"), Some("d1"))
    );
    assert!(of(M2).iter().any(|r| r.body.contains("refused: the same direction as d1")));
    assert!(swarm_records(&log, "direction").iter().any(|r| r["member"] == "m2" && r["direction"] == "d2"));
    assert_eq!(of(CHECK).len(), 2, "one bounded check per registration that needed one");
    // m1 wrote its own directory and not m2's.
    let repo = scenario.host("data/projects/default/repo");
    assert!(repo.join(&dir).join("m1/proof.md").is_file());
    assert!(!repo.join(&dir).join("m2/stolen.md").exists(), "a member wrote another member's directory");
    assert!(of(M1)[3].body.contains("permission_denied"), "{}", of(M1)[3].body);
    assert!(
        repo.join(".ultramath/.gitignore").is_file()
            && repo.join(&dir).join("transcripts/m1.jsonl").is_file()
    );
    // What m1 found and submitted is on the board and in the swarm's records.
    assert!(swarm_records(&log, "finding").iter().any(|r| r["finding"] == "f1" && r["direction"] == "d1"));
    assert_eq!(swarm_records(&log, "submission")[0]["member"], "m1");
    // Each report woke the session once through `wake`, carrying the report,
    // and that turn judged it.
    let wakes = woken(&conversation);
    assert_eq!(wakes.len(), 2, "one wake-up a report: {wakes:#?}");
    assert!(wakes[0]["text"].as_str().unwrap().contains("By induction on n: ... Q.E.D."), "{}", wakes[0]);
    assert!(
        wakes
            .iter()
            .all(|woke| conversation.iter().any(|r| r["kind"] == "reminder_fired" && r["id"] == woke["id"]))
    );
    // The agent read the swarm's records, a page at a time.
    let page = conversation.iter().find(|r| r["kind"] == "tool_result" && r["call_id"] == "j0").unwrap();
    let page = page["summary"].as_str().unwrap();
    assert!(page.contains(r#""event":"started""#) && page.contains(r#""next":1"#), "{page}");
    let verdicts: Vec<(Value, Value)> = swarm_records(&log, "verdict")
        .into_iter()
        .map(|r| (r["member"].clone(), r["accepted"].clone()))
        .collect();
    assert_eq!(verdicts, [(json!("m1"), json!(false)), (json!("m1"), json!(true))]);
    // Rejected, m1 was started again on its own transcript, the reason framed as the judge's.
    let retried = of(RETRY);
    assert!(retried[0].body.contains("| Step 3 assumes n odd."), "{}", retried[0].body);
    assert!(retried[0].body.contains("You are member m1 of swarm"), "it continued its own transcript");
    let runs: Vec<Value> = swarm_records(&log, "member")
        .into_iter()
        .filter(|r| r["member"] == "m1" && r["state"] == "running")
        .map(|r| r["process"].clone())
        .collect();
    assert!(runs.len() == 2 && runs[0] != runs[1] && runs.iter().all(Value::is_u64), "{runs:?}");
    assert_eq!(swarm_records(&log, "submission").len(), 2);
    // Accepting stopped m2 at once and ended the swarm as solved.
    assert!(swarm_records(&log, "member").iter().any(|r| r["member"] == "m2" && r["state"] == "stopped"));
    assert!(
        swarm_records(&log, "direction").iter().any(|r| r["direction"] == "d1" && r["state"] == "succeeded")
    );
    let ended: Vec<Value> =
        swarm_records(&log, "ended").into_iter().filter(|r| r["swarm"] != "sold").collect();
    assert_eq!(ended[0]["outcome"], "solved");
    let markers = std::fs::read_dir(&state).unwrap().flatten().filter(|e| e.path().is_file()).count();
    assert_eq!(markers, 0, "nothing is left to mark interrupted");
    // The window's read-only card reads the same through the UI service.
    let id = started["swarm"].as_str().unwrap();
    let page =
        ui.request(json!({"op": "swarm.records", "session": "s1", "swarm": id, "limit": 200})).unwrap();
    assert_eq!(
        (page["records"][0]["event"].as_str(), page["records"].as_array().unwrap().len()),
        (Some("started"), read().iter().filter(|r| r["swarm"] == id).count())
    );
    let status = ui.request(json!({"op": "swarm.status", "session": "s1"})).unwrap();
    assert_eq!((status["swarm"].as_str(), status["state"].as_str()), (Some(id), Some("solved")));

    drop(ui);
    assert!(supervisor.shutdown().is_clean());
}

/// One member's report: one over its bound is refused, saying to shorten
/// it, and never cut; the next reaches the agent whole in one wake-up, and
/// nothing wakes it again when its turn ends without a verdict. Then the
/// agent stops the swarm with its own action.
#[test]
fn a_report_wakes_the_agent_once_whole_and_the_agent_stops_the_swarm() {
    const ONE: &str = "SWARM-START: work this with one member";
    let report = format!("n^5 - n vanishes mod 2, 3 and 5. Q.E.D. {}", ".".repeat(1500 - 40));
    assert_eq!(report.len(), nucleus_swarm_interface::MAX_REPORT_BYTES);
    let server = FakeHttp::start();
    let task = "Show that every n^5 - n is divisible by 30.";
    let start = json!({"task": task, "members": 1, "member_steps": 10, "reasoning_effort": "low"});
    server.script_for(ONE, call("old", "swarm_start", start.clone()));
    server.script_for(ONE, call("s1", "ultramath_start", start));
    server.script_for(ONE, Scripted::json(200, &completion(MODEL, "One member is on it.")));
    server.script_for(
        M1,
        call("c1", "ultramath_register", json!({"direction": "Fermat's little theorem mod 2, 3, 5"})),
    );
    server.script_for(M1, call("c2", "ultramath_submit", json!({"report": format!("{report}!")})));
    server.script_for(M1, call("c3", "ultramath_submit", json!({"report": report})));
    // Woken, the agent judges nothing in that turn; a later one stops the swarm.
    server.script_for(JUDGE, Scripted::json(200, &completion(MODEL, "I will check it later.")));
    const STOP: &str = "Stop the swarm.";
    server.script_for(STOP, call("t1", "ultramath_stop", json!({"reason": "The person stopped it."})));
    server.script_for(STOP, Scripted::json(200, &completion(MODEL, "Stopped.")));
    let (scenario, mut supervisor, state) = boot(&server);
    let mut ui = scenario.connect_ui_as("ui", Some("Lei"));

    ui.request(json!({"op": "sessions.create", "session": "s1"})).unwrap();
    ui.say("s1", "u1", ONE).unwrap();
    let log = wait_for(
        "the woken turn's end",
        || scenario.conversation_log("s1"),
        |log| {
            woken(log).first().is_some_and(|woke| {
                let fired = log.iter().position(|r| r["kind"] == "reminder_fired" && r["id"] == woke["id"]);
                fired.is_some_and(|at| log[at..].iter().any(|r| r["kind"] == "turn_complete"))
            })
        },
    );
    let wakes = woken(&log);
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0]["text"].as_str().unwrap().contains(&report), "the report whole: {}", wakes[0]);
    let read = || records(&state);
    assert_eq!(swarm_records(&read(), "started").len(), 1, "the old tool starts no extra swarm");
    let submitted = swarm_records(&read(), "submission");
    assert_eq!(submitted.len(), 1, "the long report was refused: {submitted:?}");
    assert_eq!(submitted[0]["report"], report);
    let received = server.received();
    let members: Vec<_> = received.iter().filter(|r| r.body.contains(M1)).collect();
    assert!(
        members[2].body.contains("too long, shorten it and submit again"),
        "the member was told: {}",
        members[2].body
    );
    assert!(
        server.received().iter().any(|r| r.body.contains("This call was not executed. Call ultramath_start")
            && r.body.contains("Do not move or rename existing data"))
    );
    // The effort its start named is its members', and in its records.
    assert_eq!(swarm_records(&read(), "started")[0]["reasoning_effort"], "low");
    assert!(members.iter().all(|r| effort_asked(r) == json!({"effort": "low"})));

    // Asked to, the agent stops the swarm with its own action.
    ui.say("s1", "u2", STOP).unwrap();
    let swarm = swarm_records(&read(), "started")[0]["swarm"].as_str().unwrap().to_owned();
    let records = wait_for("the stopped swarm's end", read, |records| {
        swarm_records(records, "ended").iter().any(|r| r["swarm"] == swarm.as_str())
    });
    let ended = swarm_records(&records, "ended").into_iter().find(|r| r["swarm"] == swarm.as_str()).unwrap();
    assert_eq!((&ended["outcome"], &ended["note"]), (&json!("stopped"), &json!("The person stopped it.")));
    assert_eq!(ended["calls"], 3, "m1's three model calls, as its loop reported them: {ended}");
    let log = ui.wait_turns("s1", 3);
    assert_eq!(woken(&log).len(), 1, "the agent's own Stop wakes nobody");

    // The project is deleted: its swarms' records and markers go.
    let mut forget = cap("swarm", &["forget"], &[]);
    forget.delegable = false;
    supervisor.boot(program("deleter"), vec![forget]).unwrap();
    let forgot = scenario.wait_done_within("deleter", PHASE);
    assert_eq!(nucleus_e2e::ok_value(&forgot[0]), Some(&json!({})), "{forgot:?}");
    assert!(!state.join("records/default").exists(), "nothing of the project's swarms is left");
    assert!(
        std::fs::read_dir(&state).unwrap().flatten().all(|e| e.file_type().unwrap().is_dir()),
        "no marker"
    );

    drop(ui);
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn model_reasoning_and_mode_are_each_messages_own() {
    let server = FakeHttp::start();
    let (scenario, mut supervisor, _state) = boot_options(&server, &["--compact-at-tokens", "100000"]);
    let mut ui = scenario.connect_ui_as("ui", Some("Lei"));
    ui.request(json!({"op": "sessions.create", "session": "mode"})).unwrap();
    let say = |ui: &mut nucleus_e2e::UiClient, id: &str, settings: &Value, response: Value| {
        server.script(Scripted::json(200, &response));
        let request = json!({"op": "conversation.say", "session": "mode", "message_id": id, "text": "Q.", "settings": settings});
        ui.request(request).unwrap();
    };
    // What the n-th model call ran with: whether it was told Ultramath, its effort, its model.
    let call = |n: usize| {
        let body: Value = serde_json::from_str(&server.received()[n].body).unwrap();
        let system = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system");
        let ultramath =
            system.map(|m| m["content"].as_str().unwrap()).any(|s| s.contains("# Ultramath mode"));
        assert!(body.get("reasoning_mode").is_none(), "harness strategy is not a provider parameter");
        (ultramath, body["reasoning"]["effort"].clone(), body["model"].clone())
    };
    let ultramath = json!({"reasoning_mode": "ultramath"});
    // Every call of an Ultramath turn is told so, also after compaction and an interrupt.
    say(&mut ui, "m1", &ultramath, completion(MODEL, "First."));
    ui.wait_turns("mode", 1);
    let mut large = completion(MODEL, "Second.");
    large["usage"]["prompt_tokens"] = json!(200_000);
    say(&mut ui, "m2", &ultramath, large);
    ui.wait_turns("mode", 2);
    server.script(Scripted::json(200, &completion(MODEL, "SUMMARY: two questions answered.")));
    say(&mut ui, "m3", &ultramath, completion(MODEL, "Third."));
    assert!(ui.wait_turns("mode", 3).iter().any(|r| r["kind"] == "compacted"));
    ui.request(json!({"op": "sessions.interrupt", "session": "mode"})).unwrap();
    say(&mut ui, "m4", &ultramath, completion(MODEL, "Fourth."));
    ui.wait_turns("mode", 4);
    for n in 0..5 {
        assert_eq!(call(n), (true, json!("max"), json!(MODEL)), "call {n}");
    }
    // Waiting messages with different choices are separate turns, in order, without a restart.
    ui.request(json!({"op": "sessions.stop", "session": "mode"})).unwrap();
    say(&mut ui, "m5", &json!({"reasoning_effort": "high"}), completion(MODEL, "Fifth."));
    say(&mut ui, "m6", &ultramath, completion(MODEL, "Sixth."));
    ui.request(json!({"op": "sessions.create", "session": "mode"})).unwrap();
    ui.wait_turns("mode", 6);
    assert_eq!((call(5), call(6).0), ((false, json!("high"), json!(MODEL)), true));
    // A message's model is its turn's, unless the agent holds no capability for it.
    let pro = "deepseek/deepseek-v4-pro";
    say(&mut ui, "m7", &json!({"model": pro}), completion(pro, "Seventh."));
    ui.wait_turns("mode", 7);
    say(&mut ui, "m8", &json!({"model": "openai/gpt-5"}), completion(MODEL, "Eighth."));
    ui.wait_turns("mode", 8);
    assert_eq!((call(7).2, call(8).2), (json!(pro), json!(MODEL)));
    drop(ui);
    assert!(supervisor.shutdown().is_clean());
}
