//! The chat path end to end with real confined processes. The test plays the
//! window: it connects to the UI service's development socket, and can do only
//! what that service's capabilities allow. A (scripted)
//! DeepSeek model drives tool use through the kernel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{
    DiscoveryService, Scenario, TIMEOUT, cap, discovery_grants, endpoint, mentions, program, wait_for,
};
use nucleus_kernel::{CapSpec, SecurityEvent};
use nucleus_platform::Supervisor;
use serde_json::{Value, json};

#[path = "chat/attachments.rs"]
mod attachments;
#[path = "chat/generation.rs"]
mod generation;
#[path = "chat/jobs.rs"]
mod jobs;
#[path = "chat/queue.rs"]
mod queue;
#[path = "chat/reasoning.rs"]
mod reasoning;
#[path = "chat/reminders.rs"]
mod reminders;
#[path = "chat/restore.rs"]
mod restore;
#[path = "chat/skills.rs"]
mod skills;
#[path = "chat/swarm.rs"]
mod swarm;
#[path = "chat/turn_completion.rs"]
mod turn_completion;

const MODEL: &str = "deepseek/deepseek-v4-flash";
const KEY: &str = "sk-or-test-CHAT-0123456789";

/// The sessions service's authority: everything an agent may be given, delegable.
fn sessions_grants() -> Vec<CapSpec> {
    let mut provide = cap("kernel", &["provide"], &["endpoint", "sessions"]);
    let mut clear = cap("conversation", &["clear_reminders", "list"], &[]);
    provide.delegable = false;
    clear.delegable = false;
    vec![
        provide,
        nucleus_e2e::session_spawn(),
        cap("conversation", &["receive", "post", "history", "remind", "reminders", "cancel_reminder"], &[]),
        clear,
        cap("model", &["complete", "start"], &["deepseek"]),
        cap("fs", &["read", "list"], &["work"]),
    ]
}

/// A front end's authority: speak as the user, read history, manage sessions.
fn ui_grants(conversation_scope: &[&str], with_sessions: bool) -> Vec<CapSpec> {
    let mut grants = vec![cap("conversation", &["say", "history", "wait"], conversation_scope)];
    if with_sessions {
        grants.push(cap("sessions", &["create", "stop", "list"], &[]));
    }
    for grant in &mut grants {
        grant.delegable = false;
    }
    grants
}

struct World {
    scenario: Scenario,
    server: FakeHttp,
    supervisor: Supervisor,
}

fn boot(max_steps: &str) -> World {
    boot_with(max_steps, false)
}

/// `streaming` gives the sessions service the `next`/`cancel` capability it needs to let agents pull streams.
fn boot_with(max_steps: &str, streaming: bool) -> World {
    boot_options(max_steps, streaming, &[], false)
}

fn boot_options(max_steps: &str, streaming: bool, extra: &[&str], writable_tools: bool) -> World {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("data/work/notes.txt", "the launch code is 42");
    scenario.write("data/secret.txt", "TOP SECRET");
    scenario.add_fs("fs", "data", "fs");
    // The scripted statuses are the test's script: a retried 500 would eat the next scripted reply.
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    let mut agent_args = vec!["--max-steps", max_steps];
    agent_args.extend_from_slice(extra);
    scenario.add_chat_agent("chat", MODEL, &agent_args);
    scenario.add_sessions("sessions", "chat");
    scenario.add_ui("ui");
    scenario.add_ui("narrow-ui");
    scenario.add_labelled_ui("members-ui");
    // A process outside the session that wakes it now, when a test boots it.
    let wake = json!({"after_ms": 0, "text": "run gen2 finished"});
    let call = json!({"tool": "call", "endpoint": "conversation", "action": "wake", "path": ["default", "default"],
                      "payload": wake});
    scenario.add_agent("waker", &json!({"turns": [{"tool_calls": [call]}]}), None);

    // Parked `receive` calls occupy in-flight slots of the conversation endpoint.
    let supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "fs", "fs");
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let mut grants = sessions_grants();
    if streaming {
        grants.push(cap("model", &["next", "cancel"], &["deepseek"]));
    }
    if writable_tools {
        grants.push(cap("fs", &["create", "append"], &["work"]));
    }
    let sessions = supervisor.boot(program("sessions"), grants).unwrap();
    let target = nucleus_e2e::endpoint("sessions");
    assert!(
        supervisor
            .wait_until(nucleus_e2e::TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == sessions))
    );
    World { scenario, server, supervisor }
}

