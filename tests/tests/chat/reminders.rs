//! Reminders end to end: a session's agent sets and lists its own, another
//! process holding `wake` on the session wakes it, and one that comes due later
//! reaches a parked `receive` through the service's own timer. The source is
//! what the kernel checked, and the model sees a reminder inside its frame,
//! never as the person's words. A one-shot agent, which would never get a
//! reminder back, cannot set one.

use std::time::Duration;

use super::*;
use nucleus_conversation_interface::{REMINDER_FRAME_END, REMINDER_FRAME_START};
use nucleus_e2e::ok_value;

#[test]
fn a_session_is_woken_by_its_own_reminder_tools_and_by_another_process_holding_wake() {
    let mut world = boot("8");
    let (remind, list) = (json!({"after_ms": 60_000, "text": "check run gen2"}), json!({}));
    world.server.script(Scripted::json(
        200,
        &tool_request(
            MODEL,
            &[("r1", "conversation_remind", remind), ("r2", "conversation_reminders", list)],
        ),
    ));
    world.server.script(Scripted::json(200, &completion(MODEL, "I will look in a minute.")));
    world.server.script(Scripted::json(200, &completion(MODEL, "Run gen2 ended; I told you.")));
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let agent = ProcessOf::session(&mut ui, "default");

    ui.say("default", "u1", "Check on run gen2 in a minute.").unwrap();
    let log = ui.wait_turns("default", 1);
    let offered: Value = serde_json::from_str(&world.server.received()[0].body).unwrap();
    let names: Vec<&str> = offered["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    for name in ["conversation_remind", "conversation_reminders", "conversation_cancel_reminder"] {
        assert!(names.contains(&name), "{names:?}");
    }
    assert!(!names.contains(&"conversation_wake"), "{names:?}");
    let own = log.iter().find(|r| r["kind"] == "reminder_set").unwrap();
    assert_eq!(own["source"], json!({"action": "remind", "process": agent.0}));
    let listed = log.iter().find(|r| r["kind"] == "tool_result" && r["call_id"] == "r2").unwrap();
    assert!(listed["summary"].as_str().unwrap().contains("check run gen2"), "{listed}");

    // Another process, granted `wake` on this session alone, wakes the agent now.
    let waker = world
        .supervisor
        .boot(program("waker"), vec![cap("conversation", &["wake"], &["default", "default"])]);
    let waker = waker.unwrap();
    let log = ui.wait_turns("default", 2);
    let woke = log.iter().rev().find(|r| r["kind"] == "reminder_set").unwrap();
    assert_eq!(woke["source"], json!({"action": "wake", "process": waker.0}));
    let fired = log.iter().position(|r| r["kind"] == "reminder_fired" && r["id"] == woke["id"]).unwrap();
    assert_eq!(kinds(&conversation(&log[fired..])), ["reminder_fired", "assistant", "turn_complete"]);
    let prompt = sent_messages(&world.server, 2);
    let said = prompt.last().unwrap();
    assert_eq!(said["role"], "user");
    let text = said["content"].as_str().unwrap();
    assert!(text.starts_with(REMINDER_FRAME_START) && text.ends_with(REMINDER_FRAME_END), "{text}");
    assert!(
        text.contains(&format!("process {}", waker.0)) && text.contains("\n| run gen2 finished\n"),
        "{text}"
    );

    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// A completion queued while a tool runs must not restart work after the
/// person interrupts it. The replacement agent waits for the person's next
/// message, then receives the pending completion with that message.
#[test]
fn an_interrupted_session_waits_for_the_person_before_acting_on_a_pending_wake() {
    let mut world = boot_options("8", false, &[], true);
    world.scenario.mkfifo("data/work/pipe");
    let mut grants = ui_grants(&[], true);
    grants.push(CapSpec { delegable: false, ..cap("sessions", &["interrupt"], &[]) });
    world.supervisor.boot(program("ui"), grants).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let original = ProcessOf::session(&mut ui, "default");

    // The independent filesystem service has accepted this mutation, but
    // opening the FIFO blocks until the test reads it.
    let append = json!({"path": "work/pipe", "content": "appended once"});
    world.server.script(Scripted::json(200, &tool_request(MODEL, &[("append", "fs_append", append)])));
    ui.say("default", "u1", "Append to the pipe.").unwrap();
    world.scenario.wait_access("data", "pipe");
    world
        .supervisor
        .boot(program("waker"), vec![cap("conversation", &["wake"], &["default", "default"])])
        .unwrap();
    assert!(world.scenario.wait_done("waker").iter().all(|line| ok_value(line).is_some()));
    let queued = ui.history("default");
    let reminder = queued.iter().find(|r| r["kind"] == "reminder_set").unwrap()["id"].clone();
    assert!(!queued.iter().any(|r| r["kind"] == "reminder_fired"), "the tool is still running");

    ui.request(json!({"op": "sessions.interrupt", "session": "default"})).unwrap();
    let replacement = ProcessOf::session(&mut ui, "default");
    assert_ne!(replacement, original, "interrupt starts a new agent");
    assert!(world.supervisor.is_live(replacement));
    assert!(!world.supervisor.is_live(original));
    assert_eq!(ui.wait_turns("default", 1).last().unwrap()["kind"], "interrupted");
    // The dispatched mutation can still finish, but it must not be retried.
    assert_eq!(std::fs::read(world.scenario.host("data/work/pipe")).unwrap(), b"appended once");
    assert!(
        !wait_for(Duration::from_secs(1), || world.server.received().len() > 1),
        "a queued completion must not call the model after Stop"
    );
    assert!(
        !ui.history("default").iter().any(|r| r["kind"] == "reminder_fired"),
        "the completion remains pending until the person continues"
    );

    world
        .server
        .script(Scripted::json(200, &completion(MODEL, "Run gen2 finished; I will check the append.")));
    ui.say("default", "continue", "Continue the unfinished task.").unwrap();
    let resumed = ui.wait_turns("default", 2);
    assert_eq!(world.server.received().len(), 2, "only the explicit continuation calls the model");
    let delivered = resumed.iter().position(|r| r["kind"] == "delivered" && r["id"] == "continue").unwrap();
    let fired = resumed.iter().position(|r| r["kind"] == "reminder_fired" && r["id"] == reminder).unwrap();
    assert!(delivered < fired, "the completion is delivered with the person's new message: {resumed:?}");
    let prompt = sent_messages(&world.server, 1);
    let text = prompt.last().unwrap()["content"].as_str().unwrap();
    assert!(text.contains("Continue the unfinished task.") && text.contains("run gen2 finished"), "{text}");
    assert!(text.contains(REMINDER_FRAME_START) && text.contains(REMINDER_FRAME_END), "{text}");
    assert!(
        prompt.iter().any(|m| m["tool_call_id"] == "append"
            && m["content"].as_str().is_some_and(|text| text.contains("unknown_outcome"))),
        "the interrupted tool's outcome stays unknown: {prompt:?}"
    );
    assert_eq!(
        world.scenario.access_log("data").iter().filter(|entry| mentions(entry, "pipe")).count(),
        1,
        "nothing repeats the stopped mutation"
    );
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// Only the service's timer can answer this `receive`: the reminder is not
/// due when it parks, and nothing else calls the service meanwhile, since any
/// call on the session would deliver a due reminder too. Without the run loop
/// sleeping until then and firing it, the `receive` times out.
#[test]
fn a_reminder_that_comes_due_later_reaches_a_parked_receive_by_the_services_timer() {
    let mut scenario = Scenario::new();
    // A reminder is set only on a session that exists.
    scenario.write("sessions/default/later/log.jsonl", "");
    scenario.add_conversation("conversation");
    let call = |action: &str, payload: Value| {
        json!({"tool": "call", "endpoint": "conversation", "action": action, "path": ["default", "later"],
               "payload": payload, "deadline_ms": 10_000})
    };
    let calls = [call("wake", json!({"after_ms": 1_500, "text": "look again"})), call("receive", json!({}))];
    scenario.add_agent("sleeper", &json!({"turns": [{"tool_calls": calls}]}), None);
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let grants = vec![cap("conversation", &["wake", "receive"], &["default", "later"])];
    let sleeper = supervisor.boot(program("sleeper"), grants).unwrap();
    let replies = scenario.wait_done_within("sleeper", Duration::from_secs(30));
    let due = ok_value(&replies[0]).unwrap()["due_at_ms"].as_u64().unwrap();
    let received = ok_value(&replies[1]).unwrap_or_else(|| panic!("never delivered: {}", replies[1]));
    assert_eq!(received["reminders"][0]["text"], "look again");
    assert_eq!(received["reminders"][0]["source"], json!({"action": "wake", "process": sleeper.0}));
    let log = scenario.conversation_log("later");
    let fired = log.iter().find(|r| r["kind"] == "reminder_fired").unwrap();
    assert!(fired["at_ms"].as_u64().unwrap() >= due, "not before it was due: {log:?}");
    assert!(supervisor.shutdown().is_clean());
}

/// A one-shot agent never calls `receive`, so it is offered no reminder
/// tool, and a call to one is refused as a tool it does not have, although
/// it holds `remind` on its session.
#[test]
fn a_one_shot_agent_cannot_set_a_reminder_it_was_not_offered() {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    // The session exists, so the service would set the reminder: only the agent refuses it.
    scenario.write("sessions/default/default/log.jsonl", "");
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("task", MODEL, &["--task", "Check on run gen2 in a minute."]);
    let remind = json!({"after_ms": 60_000, "text": "check run gen2"});
    server.script(Scripted::json(200, &tool_request(MODEL, &[("r1", "conversation_remind", remind)])));
    server.script(Scripted::json(200, &completion(MODEL, "I cannot set a reminder.")));
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let grants = vec![
        cap("conversation", &["post", "remind"], &["default", "default"]),
        cap("model", &["complete"], &["deepseek"]),
    ];
    let task = supervisor.boot(program("task"), grants).unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(task)));
    let offered: Value = serde_json::from_str(&server.received()[0].body).unwrap();
    let names: Vec<&str> = offered["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(names.iter().all(|name| !name.starts_with("conversation_")), "{names:?}");
    let log = scenario.conversation_log("default");
    let refused = log.iter().find(|r| r["kind"] == "tool_result" && r["call_id"] == "r1").unwrap();
    assert_eq!(refused["error_code"], "invalid_input", "{refused}");
    assert!(log.iter().all(|r| r["kind"] != "reminder_set"), "{log:?}");
    assert!(supervisor.shutdown().is_clean());
}
