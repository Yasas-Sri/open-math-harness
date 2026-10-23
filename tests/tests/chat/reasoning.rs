//! The actual model, chat and conversation processes carry readable reasoning
//! separately from answers. Providers and tools are deterministic local fixtures.

use super::*;
use std::time::Duration;

fn session(world: &World) -> nucleus_e2e::UiClient {
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op":"sessions.create", "session":"default"})).unwrap();
    ui
}

fn joined(log: &[Value], kind: &str) -> String {
    log.iter().filter(|r| r["kind"] == kind).map(|r| r["text"].as_str().unwrap()).collect()
}

fn scripted(events: Vec<Value>, gap: Duration) -> Scripted {
    let mut events: Vec<String> = events.into_iter().map(|e| e.to_string()).collect();
    events.push("[DONE]".into());
    Scripted::events(events, gap)
}

fn delta(value: Value) -> Value {
    json!({"model":MODEL,"choices":[{"index":0,"delta":value}]})
}

fn finish() -> Value {
    json!({"model":MODEL,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":20,"cost":0}})
}

#[test]
fn reasoning_is_visible_before_the_answer_and_final_history_does_not_duplicate_it() {
    let mut world = boot_with("4", true);
    let first = "First analyze the priority queue.🚀\n";
    let rest = "Then count the relaxations of each edge.";
    let gate = world.server.script_gated_stream(scripted(vec![
        delta(json!({"reasoning":first,"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque-secret"}]})),
        delta(json!({"reasoning":rest})), delta(json!({"content":"The answer."})), finish(),
    ], Duration::from_millis(5)));
    let mut ui = session(&world);
    ui.say("default", "reasoning", "Analyze the running time.").unwrap();
    assert!(wait_for(TIMEOUT, || !joined(&ui.history("default"), "reasoning_delta").is_empty()));
    let partial = ui.history("default");
    assert_eq!(joined(&partial, "reasoning_delta"), first);
    assert!(joined(&partial, "assistant_delta").is_empty());
    assert!(!partial.iter().any(|r| r["kind"] == "turn_complete"));
    gate.release();
    let complete = ui.wait_turns("default", 1);
    assert_eq!(joined(&complete, "reasoning"), format!("{first}{rest}"));
    assert_eq!(joined(&complete, "assistant"), "The answer.");
    assert!(!complete.iter().any(|r| r["kind"] == "reasoning_delta" || r["kind"] == "assistant_delta"));
    assert!(!serde_json::to_string(&complete).unwrap().contains("opaque-secret"));
    assert!(world.scenario.conversation_log("default").iter().any(|r| r["kind"] == "reasoning_delta"));
    drop(ui);
    let mut reopened = world.scenario.connect_ui("ui");
    assert_eq!(joined(&reopened.history("default"), "reasoning"), format!("{first}{rest}"));
    assert_eq!(world.server.received().len(), 1, "history reads never call the provider");
    drop(reopened);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn long_nonstream_reasoning_preserves_unicode_across_display_chunks() {
    let mut world = boot("2");
    let reasoning = "€🚀".repeat(20_000);
    let mut value = completion(MODEL, "Done.");
    value["choices"][0]["message"]["reasoning"] = json!(reasoning);
    world.server.script(Scripted::json(200, &value));
    let mut ui = session(&world);
    ui.say("default", "large", "Think then answer.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert_eq!(joined(&log, "reasoning"), reasoning);
    assert_eq!(joined(&log, "assistant"), "Done.");
    let records: Vec<_> = log.iter().filter(|r| r["kind"] == "reasoning").collect();
    assert!(records.len() > 1);
    assert!(records.iter().all(|r| r["text"].as_str().unwrap().len() <= 64 * 1024));
    let id = records[0]["message_id"].as_str().unwrap();
    for (index, record) in records.iter().enumerate().skip(1) {
        assert_eq!(record["message_id"], format!("{id}:chunk:{index}"));
    }
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn configured_deadline_preserves_partial_reasoning_without_dispatching_tools_or_retrying() {
    let mut world = boot_options("4", true, &["--model-deadline-ms", "1200"], true);
    let tool = json!({"index":0,"id":"never","type":"function","function":{
        "name":"fs_create","arguments":"{\"path\":\"work/never.txt\",\"content\":\"bad\"}"}});
    // Held after the first event: the deadline passes mid-stream.
    let events = vec![delta(json!({"reasoning":"Working. ","tool_calls":[tool]})), finish()];
    let _held = world.server.script_gated_stream(scripted(events, Duration::ZERO));
    let mut ui = session(&world);
    ui.say("default", "timeout", "Please create the file.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(joined(&log, "error").contains("Timeout"), "{log:?}");
    assert!(!joined(&log, "reasoning_delta").is_empty(), "partial reasoning survives a failed request");
    assert!(!log.iter().any(|r| r["kind"] == "tool_call" || r["kind"] == "tool_result"));
    assert!(!world.scenario.host("data/work/never.txt").exists());
    assert_eq!(world.server.received().len(), 1, "the agent did not retry the timed-out request");
    // A new explicit user turn succeeds, without replaying unfinished work.
    world.server.script(Scripted::stream(MODEL, &["Recovered."], Duration::from_millis(5)));
    ui.say("default", "retry-user", "Just answer briefly.").unwrap();
    let log = ui.wait_turns("default", 2);
    assert_eq!(joined(&log, "assistant"), "Recovered.");
    assert!(!world.scenario.host("data/work/never.txt").exists());
    assert_eq!(world.server.received().len(), 2);
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// A provider that ends a stream with an error after showing text and
/// reasoning: the call is made again with the same request, the tool before it
/// does not run again, and the failed call's pieces give way to the answer.
#[test]
fn a_stream_cut_after_output_is_asked_again_without_rerunning_tools_or_keeping_its_pieces() {
    let mut world = boot_options("6", true, &["--model-retry-backoff-ms", "1"], false);
    let read = tool_request(MODEL, &[("read", "fs_read", json!({"path":"work/notes.txt"}))]);
    world.server.script(generation::answer(read, true));
    world.server.script(scripted(
        vec![
            delta(json!({"reasoning":"Half a thought. "})),
            delta(json!({"content":"Half an answer. "})),
            json!({"error":{"message":"upstream stream ended unexpectedly"}}),
        ],
        Duration::from_millis(5),
    ));
    world.server.script(generation::answer(completion(MODEL, "The notes say 42."), true));
    let mut ui = session(&world);
    ui.say("default", "cut", "What do the notes say?").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(joined(&log, "error").is_empty(), "{log:?}");
    assert_eq!(joined(&log, "assistant"), "The notes say 42.");
    assert!(!serde_json::to_string(&log).unwrap().contains("Half a"), "{log:?}");
    let received = world.server.received();
    assert_eq!(received.len(), 3);
    assert_eq!(received[1].body, received[2].body, "the failed request is made again unchanged");
    assert_eq!(world.scenario.access_log("data").len(), 1, "the tool ran once");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn stopping_a_thinking_session_does_not_report_a_model_timeout() {
    let mut world = boot_with("4", true);
    let gate = world.server.script_gated_stream(scripted(
        vec![
            delta(json!({"reasoning":"Thinking before a stop."})),
            delta(json!({"content":"Late."})),
            finish(),
        ],
        Duration::from_millis(5),
    ));
    let mut ui = session(&world);
    ui.say("default", "stop", "Think carefully.").unwrap();
    assert!(wait_for(TIMEOUT, || !joined(&ui.history("default"), "reasoning_delta").is_empty()));
    let agent = ProcessOf::session(&mut ui, "default");
    ui.request(json!({"op":"sessions.stop","session":"default"})).unwrap();
    assert!(wait_for(TIMEOUT, || !world.supervisor.is_live(agent)));
    let log = ui.history("default");
    assert!(!joined(&log, "error").contains("Timeout"));
    assert_eq!(joined(&log, "reasoning_delta"), "Thinking before a stop.");
    gate.release();
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

/// A session's agent thinks as hard as its `create` said: its model requests
/// carry that effort, `sessions.list` reports it, for a stopped session too,
/// and its `model_call` records say it, which is where a window finds it once
/// the sessions service has forgotten the agent. An agent started again
/// without a new `create` (an interrupt) keeps it. Changing it is a stop and
/// a create, after which the agent goes on with the same conversation; a
/// value that is not a setting starts no agent.
#[test]
fn a_session_s_reasoning_effort_is_set_when_its_agent_starts_and_the_conversation_continues() {
    let mut world = boot("4");
    let mut grants = ui_grants(&[], true);
    grants.push(CapSpec { delegable: false, ..cap("sessions", &["interrupt"], &[]) });
    world.supervisor.boot(program("ui"), grants).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    let create =
        |effort: Value| json!({"op": "sessions.create", "session": "default", "reasoning_effort": effort});
    for refused in [json!("ultra"), json!("MAX"), json!(3)] {
        assert_eq!(ui.request(create(refused)).unwrap_err()["code"], "invalid_input");
    }
    let listed = |ui: &mut nucleus_e2e::UiClient| {
        let list = ui.request(json!({"op": "sessions.list"})).unwrap();
        (list["sessions"][0]["reasoning_effort"].clone(), list["sessions"][0]["live"].clone())
    };
    // (what `create` names, what the provider is asked, what `list` says, what the log says)
    let settings = [
        (json!("high"), json!({"effort": "high"}), json!("high"), "high"),
        (json!("default"), Value::Null, Value::Null, "default"),
        (json!("off"), json!({"enabled": false}), json!("off"), "off"),
        (Value::Null, Value::Null, Value::Null, "default"),
    ];
    for (turn, (setting, sent, said, recorded)) in settings.into_iter().enumerate() {
        let created = ui.request(create(setting.clone())).unwrap();
        assert_eq!(created["reasoning_effort"], said, "{setting}");
        if turn == 0 {
            // Its agent is started again without being named an effort: it keeps this one.
            let before = created["process"].clone();
            ui.request(json!({"op": "sessions.interrupt", "session": "default"})).unwrap();
            let again = ui.request(json!({"op": "sessions.list"})).unwrap()["sessions"][0].clone();
            assert!(again["process"] != before && again["live"] == true, "{again}");
            assert_eq!(again["reasoning_effort"], said);
        }
        world.server.script(Scripted::json(200, &completion(MODEL, "Answered.")));
        ui.say("default", &format!("u{turn}"), &format!("Question {turn}.")).unwrap();
        let log = ui.wait_turns("default", turn + 1);
        let body: Value = serde_json::from_str(&world.server.received()[turn].body).unwrap();
        assert_eq!(body["reasoning"], sent, "what the provider is asked for {setting}");
        assert!(body.to_string().contains("Question 0."), "the conversation went on: {body}");
        let call = log.iter().rfind(|r| r["kind"] == "model_call").unwrap();
        assert_eq!(call["reasoning_effort"], recorded, "the log says what this agent ran at: {call}");
        assert_eq!(listed(&mut ui), (said.clone(), json!(true)));
        ui.request(json!({"op": "sessions.stop", "session": "default"})).unwrap();
        assert_eq!(listed(&mut ui), (said, json!(false)), "a stopped session's effort is still listed");
    }
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn a_message_asking_for_ultramath_without_its_tools_is_told_so_and_an_invalid_mode_is_refused() {
    let mut world = boot("4");
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let say = |id: &str, mode: Value| {
        json!({"op": "conversation.say", "session": "default", "message_id": id, "text": "Solve it.",
            "settings": {"reasoning_mode": mode, "reasoning_effort": "low"}})
    };
    for mode in [json!("ULTRAMATH"), json!("unknown"), json!(1)] {
        assert_eq!(ui.request(say("bad", mode)).unwrap_err()["code"], "invalid_input");
    }
    world.server.script(Scripted::json(200, &completion(MODEL, "Working with the available tools.")));
    ui.request(say("mode-unavailable", json!("ultramath"))).unwrap();
    ui.wait_turns("default", 1);
    let body: Value = serde_json::from_str(&world.server.received()[0].body).unwrap();
    let system =
        body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "system").unwrap()["content"]
            .as_str()
            .unwrap();
    assert!(system.contains("Ultramath tools are unavailable") && !system.contains("# Ultramath mode"));
    assert_eq!(body["reasoning"]["effort"], "max", "Ultramath reasons at Max");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
