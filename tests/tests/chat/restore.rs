//! A session's agent continues its conversation whatever ended the agent before
//! it: real processes, a scripted model, and each next model request checked
//! for what the model saw before the restart.

use super::*;

/// The messages of a model request that make or answer tool call `id`.
fn about_call<'a>(messages: &'a [Value], id: &str) -> Vec<&'a Value> {
    messages.iter().filter(|m| m["tool_call_id"] == id || m["tool_calls"][0]["id"] == id).collect()
}

/// Interrupted while a tool call runs, the next agent's first request holds
/// the conversation: the earlier message, the call that completed with its
/// result, the interrupted call with a result saying it did not return, and
/// that the turn was stopped. Nothing runs again. Summarised later, the
/// summary is where the agent after the next restart starts.
#[test]
fn an_interrupted_or_restarted_agent_continues_its_conversation() {
    let mut world = boot_options("8", false, &["--compact-at-tokens", "100000"], true);
    world.scenario.mkfifo("data/work/pipe");
    let mut grants = ui_grants(&[], true);
    grants.push(CapSpec { delegable: false, ..cap("sessions", &["interrupt"], &[]) });
    world.supervisor.boot(program("ui"), grants).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();

    // A tool call that completes.
    world.server.script(Scripted::json(
        200,
        &tool_request(MODEL, &[("read-notes", "fs_read", json!({"path": "work/notes.txt"}))]),
    ));
    world.server.script(Scripted::json(200, &completion(MODEL, "The notes say 42.")));
    ui.say("default", "u1", "What do the notes say?").unwrap();
    ui.wait_turns("default", 1);

    // One that is still running (an append blocked on a pipe) when the person stops the turn.
    let append = json!({"path": "work/pipe", "content": "appended once"});
    world.server.script(Scripted::json(200, &tool_request(MODEL, &[("append-pipe", "fs_append", append)])));
    ui.say("default", "u2", "Append to the pipe.").unwrap();
    world.scenario.wait_access("data", "pipe");
    ui.request(json!({"op": "sessions.interrupt", "session": "default"})).unwrap();
    assert_eq!(ui.wait_turns("default", 2).last().unwrap()["kind"], "interrupted");
    assert_eq!(std::fs::read(world.scenario.host("data/work/pipe")).unwrap(), b"appended once");

    // The next message: its request holds what the model saw before.
    let mut reply = completion(MODEL, "The append was interrupted; I will check it before trying again.");
    reply["usage"]["prompt_tokens"] = json!(200_000); // so the next turn is summarised
    world.server.script(Scripted::json(200, &reply));
    ui.say("default", "u3", "What happened to the append?").unwrap();
    ui.wait_turns("default", 3);
    let sent = sent_messages(&world.server, 3);
    assert!(
        sent.iter().any(|m| m["role"] == "user" && m["content"] == "What do the notes say?"),
        "{sent:#?}"
    );
    let read = about_call(&sent, "read-notes");
    assert_eq!(read.len(), 2, "{sent:#?}");
    assert!(read[1]["content"].as_str().unwrap().contains("the launch code is 42"), "{read:#?}");
    let appended = about_call(&sent, "append-pipe");
    assert_eq!(appended.len(), 2, "{sent:#?}");
    assert!(appended[1]["content"].as_str().unwrap().contains("unknown_outcome"), "{appended:#?}");
    // The note, then the message, as one (the adapter joins successive user messages).
    let last = sent.last().unwrap()["content"].as_str().unwrap();
    assert!(last.starts_with("[The turn above was stopped at "), "{sent:#?}");
    assert!(last.ends_with("whether it took effect is unknown.]\n\nWhat happened to the append?"), "{last}");
    let appends =
        world.scenario.access_log("data").iter().filter(|e| nucleus_e2e::mentions(e, "pipe")).count();
    assert_eq!(appends, 1, "nothing ran again");

    // Summarised, then stopped and started again: the summary is where it starts.
    world.server.script(Scripted::json(200, &completion(MODEL, "SUMMARY-OF-WORK: the notes say 42.")));
    world.server.script(Scripted::json(200, &completion(MODEL, "Fourth answer.")));
    ui.say("default", "u4", "Fourth question.").unwrap();
    let log = ui.wait_turns("default", 4);
    let summary = log.iter().find(|r| r["kind"] == "compacted").map(|r| r["summary"].clone());
    assert_eq!(summary, Some(json!("SUMMARY-OF-WORK: the notes say 42.")), "{log:?}");
    ui.request(json!({"op": "sessions.stop", "session": "default"})).unwrap();
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    world.server.script(Scripted::json(200, &completion(MODEL, "Fifth answer.")));
    ui.say("default", "u5", "Fifth question.").unwrap();
    ui.wait_turns("default", 5);
    assert_eq!(world.server.received().len(), 7, "no summary was asked for again");
    let sent = sent_messages(&world.server, 6);
    let first = sent.iter().find(|m| m["role"] != "system").unwrap();
    assert!(first["content"].as_str().unwrap().contains("SUMMARY-OF-WORK"), "{sent:#?}");
    assert!(!sent.iter().any(|m| m["content"] == "What do the notes say?"), "summarised away: {sent:#?}");
    assert!(sent.iter().any(|m| m["role"] == "assistant" && m["content"] == "Fourth answer."));
    assert_eq!(sent.last().unwrap()["content"], "Fifth question.");

    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
