//! Stopping or interrupting a session ends its agent, wherever it is in its
//! turn (the sessions service runs no session itself). Stop must preserve the
//! service and unrelated sessions, run nothing of the stopped turn afterwards,
//! and let the session's next agent start clean.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use nucleus_e2e::http::{FakeHttp, Scripted, StreamGate, completion, tool_request};
use nucleus_e2e::{Scenario, TIMEOUT, UiClient, cap, endpoint, ok_value, program, wait_for};
use nucleus_kernel::CapSpec;
use nucleus_platform::Supervisor;
use nucleus_protocol::ProcessId;
use serde_json::{Value, json};

const MODEL: &str = "fixture/stop";

struct World {
    supervisor: Supervisor,
    scenario: Scenario,
    server: FakeHttp,
    parent: ProcessId,
    streaming: bool,
}

fn boot(streaming: bool) -> World {
    boot_with_child_args(streaming, &[])
}

fn boot_with_child_args(streaming: bool, child_args: &[&str]) -> World {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.write("data/work/retained.txt", "keep this workspace");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_conversation("conversation");
    scenario.add_openrouter("model", &server.base_url(), "synthetic-stop-key", &["--retries", "0"], true);
    scenario.add_chat_agent("chat", MODEL, child_args);
    scenario.add_sessions("parent", "chat");
    scenario.add_ui("ui");
    // A process outside the sessions, when a test boots it: a reminder in each, due in an hour.
    let wake = |session: &str| {
        json!({"tool": "call", "endpoint": "conversation", "action": "wake", "path": ["default", session],
               "payload": {"after_ms": 3_600_000, "text": "check the run"}})
    };
    scenario.add_agent("waker", &json!({"turns": [{"tool_calls": [wake("default"), wake("other")]}]}), None);
    let config = scenario.config(|limits| limits.max_inflight_per_endpoint = 32, 64);
    let supervisor = scenario.start_with(config).unwrap();
    scenario.boot_service(&supervisor, "fs", "fs");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    scenario.boot_service(&supervisor, "model", "model");
    let mut clear = cap("conversation", &["clear_reminders", "list"], &[]);
    clear.delegable = false;
    let mut grants: Vec<CapSpec> = vec![
        cap("kernel", &["provide"], &["endpoint", "sessions"]),
        nucleus_e2e::session_spawn(),
        cap("conversation", &["receive", "post", "history"], &[]),
        clear,
        cap("model", &["complete", "start"], &["fixture"]),
        cap("fs", &["read", "list", "create", "append"], &["work"]),
    ];
    if streaming {
        grants.push(cap("model", &["next", "cancel"], &["fixture"]));
    }
    let parent = supervisor.boot(program("parent"), grants).unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |kernel| kernel.provider_of(&endpoint("sessions")).is_some()));
    supervisor
        .boot(
            program("ui"),
            vec![
                cap("conversation", &["say", "history"], &[]),
                cap("sessions", &["list", "create", "stop", "interrupt"], &[]),
            ],
        )
        .unwrap();
    // The window's first session, as a page creates it (not for one-shot agents,
    // which would start their task at once).
    if child_args.is_empty() {
        scenario.connect_ui("ui").request(json!({"op":"sessions.create", "session":"default"})).unwrap();
    }
    World { supervisor, scenario, server, parent, streaming }
}

fn response(world: &World, text: &str) -> Scripted {
    if world.streaming {
        Scripted::stream(MODEL, &[text], Duration::from_millis(10))
    } else {
        Scripted::json(200, &completion(MODEL, text))
    }
}

fn session(ui: &mut UiClient, name: &str) -> Value {
    ui.request(json!({"op":"sessions.list"})).unwrap()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == name)
        .unwrap()
        .clone()
}

fn stop(ui: &mut UiClient, name: &str) {
    ui.request(json!({"op":"sessions.stop", "session":name})).unwrap();
    assert_eq!(session(ui, name)["live"], false);
}

fn answer(world: &World, ui: &mut UiClient, name: &str, id: &str, text: &str, turns: usize) {
    world.server.script(response(world, text));
    ui.say(name, id, text).unwrap();
    let history = ui.wait_turns(name, turns);
    assert!(
        history.iter().any(|record| record["kind"] == "assistant" && record["text"] == text),
        "{history:?}"
    );
}

