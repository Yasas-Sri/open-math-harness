//! Background shell jobs end to end, with real confined processes: the
//! session's agent runs a long command through the projects service's shell; a
//! person moves it to the background from the window and the agent's call
//! answers at once; stopping the agent leaves the job running; its end stays
//! pending until the person continues, then reaches the next agent with its
//! exit status, last lines and output path; a job the agent starts in the
//! background and then stops ends with its whole tree.

use std::time::{Duration, Instant};

use nucleus_e2e::binaries;
use nucleus_platform::{Bind, ProgramSpec};

use super::*;

const RUN: &str = "JOBS-RUN: run the long scan";

/// How many host processes have `marker` as one of their arguments.
fn running(marker: &str) -> usize {
    let has_marker = |entry: &std::fs::DirEntry| {
        std::fs::read(entry.path().join("cmdline"))
            .is_ok_and(|cmdline| cmdline.split(|byte| *byte == 0).any(|arg| arg == marker.as_bytes()))
    };
    std::fs::read_dir("/proc").unwrap().flatten().filter(has_marker).count()
}

fn call(id: &str, name: &str, arguments: Value) -> Scripted {
    Scripted::json(200, &tool_request(MODEL, &[(id, name, arguments)]))
}

/// The VM's path for a session's shell: the projects service (no identities here,
/// so its hosts run as this test's user) serves `shell` and `jobs` and may
/// wake sessions; the sessions service hands each agent a shell in the
/// default project's repository and `jobs` on its own session; the window
/// may move a running command to the background.
fn boot(server: &FakeHttp) -> (Scenario, Supervisor) {
    let mut scenario = Scenario::new();
    for dir in ["projects/default/repo", "home", "jobs-state"] {
        std::fs::create_dir_all(scenario.host(dir)).unwrap();
    }
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("chat", MODEL, &["--max-steps", "8"]);
    scenario.add_sessions("sessions", "chat");
    scenario.add_args("sessions", &["--session-scope", "jobs"]);
    scenario.add_labelled_ui("ui");
    let mut config = scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64);
    let projects = ProgramSpec {
        executable: binaries().projects.clone(),
        args: ["--root", "/projects", "--home", "/home", "--programs", "/programs", "--jobs", "/jobs"]
            .map(str::to_owned)
            .to_vec(),
        binds: vec![
            Bind { host: scenario.host("projects"), guest: "/projects".into(), writable: true },
            Bind { host: scenario.host("home"), guest: "/home".into(), writable: true },
            Bind { host: binaries().shell.clone(), guest: "/programs/nucleus-shell".into(), writable: false },
            Bind { host: scenario.host("jobs-state"), guest: "/jobs".into(), writable: true },
        ],
        network: true,
    };
    config.programs.insert(program("projects"), projects);
    let supervisor = scenario.start_with(config).unwrap();
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let fixed = |endpoint: &str, actions: &[&str], resource: &[&str]| CapSpec {
        delegable: false,
        ..cap(endpoint, actions, resource)
    };
    supervisor
        .boot(
            program("projects"),
            vec![
                fixed("kernel", &["provide"], &["endpoint", "projects"]),
                fixed("kernel", &["provide"], &["endpoint", "shell"]),
                fixed("kernel", &["provide"], &["endpoint", "jobs"]),
                cap("kernel", &["create_endpoint"], &["endpoint"]),
                fixed("kernel", &["spawn_executable"], &["executable", "programs"]),
                fixed("conversation", &["wake"], &[]),
            ],
        )
        .unwrap();
    let jobs = endpoint("jobs");
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s.provider_of(&jobs).is_some()),
        "the projects service serves jobs"
    );
    let sessions_grants = vec![
        fixed("kernel", &["provide"], &["endpoint", "sessions"]),
        nucleus_e2e::session_spawn(),
        cap("conversation", &["receive", "post", "history", "remind", "reminders", "cancel_reminder"], &[]),
        fixed("conversation", &["clear_reminders", "list"], &[]),
        cap("model", &["complete", "start"], &["deepseek"]),
        cap("shell", &["exec"], &["projects", "default", "repo"]),
        cap("jobs", &["register", "list", "output", "stop"], &[]),
    ];
    supervisor.boot(program("sessions"), sessions_grants).unwrap();
    let target = endpoint("sessions");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some()));
    let window = fixed("jobs", &["background"], &[]);
    supervisor.boot(program("ui"), [ui_grants(&[], true), vec![window]].concat()).unwrap();
    (scenario, supervisor)
}