/// The conversation as a reader sees it. The agent's per-step `model_call` reports
/// and the `delivered` markers are bookkeeping, checked separately.
fn conversation(log: &[Value]) -> Vec<Value> {
    log.iter().filter(|r| r["kind"] != "model_call" && r["kind"] != "delivered").cloned().collect()
}

fn kinds(log: &[Value]) -> Vec<&str> {
    log.iter().map(|r| r["kind"].as_str().unwrap()).collect()
}

fn sent_messages(server: &FakeHttp, request: usize) -> Vec<Value> {
    let body: Value = serde_json::from_str(&server.received()[request].body).unwrap();
    body["messages"].as_array().unwrap().clone()
}

#[test]
fn a_window_chats_through_a_ui_service_that_holds_only_capabilities() {
    let mut world = boot("8");
    world.server.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[
                ("call_1", "fs_read", json!({"path": "work/notes.txt"})),
                ("call_2", "fs_read", json!({"path": "secret.txt"})),
                ("call_3", "fs_read", json!({"path": "work/../secret.txt"})),
                ("call_4", "shell_exec", json!({"cmd": "cat /data/secret.txt"})),
            ],
        ),
    ));
    world.server.script(Scripted::json(
        200,
        &completion(MODEL, "The notes say 42. I was not allowed to read secret.txt."),
    ));
    // A request the provider rejects is not made again.
    world.server.script(Scripted::json(400, &json!({"error": {"message": "request rejected"}})));
    world.server.script(Scripted::json(200, &completion(MODEL, "You asked about the notes earlier.")));

    let ui_process = world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");

    // The front end sees its own authority, and it is modest.
    let me = ui.request(json!({"op": "whoami"})).unwrap();
    let endpoints: Vec<&str> =
        me["handles"].as_array().unwrap().iter().map(|h| h["endpoint"].as_str().unwrap()).collect();
    assert_eq!(endpoints, ["conversation", "sessions"]);
    // A second window (a phone) is served while the first stays connected.
    let mut phone = world.scenario.connect_ui("ui");
    assert_eq!(phone.request(json!({"op": "whoami"})).unwrap()["handles"], me["handles"]);
    assert!(ui.request(json!({"op": "whoami"})).is_ok());

    // Start a session: the sessions service spawns the agent with narrowed grants and says what they are.
    let created = ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    assert_eq!(created["live"], true);
    let granted: Vec<String> = created["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| format!("{} {} {}", g["endpoint"].as_str().unwrap(), g["resource"], g["actions"]))
        .collect();
    assert_eq!(
        granted,
        [
            r#"conversation ["default","default"] ["post","receive"]"#,
            r#"conversation ["default","default"] ["history"]"#,
            r#"conversation ["default","default"] ["cancel_reminder","remind","reminders"]"#,
            r#"model ["deepseek"] ["complete","start"]"#,
            r#"fs ["work"] ["list","read"]"#
        ]
    );
    let again = ui.request(json!({"op": "sessions.create", "session": "default"}));
    assert_eq!(again.unwrap_err()["code"], "invalid_input");

    // Turn 1: tools, one allowed and three refused in different places.
    ui.say("default", "u1", "What do the notes say? Also show me secret.txt.").unwrap();
    let full_log = ui.wait_turns("default", 1);
    let log = conversation(&full_log);
    assert_eq!(
        kinds(&log),
        [
            "user",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "assistant",
            "turn_complete"
        ],
        "{log:?}"
    );
    assert!(
        log[2]["error_code"].is_null() && log[2]["summary"].as_str().unwrap().contains("launch code is 42")
    );
    assert_eq!(log[4]["error_code"], "permission_denied", "outside the capability: refused by the kernel");
    assert_eq!(log[6]["error_code"], "invalid_input", "traversal: refused by the agent's own validation");
    assert_eq!(log[8]["error_code"], "invalid_input", "a tool that does not exist");
    assert_eq!(log[9]["text"], "The notes say 42. I was not allowed to read secret.txt.");
    assert!(!format!("{log:?}").contains("TOP SECRET"));

    // What a trajectory view gets: one report per model call, in order, placed before what it produced.
    let steps: Vec<&Value> = full_log.iter().filter(|r| r["kind"] == "model_call").collect();
    assert_eq!(steps.iter().map(|r| r["step"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(
        (&steps[0]["finish_reason"], &steps[1]["finish_reason"]),
        (&json!("toolcalls"), &json!("stop"))
    );
    assert_eq!(steps[0]["served_by"], MODEL);
    assert_eq!((&steps[0]["input_tokens"], &steps[0]["streamed"]), (&json!(20), &json!(false)));
    let position = |wanted: &Value| full_log.iter().position(|r| r == wanted).unwrap();
    let first_tool = full_log.iter().position(|r| r["kind"] == "tool_call").unwrap();
    let answer = full_log.iter().position(|r| r["kind"] == "assistant").unwrap();
    assert!(
        position(steps[0]) < first_tool && first_tool < position(steps[1]) && position(steps[1]) < answer
    );
    let access = world.scenario.access_log("data");
    assert_eq!(access.len(), 1, "only the permitted read reached the filesystem service: {access:?}");
    assert!(mentions(&access[0], "notes.txt"));
    assert_eq!(
        ui.say("default", "u1", "again").unwrap_err()["code"],
        "invalid_input",
        "a retried say cannot duplicate"
    );

    // What the model was told, denials included.
    let second = sent_messages(&world.server, 1);
    let tool_messages: Vec<&Value> = second.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_messages.len(), 4);
    assert!(tool_messages[1]["content"].as_str().unwrap().contains("permission_denied"));

    // Turn 2: the provider refuses; the user is told and the agent survives. Turn 3: context is kept.
    ui.say("default", "u2", "And now?").unwrap();
    let full_log = ui.wait_turns("default", 2);
    let log = conversation(&full_log);
    assert_eq!(kinds(&log[log.len() - 3..]), ["user", "error", "turn_complete"]);
    ui.say("default", "u3", "What did I ask first?").unwrap();
    let full_log = ui.wait_turns("default", 3);
    let log = conversation(&full_log);
    assert_eq!(log[log.len() - 2]["text"], "You asked about the notes earlier.");
    assert!(
        sent_messages(&world.server, 3)
            .iter()
            .any(|m| m["content"] == "What do the notes say? Also show me secret.txt.")
    );

    // Stopping the session ends the agent process, by its parent's hand; the conversation remains.
    let agent = ProcessOf::session(&mut ui, "default");
    ui.request(json!({"op": "sessions.stop", "session": "default"})).unwrap();
    assert!(wait_for(nucleus_e2e::TIMEOUT, || !world.supervisor.is_live(agent)));
    assert!(world.supervisor.is_live(ui_process));
    let listed = ui.request(json!({"op": "sessions.list"})).unwrap();
    assert_eq!(listed["sessions"][0]["live"], false);
    let stopped = conversation(&ui.history("default"));
    assert_eq!(&stopped[..log.len()], log, "the completed conversation is unchanged");
    assert_eq!(
        kinds(&stopped[log.len()..]),
        ["reminders_paused"],
        "Stop persists without a false interrupted turn"
    );

    // A message said while nobody is listening waits for the next agent of that session.
    world.server.script(Scripted::json(200, &completion(MODEL, "Back again.")));
    ui.request(json!({"op": "conversation.say", "session": "default", "message_id": "u4",
        "text": "Anyone there?"}))
        .unwrap();
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let full_log = ui.wait_turns("default", 4);
    let log = conversation(&full_log);
    assert_eq!(log[log.len() - 2]["text"], "Back again.");
    let prompt = sent_messages(&world.server, 4);
    let user: Vec<&str> =
        prompt.iter().filter(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap()).collect();
    let last = user.last().unwrap();
    assert!(last.starts_with("Anyone there?"), "{user:?}");
    // Turn 2 failed before any answer, so its question and turn 3's go as one message.
    assert_eq!(
        user[..user.len() - 1],
        ["What do the notes say? Also show me secret.txt.", "And now?\n\nWhat did I ask first?"],
        "a session's next agent continues its conversation"
    );
    // Every model call of the session, its next agent's too, names the session's project.
    let tags: Vec<_> =
        world.server.received().iter().map(|r| r.headers.get("x-proxy-tags").cloned()).collect();
    assert_eq!(tags, vec![Some("project=default".to_owned()); 5]);

    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
    let observable = format!("{:?}{:?}", world.scenario.events(), log);
    assert!(!observable.contains(KEY), "the key leaked");
}

/// A window long-polls instead of polling history: its `conversation.wait`
/// returns when the reply is recorded, and while it waits the UI service keeps
/// serving that window's other requests and a second window.
#[test]
fn a_waiting_window_is_woken_by_the_reply_and_blocks_no_one() {
    let mut world = boot("8");
    let (reply, held) = Scripted::json(200, &completion(MODEL, "Hello.")).held();
    world.server.script(reply);
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ui.say("default", "u1", "Hi").unwrap();
    // The agent takes the message at once, which the history shows; the window waits after that.
    let taken = |ui: &mut nucleus_e2e::UiClient| {
        ui.history("default").iter().find(|r| r["kind"] == "delivered").and_then(|r| r["seq"].as_u64())
    };
    assert!(wait_for(TIMEOUT, || taken(&mut ui).is_some()));
    let taken = taken(&mut ui).unwrap();

    let wait = ui.send(json!({"op": "conversation.wait", "session": "default", "after_seq": taken}));
    let mut phone = world.scenario.connect_ui("ui");
    assert_eq!(
        kinds(&conversation(&phone.history("default"))),
        ["user"],
        "served while the model is still answering"
    );
    let whoami = ui.send(json!({"op": "whoami"}));
    assert_eq!(ui.receive().0, Some(whoami), "the same window is answered before its wait");
    held.release();
    let (answered, woke) = ui.receive();
    let woke = woke.unwrap();
    assert_eq!((answered, &woke["changed"]), (Some(wait), &json!(true)));
    assert!(woke["last_seq"].as_u64().unwrap() > taken);
    assert!(conversation(&ui.wait_turns("default", 1)).iter().any(|r| r["text"] == "Hello."));

    drop((ui, phone));
    assert!(world.supervisor.shutdown().is_clean());
}

/// An organisation's VM: each window's connection carries its member's
/// account, which the conversation records as the author of what it says (no
/// nickname) and a page cannot choose; once two members have written, the
/// agent's prompt names each message's author's account, and its
/// instructions say the VM is shared.
#[test]
fn members_speak_under_their_own_names_and_the_agent_tells_them_apart() {
    let mut world = boot_options("8", false, &["--organisation"], false);
    world.server.script(Scripted::json(200, &completion(MODEL, "Hello Ada.")));
    world.server.script(Scripted::json(200, &completion(MODEL, "Hello Grace.")));
    world.supervisor.boot(program("members-ui"), ui_grants(&[], true)).unwrap();
    let mut ada = world.scenario.connect_ui_as("members-ui", Some("Ada"));
    let mut grace = world.scenario.connect_ui_as("members-ui", Some("Grace Hopper"));
    ada.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ada.say("default", "u1", "Hi, I am Ada.").unwrap();
    ada.wait_turns("default", 1);
    grace.say("default", "u2", "And I am Grace.").unwrap();
    let history = conversation(&grace.wait_turns("default", 2));
    let authors: Vec<_> =
        history.iter().filter(|r| r["kind"] == "user").map(|r| r["author"].clone()).collect();
    let (ada_account, grace_account) =
        (nucleus_e2e::account_of("Ada"), nucleus_e2e::account_of("Grace Hopper"));
    assert_eq!(authors, [json!(ada_account), json!(grace_account)]);
    let shown = serde_json::to_string(&history).unwrap();
    assert!(
        !shown.contains("Grace Hopper") && !shown.contains("\"Ada\""),
        "no nickname in a record: {shown}"
    );
    let first = sent_messages(&world.server, 0);
    assert!(first[0]["content"].as_str().unwrap().contains("shared by the members of an organisation"));
    assert_eq!(first.last().unwrap()["content"], "Hi, I am Ada.", "one person: no names");
    let second = sent_messages(&world.server, 1);
    assert_eq!(second.last().unwrap()["content"], format!("[from \"{grace_account}\"]\nAnd I am Grace."));
    drop((ada, grace));
    assert!(world.supervisor.shutdown().is_clean());
}

struct ProcessOf;
impl ProcessOf {
    fn session(ui: &mut nucleus_e2e::UiClient, name: &str) -> nucleus_protocol::ProcessId {
        let listed = ui.request(json!({"op": "sessions.list"})).unwrap();
        let session =
            listed["sessions"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap().clone();
        nucleus_protocol::ProcessId(session["process"].as_u64().unwrap())
    }
}

#[test]
fn a_front_end_is_not_above_other_services() {
    let mut world = boot("8");
    // One front end may manage sessions; the other may only talk in session `alpha`.
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let narrow =
        world.supervisor.boot(program("narrow-ui"), ui_grants(&["default", "alpha"], false)).unwrap();
    let mut full = world.scenario.connect_ui("ui");
    let mut limited = world.scenario.connect_ui("narrow-ui");

    for operation in [
        json!({"op": "sessions.create", "session": "alpha"}),
        json!({"op": "sessions.stop", "session": "alpha"}),
        json!({"op": "sessions.list"}),
    ] {
        assert_eq!(limited.request(operation).unwrap_err()["code"], "permission_denied");
    }
    assert_eq!(limited.say("beta", "u1", "hello").unwrap_err()["code"], "permission_denied");
    assert!(
        world.scenario.conversation_log("beta").is_empty(),
        "the kernel refused it; the service never saw it"
    );
    let refused_by_kernel = world.scenario.events().iter().any(|e| {
        matches!(e, SecurityEvent::Denied { process, reason: nucleus_protocol::ErrorCode::PermissionDenied, .. } if *process == narrow)
    });
    assert!(refused_by_kernel);

    // Within its grant it works like any other front end, and both see the same conversation.
    full.request(json!({"op": "sessions.create", "session": "alpha"})).unwrap();
    world.server.script(Scripted::json(200, &completion(MODEL, "Hello from alpha.")));
    limited.say("alpha", "u1", "hello").unwrap();
    let seen_by_full = conversation(&full.wait_turns("alpha", 1));
    assert_eq!(seen_by_full[1]["text"], "Hello from alpha.");
    assert_eq!(conversation(&limited.history("alpha")), seen_by_full);

    // Nothing in the UI service reaches past its fixed operations.
    for operation in [
        json!({"op": "kernel.kill", "process": 1}),
        json!({"op": "conversation.post", "session": "alpha", "entry": {"kind": "assistant", "text": "forged"}}),
    ] {
        assert_eq!(full.request(operation).unwrap_err()["code"], "invalid_input");
    }
    drop((full, limited));
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn a_runaway_model_is_stopped_by_the_step_limit() {
    let mut world = boot("3");
    for n in 0..5 {
        let id = format!("call_{n}");
        world.server.script(Scripted::json(
            200,
            &tool_request(MODEL, &[(id.as_str(), "fs_list", json!({"path": "work"}))]),
        ));
    }
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ui.say("default", "u1", "Loop forever.").unwrap();
    let full_log = ui.wait_turns("default", 1);
    let log = conversation(&full_log);
    assert_eq!(world.server.received().len(), 3, "exactly --max-steps model calls");
    assert!(log[log.len() - 2]["text"].as_str().unwrap().contains("step limit"));
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn an_answer_reaches_the_user_piece_by_piece() {
    let mut world = boot_with("8", true);
    let pieces = ["The ", "notes ", "say ", "**42**", "."];
    // Held after the first piece until the window has seen it.
    let gate = world.server.script_gated_stream(Scripted::stream(MODEL, &pieces, std::time::Duration::ZERO));
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    let created = ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let model_grants: Vec<String> = created["grants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["endpoint"] == "model")
        .map(|g| format!("{} max_calls={}", g["actions"], g["max_calls"]))
        .collect();
    // Neither is capped: a session's agent has no limit on its model calls.
    assert_eq!(
        model_grants,
        [r#"["complete","start"] max_calls=null"#, r#"["cancel","next"] max_calls=null"#]
    );

    ui.say("default", "u1", "What do the notes say?").unwrap();
    // What a window polling the history sees while the answer is being produced.
    let shown = |ui: &mut nucleus_e2e::UiClient| -> String {
        ui.history("default")
            .iter()
            .filter(|r| r["kind"] == "assistant_delta")
            .map(|r| r["text"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(wait_for(TIMEOUT, || shown(&mut ui) == pieces[0]), "the user saw the first piece");
    assert!(!ui.history("default").iter().any(|r| r["kind"] == "assistant"), "before all of it existed");
    gate.release();

    let full_log = ui.wait_turns("default", 1);
    let log = conversation(&full_log);
    assert_eq!(
        kinds(&log),
        ["user", "assistant", "turn_complete"],
        "once whole, the pieces are no longer served"
    );
    assert_eq!(log[1]["text"], pieces.concat());
    let on_disk = world.scenario.conversation_log("default");
    assert!(
        on_disk.iter().any(|r| r["kind"] == "assistant_delta"),
        "the log itself is append-only and keeps them"
    );

    // A second answer streams on the same handles.
    world.server.script(Scripted::stream(MODEL, &["Again."], std::time::Duration::from_millis(10)));
    ui.say("default", "u2", "Again?").unwrap();
    let full_log = ui.wait_turns("default", 2);
    let log = conversation(&full_log);
    assert_eq!(log[log.len() - 2]["text"], "Again.");

    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// The window's Runs views: `runs.*` operations reach the resident discovery
/// service only from a window holding the matching `discovery` action, on the
/// runs its grant covers; a window without it is refused before anything is
/// sent. No Lean and no run: the service answers `list` from memory and refuses
/// an unknown run itself.
#[test]
fn a_window_observes_discovery_runs_only_with_the_grant() {
    let mut scenario = Scenario::new();
    scenario.add_ui("ui");
    scenario.add_ui("narrow-ui");
    let service = DiscoveryService {
        tasks: scenario.host("tasks"),
        policies: scenario.host("policies"),
        prover_args: vec!["--model".to_owned(), MODEL.to_owned()],
        extra: vec!["--no-usd-ceiling".to_owned()],
    };
    scenario.add_discovery("discovery", &service);
    let mut supervisor = scenario.start();
    // The service holds `model`, `lean` and `explore` grants against endpoints
    // nobody provides here: it needs them only once a task is started.
    let discovery = supervisor.boot(program("discovery"), discovery_grants(4)).unwrap();
    let target = endpoint("discovery");
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == discovery)),
        "the service provides `discovery`"
    );

    // The four observations, nothing delegable.
    let mut runs = ui_grants(&[], true);
    runs.push(cap("discovery", &["list", "status", "problems", "tree"], &[]));
    runs.last_mut().unwrap().delegable = false;
    supervisor.boot(program("ui"), runs).unwrap();
    supervisor.boot(program("narrow-ui"), ui_grants(&[], true)).unwrap();
    let mut window = scenario.connect_ui("ui");
    let mut narrow = scenario.connect_ui("narrow-ui");

    // `whoami` is how a renderer decides what to show.
    let discovery_actions = |me: &Value| -> Option<Vec<String>> {
        me["handles"].as_array().unwrap().iter().find(|h| h["endpoint"] == "discovery").map(|h| {
            h["actions"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_owned()).collect()
        })
    };
    let me = window.request(json!({"op": "whoami"})).unwrap();
    let four: Vec<String> = ["list", "problems", "status", "tree"].map(String::from).to_vec();
    assert_eq!(discovery_actions(&me), Some(four));
    let handle = me["handles"].as_array().unwrap().iter().find(|h| h["endpoint"] == "discovery").unwrap();
    assert_eq!((&handle["delegable"], &handle["resource"]), (&json!(false), &json!([])));
    assert_eq!(discovery_actions(&narrow.request(json!({"op": "whoami"})).unwrap()), None);

    // The holder: an empty listing, and the service's own refusal of an unknown run.
    assert_eq!(window.request(json!({"op": "runs.list"})).unwrap(), json!({"runs": []}));
    let observations = [
        json!({"op": "runs.status", "run": "nope"}),
        json!({"op": "runs.problems", "run": "nope"}),
        json!({"op": "runs.tree", "run": "nope", "problem": "demo_a"}),
    ];
    for operation in &observations {
        assert_eq!(window.request(operation.clone()).unwrap_err()["code"], "invalid_input", "{operation}");
    }

    // Without the grant: refused by the UI service, and the service never sees a call.
    for operation in observations.iter().chain([&json!({"op": "runs.list"})]) {
        assert_eq!(
            narrow.request(operation.clone()).unwrap_err()["code"],
            "permission_denied",
            "{operation}"
        );
    }
    let reached_kernel = scenario.events().iter().any(|e| {
        matches!(e, SecurityEvent::Denied { reason: nucleus_protocol::ErrorCode::PermissionDenied, .. })
    });
    assert!(!reached_kernel, "the UI service refused before the kernel was asked: {:?}", scenario.events());

    // Stop is a valid operation that neither window holds.
    let stop = json!({"op": "runs.stop", "run": "nope"});
    assert_eq!(window.request(stop.clone()).unwrap_err()["code"], "permission_denied");
    assert_eq!(narrow.request(stop).unwrap_err()["code"], "permission_denied");
    // There is no generic service forwarding shortcut.
    let generic = json!({"op": "discovery.stop", "run": "nope"});
    assert_eq!(window.request(generic.clone()).unwrap_err()["code"], "invalid_input");
    assert_eq!(narrow.request(generic).unwrap_err()["code"], "invalid_input");

    drop((window, narrow));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

/// The sessions service runs no session itself: it starts another session's
/// agent while one session's agent waits for its model, gives none of them its
/// kernel authority, and its end ends them all.
#[test]
fn the_sessions_service_starts_a_session_while_another_waits_for_its_model() {
    let mut world = boot("8");
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    let parent = world.supervisor.provider_of(&endpoint("sessions")).unwrap().0;
    // The root session's model call is held until the child has answered.
    let (read, held) =
        Scripted::json(200, &tool_request(MODEL, &[("read", "fs_read", json!({"path": "work/notes.txt"}))]))
            .held();
    world.server.script_for("slow session's task", read);
    world.server.script_for("slow session's task", Scripted::json(200, &completion(MODEL, "Slow read 42.")));
    ui.request(json!({"op": "sessions.create", "session": "root"})).unwrap();
    ui.say("root", "parent-turn", "This is the slow session's task").unwrap();
    assert!(wait_for(TIMEOUT, || world
        .server
        .received()
        .iter()
        .any(|r| r.body.contains("slow session's task"))));
    let child = ui.request(json!({"op": "sessions.create", "session": "child"})).unwrap();
    assert!(world.scenario.children_of(parent).iter().any(|p| p.0 == child["process"].as_u64().unwrap()));
    world.server.script_for("child's task", Scripted::json(200, &completion(MODEL, "Child answered.")));
    ui.say("child", "child-turn", "This is the child's task").unwrap();
    ui.wait_turns("child", 1);
    held.release();
    let log = ui.wait_turns("root", 1);
    assert!(
        log.iter().any(|entry| entry["kind"] == "tool_result"
            && entry["summary"].as_str().is_some_and(|s| s.contains("42")))
    );
    let listed = ui.request(json!({"op": "sessions.list"})).unwrap();
    let grants =
        &listed["sessions"].as_array().unwrap().iter().find(|s| s["name"] == "child").unwrap()["grants"];
    assert!(!grants.as_array().unwrap().iter().any(|g| g["endpoint"] == "kernel"));
    world.supervisor.terminate(parent);
    assert!(world.supervisor.wait_until(TIMEOUT, |s| {
        !s.is_live(nucleus_protocol::ProcessId(child["process"].as_u64().unwrap()))
    }));
    // The independent conversation/UI services still answer history requests.
    assert!(!ui.history("child").is_empty());
    assert!(world.supervisor.shutdown().is_clean());
}
