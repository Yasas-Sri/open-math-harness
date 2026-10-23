//! The message queue end to end: members move and withdraw messages waiting for
//! a session's agent through the labelled UI service, the records name who did
//! it to whose message, a window whose grants do not reach the session cannot
//! touch its queue, and the agent takes the queue whole in its new order, never
//! the withdrawn message.

use super::*;

fn queued(ui: &mut nucleus_e2e::UiClient) -> Vec<String> {
    let listed = ui.request(json!({"op": "conversation.queue", "session": "default"})).unwrap();
    listed["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn members_move_and_withdraw_waiting_messages_and_the_agent_takes_the_new_order() {
    let mut world = boot_options("8", false, &["--organisation"], false);
    world.server.script(Scripted::json(200, &completion(MODEL, "Done.")));
    let queue_actions = |scope: &[&str]| CapSpec {
        delegable: false,
        ..cap("conversation", &["queue", "move", "withdraw"], scope)
    };
    let mut grants = ui_grants(&[], true);
    grants.push(queue_actions(&[]));
    world.supervisor.boot(program("members-ui"), grants).unwrap();
    let mut other = ui_grants(&["default", "alpha"], false);
    other.push(queue_actions(&["default", "alpha"]));
    world.supervisor.boot(program("narrow-ui"), other).unwrap();
    let mut ada = world.scenario.connect_ui_as("members-ui", Some("Ada"));
    let mut grace = world.scenario.connect_ui_as("members-ui", Some("Grace Hopper"));
    let mut elsewhere = world.scenario.connect_ui("narrow-ui");

    // No agent runs yet, so what they say waits.
    ada.say("default", "u1", "first").unwrap();
    grace.say("default", "u2", "second").unwrap();
    ada.say("default", "u3", "third").unwrap();
    assert_eq!(queued(&mut ada), ["u1", "u2", "u3"]);
    grace
        .request(json!({"op": "conversation.move", "session": "default", "message_id": "u3", "before": "u1"}))
        .unwrap();
    ada.request(json!({"op": "conversation.withdraw", "session": "default", "message_id": "u2"})).unwrap();
    assert_eq!(queued(&mut grace), ["u3", "u1"]);
    let history = ada.history("default");
    let moved = history.iter().find(|r| r["kind"] == "message_moved").unwrap();
    let (ada_account, grace_account) =
        (nucleus_e2e::account_of("Ada"), nucleus_e2e::account_of("Grace Hopper"));
    assert_eq!((&moved["order"], &moved["author"]), (&json!(["u3", "u1", "u2"]), &json!(grace_account)));
    let withdrawn = history.iter().find(|r| r["kind"] == "message_withdrawn").unwrap();
    assert_eq!(
        (&withdrawn["id"], &withdrawn["message_author"], &withdrawn["author"]),
        (&json!("u2"), &json!(grace_account), &json!(ada_account)),
        "Ada removed Grace's message"
    );
    // A window whose grants reach another session only is refused by the kernel.
    let refused = elsewhere
        .request(json!({"op": "conversation.withdraw", "session": "default", "message_id": "u1"}))
        .unwrap_err();
    assert_eq!(refused["code"], "permission_denied");

    // The agent starts and takes the queue whole, in its order, and never the withdrawn message.
    ada.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    ada.wait_turns("default", 1);
    let asked = sent_messages(&world.server, 0);
    assert_eq!(asked.last().unwrap()["content"], "third\n\nfirst", "one author reached the agent: no names");
    assert!(queued(&mut ada).is_empty());
    for operation in [
        json!({"op": "conversation.withdraw", "session": "default", "message_id": "u1"}),
        json!({"op": "conversation.move", "session": "default", "message_id": "u3"}),
    ] {
        let taken = grace.request(operation).unwrap_err();
        assert_eq!(taken["code"], "invalid_input");
        assert!(taken["message"].as_str().unwrap().starts_with("already taken:"), "{taken}");
    }
    drop((ada, grace, elsewhere));
    assert!(world.supervisor.shutdown().is_clean());
}

/// A message marked Send now reaches the agent between two steps of its turn:
/// not while its tool call runs, but right after that call's result, in the
/// same turn; a withdrawn message never. A later agent rebuilds the turn with
/// the message where it was taken.
#[test]
fn a_message_marked_send_now_is_read_after_the_tool_result_in_the_same_turn() {
    let mut world = boot_options("8", false, &[], true);
    world.scenario.mkfifo("data/work/pipe");
    let mut grants = ui_grants(&[], true);
    grants.push(CapSpec { delegable: false, ..cap("conversation", &["queue", "withdraw", "send_now"], &[]) });
    world.supervisor.boot(program("ui"), grants).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    // The turn's first response appends to a pipe: the call runs until the test reads it.
    let append = json!({"path": "work/pipe", "content": "appended"});
    world.server.script(Scripted::json(200, &tool_request(MODEL, &[("append", "fs_append", append)])));
    world.server.script(Scripted::json(200, &completion(MODEL, "Appended; plan B noted.")));
    ui.say("default", "u1", "Append to the pipe.").unwrap();
    world.scenario.wait_access("data", "pipe");
    ui.say("default", "u2", "Use plan B instead.").unwrap();
    ui.say("default", "u3", "Withdrawn text.").unwrap();
    ui.request(json!({"op": "conversation.withdraw", "session": "default", "message_id": "u3"})).unwrap();
    assert_eq!(queued(&mut ui), ["u2"], "nothing is taken while the call runs");
    ui.request(json!({"op": "conversation.send_now", "session": "default", "message_id": "u2"})).unwrap();

    assert_eq!(std::fs::read(world.scenario.host("data/work/pipe")).unwrap(), b"appended");
    let log = ui.wait_turns("default", 1);
    assert_eq!(world.server.received().len(), 2, "one turn, which went on");
    let sent = sent_messages(&world.server, 1);
    let result = sent.iter().position(|m| m["tool_call_id"] == "append").unwrap();
    assert_eq!(sent[result + 1..], [json!({"role": "user", "content": "Use plan B instead."})], "{sent:#?}");
    assert!(!format!("{:?}", world.server.received()).contains("Withdrawn text."));
    assert!(queued(&mut ui).is_empty());
    let at = |wanted: &dyn Fn(&Value) -> bool| log.iter().position(wanted).unwrap();
    let (answered, taken, next) = (
        at(&|r| r["kind"] == "tool_result" && r["call_id"] == "append"),
        at(&|r| r["kind"] == "delivered" && r["id"] == "u2"),
        at(&|r| r["kind"] == "model_call" && r["step"] == 2),
    );
    assert!(answered < taken && taken < next, "{log:?}");

    // A later agent: the turn as its model saw it, the message where it was taken.
    ui.request(json!({"op": "sessions.stop", "session": "default"})).unwrap();
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    world.server.script(Scripted::json(200, &completion(MODEL, "Still on plan B.")));
    ui.say("default", "u4", "Which plan?").unwrap();
    ui.wait_turns("default", 2);
    let rebuilt = sent_messages(&world.server, 2);
    let said = |messages: &[Value]| -> Vec<Value> {
        messages.iter().filter(|m| m["role"] != "system").cloned().collect()
    };
    let (live, rebuilt) = (said(&sent), said(&rebuilt));
    assert_eq!(rebuilt[..live.len()], live[..], "{rebuilt:#?}");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