fn wait_log(scenario: &Scenario, what: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let log = scenario.conversation_log("s1");
        if done(&log) {
            return log;
        }
        assert!(Instant::now() < deadline, "{what} never happened: {log:#?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn result_of<'a>(log: &'a [Value], call: &str) -> Option<&'a Value> {
    log.iter().find(|r| r["kind"] == "tool_result" && r["call_id"] == call)
}

#[test]
fn a_moved_command_runs_past_stop_waits_for_continuation_and_a_stopped_job_takes_its_tree() {
    const STOP: &str = "JOBS-STOP: stop the background job";
    let server = FakeHttp::start();
    let mark = std::process::id();
    let (go, started) = (format!("go-{mark}"), format!("started-{mark}"));
    // A command that runs until the test lets it end.
    let long = format!("touch {started}; until [ -e {go} ]; do sleep 0.1; done; echo finished-{mark}");
    server.script_for(RUN, call("c1", "shell_exec", json!({"command": long, "timeout_ms": 120_000})));
    // Lei's window: what it does is recorded as Lei's account.
    let lei = nucleus_e2e::account_of("Lei");
    let moved_by = format!("Moved to the background by a person (account {lei})");
    server.script_for(&moved_by, Scripted::json(200, &completion(MODEL, "It runs.")));
    // After the person continues, the agent processes its end and starts another job, whose
    // processes leave their group and session.
    let escape = format!("304.{mark}");
    let detached = format!("setsid sh -c 'exec sleep {escape}' & timeout {escape} sleep {escape} & wait");
    server.script_for(
        "ended: exit status 0",
        call("c2", "shell_exec", json!({"command": detached, "background": true})),
    );
    server.script_for("Started in the background", Scripted::json(200, &completion(MODEL, "Started.")));
    let (scenario, mut supervisor) = boot(&server);
    let mut ui = scenario.connect_ui_as("ui", Some("Lei"));
    let repo = scenario.host("projects/default/repo");

    ui.request(json!({"op": "sessions.create", "session": "s1"})).unwrap();
    ui.say("s1", "u1", RUN).unwrap();
    assert!(wait_for(Duration::from_secs(60), || repo.join(&started).exists()), "the command never started");
    let moved = ui.request(json!({"op": "jobs.background", "session": "s1", "call": "c1"})).unwrap();
    let job = moved["job"]["job"].as_str().unwrap().to_owned();
    assert_eq!(
        (&moved["job"]["by"], &moved["job"]["author"], &moved["job"]["session"], &moved["job"]["state"]),
        (&json!("person"), &json!(lei), &json!("s1"), &json!("running"))
    );
    // The agent's call answers at once, saying who moved it and how to follow it.
    let log = wait_log(&scenario, "the moved call's answer", |log| result_of(log, "c1").is_some());
    let summary = result_of(&log, "c1").unwrap()["summary"].as_str().unwrap().to_owned();
    assert!(summary.contains(&job) && summary.contains(&moved_by), "{summary}");

    // Stopping the agent leaves the job running, but its eventual completion
    // cannot wake the session's next agent without a user message.
    wait_log(&scenario, "the first turn's end", |log| log.iter().any(|r| r["kind"] == "turn_complete"));
    ui.request(json!({"op": "sessions.stop", "session": "s1"})).unwrap();
    ui.request(json!({"op": "sessions.create", "session": "s1"})).unwrap();
    let calls_before_completion = server.received().len();
    std::fs::write(repo.join(&go), "").unwrap();
    let pending = wait_log(&scenario, "the job's completion reminder", |log| {
        log.iter().any(|r| {
            r["kind"] == "reminder_set"
                && r["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&format!("Background job {job} ended")))
        })
    });
    assert!(!pending.iter().any(|r| r["kind"] == "reminder_fired"));
    assert!(
        !wait_for(Duration::from_secs(1), || server.received().len() > calls_before_completion),
        "a background completion must not resume stopped work"
    );
    ui.say("s1", "continue", "Continue and process the finished job.").unwrap();
    let woken = wait_for(Duration::from_secs(60), || {
        server.received().iter().any(|r| r.body.contains("ended: exit status 0"))
    });
    assert!(woken, "the explicit continuation did not receive the job's result");
    let wake = server.received().into_iter().find(|r| r.body.contains("ended: exit status 0")).unwrap().body;
    // The path a command of the project sees its output at: in its folder.
    let path = format!("/projects/default/.nucleus/jobs/{job}.log");
    for part in [format!("Background job {job} ended"), format!("finished-{mark}"), path] {
        assert!(wake.contains(&part), "{part} missing from the wake-up: {wake}");
    }

    // The job the agent started in the background: stopped by the agent, with
    // everything it started, wherever it moved.
    let log = wait_log(&scenario, "the background start's answer", |log| result_of(log, "c2").is_some());
    let started = result_of(&log, "c2").unwrap()["summary"].as_str().unwrap().to_owned();
    assert!(started.contains("Started in the background"), "{started}");
    assert!(wait_for(TIMEOUT, || running(&escape) == 3), "the background job did not start");
    let second = started.split("as job ").nth(1).and_then(|rest| rest.split(':').next()).unwrap().to_owned();
    server.script_for(STOP, call("c3", "shell_jobs", json!({"action": "stop", "job": second})));
    server.script_for(r#""tool_call_id":"c3""#, Scripted::json(200, &completion(MODEL, "Stopped.")));
    ui.say("s1", "u2", STOP).unwrap();
    assert!(wait_for(TIMEOUT, || running(&escape) == 0), "a stopped job's processes outlived it");
    assert!(supervisor.shutdown().is_clean());
}