/// Stop withdraws a session's pending reminders as the person's, keeps the
/// other sessions and the service, and what is said meanwhile waits for the
/// session's next agent, a new process.
#[test]
fn a_stopped_session_s_next_agent_answers_what_was_said_meanwhile() {
    {
        let mut world = boot(false);
        let mut ui = world.scenario.connect_ui("ui");
        let own = session(&mut ui, "default");
        let child = ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();
        // Each session has a reminder pending, which its Stop withdraws as the person's.
        let log = |name: &str| world.scenario.host(&format!("sessions/default/{name}/log.jsonl"));
        assert!(wait_for(TIMEOUT, || log("default").exists() && log("other").exists()));
        world.supervisor.boot(program("waker"), vec![cap("conversation", &["wake"], &[])]).unwrap();
        assert!(world.scenario.wait_done("waker").iter().all(|line| ok_value(line).is_some()));
        let cleared = |ui: &mut UiClient, name: &str| {
            let history = ui.history(name);
            let by_stop = |r: &Value| r["action"] == "clear_reminders" && r["process"] == world.parent.0;
            assert!(history.iter().any(|r| r["kind"] == "reminder_cancelled" && by_stop(r)), "{history:?}");
        };
        stop(&mut ui, "default");
        cleared(&mut ui, "default");
        assert!(world.supervisor.is_live(world.parent));
        assert_eq!(session(&mut ui, "other"), child);
        ui.say("default", "queued", "Deliver only after resuming.").unwrap();
        answer(&world, &mut ui, "other", "other-1", "Independent child answer.", 1);
        assert_eq!(world.server.received().len(), 1);
        assert!(!ui.history("default").iter().any(|record| record["kind"] == "assistant"));
        world.server.script(Scripted::json(200, &completion(MODEL, "Resumed queued input.")));
        let resumed = ui.request(json!({"op":"sessions.create", "session":"default"})).unwrap();
        assert_ne!(resumed["process"], own["process"], "a stopped session's next agent is a new process");
        // Stopping while idle closed no turn; the queued message is the one turn.
        let history = ui.wait_turns("default", 1);
        assert!(history.iter().any(|record| record["text"] == "Resumed queued input."));
        assert_eq!(world.server.received().len(), 2);
        stop(&mut ui, "other");
        cleared(&mut ui, "other");
        stop(&mut ui, "other");
        let replacement = ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();
        assert_ne!(replacement["process"], child["process"]);
        assert!(world.supervisor.is_live(world.parent));
        assert_eq!(
            std::fs::read_to_string(world.scenario.host("data/work/retained.txt")).unwrap(),
            "keep this workspace"
        );
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn stopping_a_named_session_after_its_confirmed_natural_exit_is_idempotent() {
    {
        let mut world = boot_with_child_args(false, &["--task", "Finish this one-shot session."]);
        let mut ui = world.scenario.connect_ui("ui");
        world.server.script(response(&world, "The one-shot task is complete."));
        let child = ui.request(json!({"op":"sessions.create", "session":"one-shot"})).unwrap();
        let process = ProcessId(child["process"].as_u64().unwrap());
        // Wait for the actual owned child to finish and be reaped. No timing
        // guess or external kill stands in for this natural-exit boundary.
        assert!(world.supervisor.wait_until(TIMEOUT, |supervisor| supervisor.cleanup_confirmed(process)));
        assert!(!world.supervisor.is_live(process));
        assert_eq!(world.server.received().len(), 1);
        stop(&mut ui, "one-shot");
        stop(&mut ui, "one-shot");
        assert_eq!(session(&mut ui, "one-shot")["process"], child["process"]);
        assert!(ui.request(json!({"op":"sessions.stop", "session":"unknown"})).is_err());
        assert!(world.supervisor.is_live(world.parent));
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn stopping_a_session_ends_its_model_call_streaming_or_not() {
    {
        for streaming in [false, true] {
            let mut world = boot(streaming);
            let mut ui = world.scenario.connect_ui("ui");
            let own = session(&mut ui, "default");
            let child = ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();
            // The reply is held until the test ends: Stop never waits for it.
            let _held = if streaming {
                world.server.script_gated_stream(Scripted::events(vec![
                    json!({"model":MODEL,"choices":[{"delta":{"content":"Visible before Stop."}}]}).to_string(),
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"late-call","function":{"name":"fs_create","arguments":"{\"path\":\"work/forbidden.txt\",\"content\":\"must not execute after Stop\"}"}}]},"finish_reason":"tool_calls"}]}).to_string(),
                    "[DONE]".into(),
                ], Duration::ZERO))
            } else {
                let late = tool_request(
                    MODEL,
                    &[(
                        "late-call",
                        "fs_create",
                        json!({"path":"work/forbidden.txt", "content":"must not execute after Stop"}),
                    )],
                );
                let (script, held) = Scripted::json(200, &late).held();
                world.server.script(script);
                held
            };
            ui.say("default", "inflight", "Wait for a model response.").unwrap();
            assert!(wait_for(TIMEOUT, || !world.server.received().is_empty()));
            if streaming {
                assert!(wait_for(TIMEOUT, || ui
                    .history("default")
                    .iter()
                    .any(|record| record["kind"] == "assistant_delta")));
            }
            stop(&mut ui, "default");
            let stopped = ui.wait_turns("default", 1);
            if streaming {
                assert!(
                    stopped.iter().any(|record| record["text"] == "Visible before Stop."),
                    "Stop preserves shown partial text"
                );
            }
            assert!(world.supervisor.is_live(world.parent));
            assert_eq!(session(&mut ui, "other"), child);
            answer(&world, &mut ui, "other", "other-1", "Other session survived.", 1);
            world.server.script(response(&world, "A fresh turn after Stop."));
            let next = ui.request(json!({"op":"sessions.create", "session":"default"})).unwrap();
            assert_ne!(next["process"], own["process"]);
            ui.say("default", "resumed", "Start a fresh turn.").unwrap();
            let resumed = ui.wait_turns("default", 2);
            assert!(resumed.iter().any(|record| record["text"] == "A fresh turn after Stop."));
            assert!(
                !resumed
                    .iter()
                    .any(|record| record["kind"] == "tool_call" && record["call_id"] == "late-call")
            );
            assert!(!world.scenario.host("data/work/forbidden.txt").exists());
            assert_eq!(world.server.received().len(), 3, "Stop must not retry an interrupted model request");
            assert!(!world.server.received()[2].body.contains("late-call"), "nothing of the stopped turn");
            drop(ui);
            assert!(world.supervisor.shutdown().is_clean());
        }
    }
}

