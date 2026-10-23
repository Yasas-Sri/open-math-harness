//! Irreversible actions behind the approvals guard, end to end with real
//! confined processes: the agent never holds `fs remove`; the guard does, and
//! uses it only when a person, through a front end, approves the exact call.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{Scenario, TIMEOUT, UiClient, cap, mentions, program, wait_for};
use nucleus_platform::Supervisor;
use nucleus_protocol::ProcessId;
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";

struct World {
    scenario: Scenario,
    server: FakeHttp,
    supervisor: Supervisor,
    guard: ProcessId,
}

fn boot() -> World {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("data/work/old.txt", "obsolete");
    scenario.write("data/work/keep.txt", "precious");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_openrouter("openrouter", &server.base_url(), "sk-or-test-APPROVALS", &[], true);
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("chat", MODEL, &[]);
    scenario.add_sessions("sessions", "chat");
    scenario.add_approvals(
        "approvals",
        &json!({"version": 7, "rules": [{"endpoint": "fs", "action": "remove", "resource": ["work"], "effect": "ask"}]}),
    );
    scenario.add_ui("ui");
    scenario.add_ui("narrow-ui");

    let supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "fs", "fs");
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");

    // The guard, and only the guard, holds `fs remove`.
    let mut holds_remove = cap("fs", &["remove"], &["work"]);
    holds_remove.delegable = false;
    let mut provide = cap("kernel", &["provide"], &["endpoint", "approvals"]);
    provide.delegable = false;
    let guard = supervisor.boot(program("approvals"), vec![provide, holds_remove]).unwrap();
    wait_provider(&supervisor, "approvals", guard);

    // The sessions service can hand agents the right to ask, never the right to remove.
    let mut authority = vec![
        cap("kernel", &["provide"], &["endpoint", "sessions"]),
        nucleus_e2e::session_spawn(),
        cap("conversation", &["list"], &[]),
        cap("conversation", &["receive", "post", "history"], &[]),
        cap("model", &["complete", "start"], &["deepseek"]),
        cap("fs", &["read", "list"], &["work"]),
        cap("approvals", &["request"], &["fs", "work"]),
    ];
    authority[0].delegable = false;
    authority[2].delegable = false;
    let sessions = supervisor.boot(program("sessions"), authority).unwrap();
    wait_provider(&supervisor, "sessions", sessions);
    World { scenario, server, supervisor, guard }
}

fn wait_provider(supervisor: &Supervisor, name: &str, process: ProcessId) {
    let target = nucleus_e2e::endpoint(name);
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == process)));
}

fn front_end(world: &World, name: &str, may_decide: bool) -> UiClient {
    let mut grants = vec![
        cap("conversation", &["say", "history"], &[]),
        cap("sessions", &["create", "stop", "list"], &[]),
    ];
    if may_decide {
        grants.push(cap("approvals", &["list", "decide"], &[]));
    }
    for grant in &mut grants {
        grant.delegable = false;
    }
    world.supervisor.boot(program(name), grants).unwrap();
    world.scenario.connect_ui(name)
}

fn pending(ui: &mut UiClient) -> Vec<Value> {
    ui.request(json!({"op": "approvals.list"})).unwrap()["pending"].as_array().cloned().unwrap()
}

fn wait_pending(ui: &mut UiClient) -> Value {
    let mut found = None;
    assert!(wait_for(TIMEOUT, || {
        found = pending(ui).into_iter().next();
        found.is_some()
    }));
    found.unwrap()
}

fn removes_seen_by_fs(world: &World) -> Vec<Value> {
    world.scenario.access_log("data").into_iter().filter(|r| r["action"] == "remove").collect()
}

