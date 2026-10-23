//! A turn ends when the model replies without calling a tool, and that
//! reply's text is the answer. These fixtures drive the real chat loop and
//! services with a scripted model that simply stops calling tools.

use super::*;

fn session(world: &World) -> nucleus_e2e::UiClient {
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op":"sessions.create","session":"default"})).unwrap();
    ui
}

fn read(id: &str) -> Scripted {
    Scripted::json(200, &tool_request(MODEL, &[(id, "fs_read", json!({"path":"work/notes.txt"}))]))
}

fn says(text: &str) -> Scripted {
    Scripted::json(200, &completion(MODEL, text))
}

fn errors(log: &[Value]) -> Vec<&str> {
    log.iter().filter(|v| v["kind"] == "error").map(|v| v["text"].as_str().unwrap()).collect()
}

fn assistant(log: &[Value]) -> Vec<&str> {
    log.iter().filter(|v| v["kind"] == "assistant").map(|v| v["text"].as_str().unwrap()).collect()
}

/// After tool work the model answers in text: the turn ends there, with no
/// further model call, nothing told to the model and no error. What the
/// model would have done had it been asked again (more tool calls, then
/// text again) belongs to the next message.
#[test]
fn a_text_answer_after_tool_work_ends_the_turn() {
    let mut world = boot("8");
    for reply in [read("first"), says("The notes say 42."), read("more"), says("Nothing to add.")] {
        world.server.script(reply);
    }
    let mut ui = session(&world);
    ui.say("default", "u1", "What do the notes say?").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(errors(&log).is_empty(), "{log:?}");
    assert_eq!(assistant(&log), ["The notes say 42."]);
    assert_eq!(world.server.received().len(), 2, "the answer ended the turn");
    assert_eq!(world.scenario.access_log("data").len(), 1, "no tool ran after the answer");

    ui.say("default", "u2", "Anything else?").unwrap();
    let log = ui.wait_turns("default", 2);
    assert!(errors(&log).is_empty(), "{log:?}");
    assert_eq!(assistant(&log), ["The notes say 42.", "Nothing to add."]);
    assert_eq!(world.server.received().len(), 4);
    let users: Vec<Value> =
        sent_messages(&world.server, 3).into_iter().filter(|m| m["role"] == "user").collect();
    assert_eq!(users.len(), 2, "the model read the person's two messages and nothing else: {users:?}");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// A reply with neither text nor a tool call after tool work is answered
/// once, plainly; the turn then ends with the next reply, empty or not.
#[test]
fn an_empty_reply_after_tool_work_is_nudged_once_and_then_the_turn_ends_as_it_is() {
    for (last, shown) in [("The notes say 42.", vec!["The notes say 42."]), ("", vec![])] {
        let mut world = boot("8");
        for reply in [read("first"), says(""), says(last)] {
            world.server.script(reply);
        }
        let mut ui = session(&world);
        ui.say("default", "u1", "What do the notes say?").unwrap();
        let log = ui.wait_turns("default", 1);
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(assistant(&log), shown);
        assert_eq!(world.server.received().len(), 3, "one nudge, never a second");
        let nudged = sent_messages(&world.server, 2);
        let nudge = nudged.last().unwrap();
        assert_eq!(nudge["role"], "user");
        assert!(nudge["content"].as_str().unwrap().contains("Give the person your answer"), "{nudge}");
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}