/// Stop while a mutation is dispatched: the rest of the batch never runs, the
/// service may still finish the dispatched one (its outcome is unknown, as
/// the turn records no result for it), and the next agent replays nothing.
#[test]
fn stop_during_a_dispatched_mutation_never_runs_the_rest_of_the_batch() {
    {
        let mut world = boot(false);
        world.scenario.mkfifo("data/work/pipe");
        let mut ui = world.scenario.connect_ui("ui");
        world.server.script(Scripted::json(
            200,
            &tool_request(
                MODEL,
                &[
                    ("inflight-append", "fs_append", json!({"path":"work/pipe", "content":"written once"})),
                    (
                        "not-dispatched",
                        "fs_create",
                        json!({"path":"work/forbidden.txt", "content":"must not execute"}),
                    ),
                ],
            ),
        ));
        ui.say("default", "tool-wait", "Append then create a file.").unwrap();
        // Opening the FIFO for append blocks until a reader exists. The real
        // service has logged dispatch but cannot yet acknowledge its effect.
        world.scenario.wait_access("data", "pipe");
        stop(&mut ui, "default");
        let history = ui.wait_turns("default", 1);
        assert!(
            !history.iter().any(|r| r["kind"] == "tool_result" && r["call_id"] == "inflight-append"),
            "no outcome is claimed for a dispatched mutation: {history:?}"
        );
        assert_eq!(history.last().unwrap()["kind"], "interrupted");
        // The independent service may still finish the original operation.
        assert_eq!(std::fs::read(world.scenario.host("data/work/pipe")).unwrap(), b"written once");
        assert!(!world.scenario.host("data/work/forbidden.txt").exists());
        world.server.script(Scripted::json(200, &completion(MODEL, "I will not retry uncertain effects.")));
        ui.request(json!({"op":"sessions.create", "session":"default"})).unwrap();
        ui.say("default", "resume-tool", "Explain the prior outcome.").unwrap();
        let resumed = ui.wait_turns("default", 2);
        assert!(resumed.iter().any(|record| record["text"] == "I will not retry uncertain effects."));
        assert!(!world.scenario.host("data/work/forbidden.txt").exists());
        assert_eq!(
            world.scenario.access_log("data").iter().filter(|e| nucleus_e2e::mentions(e, "pipe")).count(),
            1
        );
        assert_eq!(world.server.received().len(), 2);
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

fn kinds(ui: &mut UiClient, name: &str) -> Vec<String> {
    ui.history(name).iter().map(|record| record["kind"].as_str().unwrap_or_default().to_owned()).collect()
}

/// A reply held until the test ends: the turn is still running when it is stopped.
fn slow(world: &World, text: &str) -> StreamGate {
    let (script, held) = response(world, text).held();
    world.server.script(script);
    held
}

/// What the person sent while the agent worked, as the model was asked.
fn last_prompt(world: &World) -> String {
    let request: Value = serde_json::from_str(&world.server.received().last().unwrap().body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    messages.iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap().to_owned()
}

#[test]
fn stop_and_interrupt_close_the_turn_and_the_queue_is_answered_as_one() {
    let mut world = boot(false);
    let mut ui = world.scenario.connect_ui("ui");
    let child = ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();

    // Stop mid-turn: the turn is closed, and a started agent does not take it up again.
    let _held = slow(&world, "Never shown.");
    ui.say("other", "s1", "A long question.").unwrap();
    assert!(wait_for(TIMEOUT, || world.server.received().len() == 1));
    stop(&mut ui, "other");
    assert_eq!(kinds(&mut ui, "other"), ["user", "delivered", "interrupted"]);
    let restarted = ui.request(json!({"op":"sessions.create", "session":"other"})).unwrap();
    assert_ne!(restarted["process"], child["process"]);
    assert_eq!(world.server.received().len(), 1, "a stopped turn is not resumed");

    // Interrupt mid-turn with two messages queued: the agent restarts on the same model and
    // answers both in one turn.
    let _held_too = slow(&world, "Never shown either.");
    world.server.script(response(&world, "Both queued messages answered."));
    ui.say("other", "i1", "Another long question.").unwrap();
    assert!(wait_for(TIMEOUT, || world.server.received().len() == 2));
    ui.say("other", "q1", "Actually, first this.").unwrap();
    ui.say("other", "q2", "And this.").unwrap();
    ui.request(json!({"op":"sessions.interrupt", "session":"other"})).unwrap();
    let after = session(&mut ui, "other");
    assert_eq!(after["live"], true);
    assert_eq!(after["model"], restarted["model"]);
    // Stopped, interrupted, then the queue's turn.
    let history = ui.wait_turns("other", 3);
    assert!(history.iter().any(|r| r["text"] == "Both queued messages answered."), "{history:?}");
    assert_eq!(world.server.received().len(), 3, "one turn for the whole queue");
    // After the note that the turn before was stopped, in the same message (the adapter joins
    // successive user messages).
    assert!(last_prompt(&world).ends_with("is unknown.]\n\nActually, first this.\n\nAnd this."));
    assert_eq!(kinds(&mut ui, "other").iter().filter(|k| *k == "interrupted").count(), 2);

    // An idle Stop persists the reminder pause without marking the completed
    // turn interrupted or changing its existing records.
    let before = ui.history("other");
    ui.request(json!({"op":"sessions.interrupt", "session":"other"})).unwrap();
    let after = ui.history("other");
    assert_eq!(&after[..before.len()], before);
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after.last().unwrap()["kind"], "reminders_paused");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