#[test]
fn a_person_approves_and_the_guard_performs_exactly_that_call() {
    let mut world = boot();
    world.server.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[("c1", "fs_remove", json!({"path": "work/old.txt", "reason": "it is obsolete"}))],
        ),
    ));
    world.server.script(Scripted::json(200, &completion(MODEL, "Deleted work/old.txt.")));
    let mut ui = front_end(&world, "ui", true);

    let session = ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let agent = session["process"].as_u64().unwrap();
    let asks: Vec<&Value> =
        session["grants"].as_array().unwrap().iter().filter(|g| g["endpoint"] == "approvals").collect();
    assert_eq!(asks[0]["actions"], json!(["request"]));
    assert_eq!(asks[0]["resource"], json!(["fs", "work"]));
    assert!(
        !session["grants"].to_string().contains("remove"),
        "the agent is never given `remove`: {session}"
    );

    ui.say("default", "u1", "Please delete work/old.txt").unwrap();
    let waiting = wait_pending(&mut ui);
    assert_eq!(waiting["requester"], agent, "who is asking comes from the kernel");
    assert_eq!(
        (&waiting["endpoint"], &waiting["action"], &waiting["resource"]),
        (&json!("fs"), &json!("remove"), &json!(["work", "old.txt"]))
    );
    assert_eq!((&waiting["reason"], &waiting["policy_version"]), (&json!("it is obsolete"), &json!(7)));
    assert!(world.scenario.host("data/work/old.txt").exists(), "nothing happens while a person is thinking");
    assert!(removes_seen_by_fs(&world).is_empty());

    // A decision has to be about this request as shown.
    let wrong = ui.request(
        json!({"op": "approvals.decide", "approval": waiting["id"], "digest": "0000000000000000", "allow": true}),
    );
    assert_eq!(wrong.unwrap_err()["code"], "invalid_input");
    assert!(world.scenario.host("data/work/old.txt").exists());

    ui.request(
        json!({"op": "approvals.decide", "approval": waiting["id"], "digest": waiting["digest"], "allow": true}),
    )
    .unwrap();
    let log = ui.wait_turns("default", 1);
    let result = log.iter().find(|r| r["kind"] == "tool_result").unwrap();
    assert!(result["error_code"].is_null(), "{result}");
    assert!(!world.scenario.host("data/work/old.txt").exists(), "the approved call ran");
    assert!(world.scenario.host("data/work/keep.txt").exists(), "and only that call");

    // The filesystem service saw the guard, not the agent, and saw it once.
    let removes = removes_seen_by_fs(&world);
    assert_eq!(removes.len(), 1);
    assert_eq!(removes[0]["caller"], world.guard.0);
    assert!(mentions(&removes[0], "old.txt"));
    let again = ui.request(
        json!({"op": "approvals.decide", "approval": waiting["id"], "digest": waiting["digest"], "allow": true}),
    );
    assert_eq!(again.unwrap_err()["code"], "invalid_input", "an approval is used once");

    let record = world.scenario.approvals_log("approvals");
    let events: Vec<&str> = record.iter().map(|r| r["event"].as_str().unwrap()).collect();
    assert_eq!(
        events,
        ["requested", "decided", "dispatching", "outcome"],
        "intent is on record before the call"
    );
    assert_eq!(record[1]["allow"], true);
    assert_ne!(record[1]["decided_by"], agent);
    assert_eq!(record[3]["outcome"], "ok");

    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn a_declined_request_changes_nothing_and_the_model_is_told() {
    let mut world = boot();
    world.server.script(Scripted::json(
        200,
        &tool_request(MODEL, &[("c1", "fs_remove", json!({"path": "work/keep.txt", "reason": "tidying"}))]),
    ));
    world.server.script(Scripted::json(200, &completion(MODEL, "I was not allowed to delete it.")));
    let mut ui = front_end(&world, "ui", true);
    // A front end that was not given the approvals capability sees and decides nothing.
    let mut onlooker = front_end(&world, "narrow-ui", false);

    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ui.say("default", "u1", "Delete work/keep.txt").unwrap();
    let waiting = wait_pending(&mut ui);
    assert_eq!(onlooker.request(json!({"op": "approvals.list"})).unwrap_err()["code"], "permission_denied");
    let forged = json!({"op": "approvals.decide", "approval": waiting["id"], "digest": waiting["digest"], "allow": true});
    assert_eq!(onlooker.request(forged).unwrap_err()["code"], "permission_denied");

    ui.request(
        json!({"op": "approvals.decide", "approval": waiting["id"], "digest": waiting["digest"], "allow": false}),
    )
    .unwrap();
    let log = ui.wait_turns("default", 1);
    let result = log.iter().find(|r| r["kind"] == "tool_result").unwrap();
    assert_eq!(result["error_code"], "permission_denied");
    assert!(result["summary"].as_str().unwrap().contains("declined"), "{result}");
    assert_eq!(std::fs::read_to_string(world.scenario.host("data/work/keep.txt")).unwrap(), "precious");
    assert!(removes_seen_by_fs(&world).is_empty(), "the filesystem service was never asked");

    // What the model was told is what happened.
    let second: Value = serde_json::from_str(&world.server.received()[1].body).unwrap();
    let told = second["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
    assert!(told["content"].as_str().unwrap().contains("permission_denied"));
    drop((ui, onlooker));
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn a_request_dies_with_the_agent_that_made_it() {
    let mut world = boot();
    world.server.script(Scripted::json(
        200,
        &tool_request(MODEL, &[("c1", "fs_remove", json!({"path": "work/old.txt", "reason": "cleanup"}))]),
    ));
    let mut ui = front_end(&world, "ui", true);
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ui.say("default", "u1", "Delete work/old.txt").unwrap();
    let waiting = wait_pending(&mut ui);

    ui.request(json!({"op": "sessions.stop", "session": "default"})).unwrap();
    assert!(wait_for(TIMEOUT, || pending(&mut ui).is_empty()), "the request went with its requester");
    let late = ui.request(
        json!({"op": "approvals.decide", "approval": waiting["id"], "digest": waiting["digest"], "allow": true}),
    );
    assert_eq!(late.unwrap_err()["code"], "invalid_input", "a late approval approves nothing");
    assert!(world.scenario.host("data/work/old.txt").exists());
    assert!(removes_seen_by_fs(&world).is_empty());
    let events: Vec<String> = world
        .scenario
        .approvals_log("approvals")
        .iter()
        .map(|r| r["event"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(events, ["requested", "expired"]);
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
