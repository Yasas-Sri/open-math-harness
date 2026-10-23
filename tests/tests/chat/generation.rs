//! Provider finish reasons are exercised through the actual model, chat, and
//! conversation processes. A fake HTTP server supplies data, not an agent mock.

use super::*;
use std::time::Duration;

fn session(world: &World) -> nucleus_e2e::UiClient {
    session_at(world, "default")
}

/// A window whose session's agent runs at the reasoning effort `effort`.
fn session_at(world: &World, effort: &str) -> nucleus_e2e::UiClient {
    world.supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = world.scenario.connect_ui("ui");
    ui.request(json!({"op":"sessions.create", "session":"default", "reasoning_effort":effort})).unwrap();
    ui
}

fn wire(world: &World, index: usize) -> Value {
    serde_json::from_str(&world.server.received()[index].body).unwrap()
}

fn text(log: &[Value]) -> String {
    log.iter()
        .filter(|record| record["kind"] == "assistant")
        .map(|record| record["text"].as_str().unwrap())
        .collect()
}

fn errors(log: &[Value]) -> Vec<&str> {
    log.iter()
        .filter(|record| record["kind"] == "error")
        .map(|record| record["text"].as_str().unwrap())
        .collect()
}

fn response(content: &str, finish: &str) -> Value {
    let mut value = completion(MODEL, content);
    value["choices"][0]["finish_reason"] = json!(finish);
    value
}

/// Emit the same provider response through either HTTP completion or SSE.
/// Each tool's argument string is intentionally left untouched, including
/// incomplete JSON. The actual model adapter must preserve the finish reason.
pub(super) fn answer(value: Value, streaming: bool) -> Scripted {
    if !streaming {
        return Scripted::json(200, &value);
    }
    let message = &value["choices"][0]["message"];
    let mut delta = json!({});
    for key in ["content", "reasoning"] {
        if !message[key].is_null() {
            delta[key] = message[key].clone();
        }
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        delta["tool_calls"] = json!(
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let mut call = call.clone();
                    call["index"] = json!(index);
                    call
                })
                .collect::<Vec<_>>()
        );
    }
    let events = vec![
        json!({"model":MODEL,"choices":[{"index":0,"delta":delta}]}).to_string(),
        json!({"model":MODEL,"choices":[{"index":0,"delta":{},
            "finish_reason":value["choices"][0]["finish_reason"]}]})
        .to_string(),
        json!({"choices":[],"usage":value["usage"]}).to_string(),
        "[DONE]".to_owned(),
    ];
    Scripted::events(events, Duration::from_millis(2))
}

/// Every model call of a turn, a length continuation too, asks for the
/// session's output budget and reasoning effort: a chosen effort is sent as
/// such, and the default asks for nothing.
#[test]
fn the_output_budget_and_reasoning_effort_reach_the_provider_on_every_call() {
    // (agent flags, effort, max_tokens sent, reasoning effort sent)
    for (extra, effort, max_tokens, sent) in [
        (&[][..], "default", None, None),
        (&["--max-output-tokens", "8192"][..], "default", Some(8192), None),
        (&[][..], "high", None, Some("high")),
        (&["--max-output-tokens", "393216"][..], "max", Some(393216), Some("max")),
    ] {
        let mut world = boot_options("4", false, extra, false);
        world.server.script(answer(response("Partial answer. ", "length"), false));
        world.server.script(answer(completion(MODEL, "The conclusion."), false));
        let mut ui = session_at(&world, effort);
        ui.say("default", "budget", "Explain the result.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(text(&log), "Partial answer. The conclusion.");
        assert_eq!(world.server.received().len(), 2);
        for index in 0..2 {
            let request = wire(&world, index);
            assert_eq!(
                request.get("max_tokens"),
                max_tokens.map(|n| json!(n)).as_ref(),
                "{extra:?} {effort}"
            );
            match sent {
                None => assert!(request.get("reasoning").is_none(), "{effort}: {request}"),
                Some(sent) => {
                    assert_eq!(request["reasoning"]["effort"], sent);
                }
            }
        }
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn length_text_continues_in_both_transport_modes_without_losing_the_prefix() {
    for streaming in [false, true] {
        let mut world = boot_with("4", streaming);
        world.server.script(answer(response("The first part. ", "length"), streaming));
        world.server.script(answer(completion(MODEL, "The conclusion."), streaming));
        let mut ui = session(&world);
        ui.say("default", "continue", "Explain the result.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert_eq!(text(&log), "The first part. The conclusion.", "streaming={streaming}");
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(world.server.received().len(), 2);
        let history = sent_messages(&world.server, 1);
        assert!(history.iter().any(|m| m["role"] == "assistant" && m["content"] == "The first part. "));
        assert_eq!(history.last().unwrap()["role"], "user");
        let continuation = history.last().unwrap()["content"].as_str().unwrap().to_ascii_lowercase();
        assert!(continuation.contains("continu"));
        // A continuation has a fresh per-request allowance, not a shrinking turn budget.
        assert!(!continuation.contains("budget") && !continuation.contains("remaining"));
        let reports: Vec<_> = log.iter().filter(|r| r["kind"] == "model_call").collect();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0]["finish_reason"], "length");
        assert_eq!(reports[1]["finish_reason"], "stop");
        assert!(reports.iter().all(|r| r["streamed"] == streaming));
        for index in 0..2 {
            assert!(wire(&world, index).get("max_tokens").is_none());
        }
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn reasoning_only_length_is_displayed_separately_without_replaying_it_to_the_model() {
    for streaming in [false, true] {
        let mut world = boot_with("4", streaming);
        let mut truncated = response("", "length");
        truncated["choices"][0]["message"]["reasoning"] = json!("private reasoning marker");
        world.server.script(answer(truncated, streaming));
        world.server.script(answer(completion(MODEL, "The visible answer."), streaming));
        let mut ui = session(&world);
        ui.say("default", "reasoning", "Solve the problem.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert_eq!(text(&log), "The visible answer.");
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(world.server.received().len(), 2);
        let history = sent_messages(&world.server, 1);
        assert!(!history.iter().any(|m| m["role"] == "assistant"));
        assert!(!serde_json::to_string(&history).unwrap().contains("private reasoning marker"));
        assert_eq!(
            log.iter()
                .filter(|r| r["kind"] == "reasoning")
                .map(|r| r["text"].as_str().unwrap())
                .collect::<String>(),
            "private reasoning marker"
        );
        assert!(!text(&log).contains("private reasoning marker"));
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn length_tool_calls_never_execute_even_when_the_arguments_are_valid() {
    for streaming in [false, true] {
        let mut world = boot_options("4", streaming, &[], true);
        let mut truncated = tool_request(
            MODEL,
            &[
                (
                    "valid_cut_call",
                    "fs_create",
                    json!({"path":"work/forbidden.txt", "content":"side effect"}),
                ),
                ("invalid_cut_call", "fs_append", json!({"path":"work/notes.txt", "content":"mutated"})),
            ],
        );
        truncated["choices"][0]["finish_reason"] = json!("length");
        truncated["choices"][0]["message"]["content"] = json!("A partial explanation. ");
        truncated["choices"][0]["message"]["tool_calls"][1]["function"]["arguments"] = json!("{\"path\":");
        world.server.script(answer(truncated, streaming));
        world.server.script(answer(completion(MODEL, "No changes were made."), streaming));
        let mut ui = session(&world);
        ui.say("default", "cut-tool", "Consider writing a file.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert!(world.scenario.access_log("data").is_empty(), "truncated tool calls reached fs");
        assert!(!world.scenario.host("data/work/forbidden.txt").exists());
        assert_eq!(
            std::fs::read_to_string(world.scenario.host("data/work/notes.txt")).unwrap(),
            "the launch code is 42"
        );
        assert!(log.iter().all(|r| r["kind"] != "tool_call" && r["kind"] != "tool_result"));
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(text(&log), "A partial explanation. No changes were made.");
        assert_eq!(world.server.received().len(), 2);
        let history = sent_messages(&world.server, 1);
        assert!(history.iter().all(|m| m["role"] != "tool" && m.get("tool_calls").is_none()));
        assert!(!serde_json::to_string(&history).unwrap().contains("cut_call"));
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

/// A provider may cut several times even without a caller output budget.
#[test]
fn default_length_continuation_has_no_two_or_eight_attempt_policy_limit() {
    let mut world = boot_options("16", false, &[], false);
    for _ in 0..10 {
        world.server.script(answer(response("partial ", "length"), false));
    }
    world.server.script(answer(completion(MODEL, "Done."), false));
    let mut ui = session(&world);
    ui.say("default", "long", "Finish the answer.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(errors(&log).is_empty(), "{log:?}");
    assert_eq!(text(&log), format!("{}Done.", "partial ".repeat(10)));
    assert_eq!(world.server.received().len(), 11);
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn length_continuations_are_finite_and_reset_for_the_next_user_turn() {
    for (extra, calls) in [
        (vec!["--max-length-continuations", "2"], 3),
        (vec!["--max-length-continuations", "0"], 1),
        (vec!["--max-length-continuations", "1"], 2),
    ] {
        let mut world = boot_options("8", false, &extra, false);
        for _ in 0..calls {
            world.server.script(answer(response("partial ", "length"), false));
        }
        let mut ui = session(&world);
        ui.say("default", "exhaust", "Give a long answer.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert_eq!(world.server.received().len(), calls);
        assert_eq!(text(&log), "partial ".repeat(calls));
        assert_eq!(errors(&log).len(), 1, "{log:?}");
        assert!(errors(&log)[0].contains("continuation limit"), "{log:?}");
        world.server.script(answer(completion(MODEL, "A fresh turn succeeds."), false));
        ui.say("default", "recover", "Try a shorter answer.").unwrap();
        let next = ui.wait_turns("default", 2);
        assert_eq!(world.server.received().len(), calls + 1);
        assert_eq!(text(&next[log.len()..]), "A fresh turn succeeds.");
        assert!(errors(&next[log.len()..]).is_empty());
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn failed_compaction_preserves_history_and_is_retried_without_running_its_tools() {
    let mut tool_summary = tool_request(
        MODEL,
        &[(
            "summary_tool",
            "fs_create",
            json!({
                "path":"work/summary-side-effect.txt", "content":"wrong"
            }),
        )],
    );
    tool_summary["choices"][0]["finish_reason"] = json!("stop");
    tool_summary["choices"][0]["message"]["content"] = json!("A summary with a tool is invalid.");
    for invalid in [response("Unfinished summary", "length"), response("   ", "stop"), tool_summary] {
        let mut world =
            boot_options("2", false, &["--compact-at-tokens", "1", "--max-output-tokens", "8192"], true);
        world.server.script(answer(completion(MODEL, "Remember the original answer."), false));
        world.server.script(answer(completion(MODEL, "Remember the second answer."), false));
        world.server.script(answer(invalid, false));
        let mut ui = session_at(&world, "low");
        for (index, prompt) in
            ["Original question.", "Second question.", "Third question."].iter().enumerate()
        {
            ui.say("default", &format!("compact-{index}"), prompt).unwrap();
            ui.wait_turns("default", index + 1);
        }
        let failed = ui.history("default");
        assert_eq!(world.server.received().len(), 3, "failed compaction must stop this turn");
        assert_eq!(errors(&failed).len(), 1, "{failed:?}");
        assert!(errors(&failed)[0].contains("compaction failed"));
        assert!(!failed.iter().any(|r| r["kind"] == "compacted"));
        assert!(world.scenario.access_log("data").is_empty());
        assert!(!world.scenario.host("data/work/summary-side-effect.txt").exists());
        world.server.script(answer(completion(MODEL, "A complete replacement summary."), false));
        world.server.script(answer(completion(MODEL, "Recovered after compaction."), false));
        ui.say("default", "compact-recover", "Fourth question.").unwrap();
        let recovered = ui.wait_turns("default", 4);
        assert!(errors(&recovered[failed.len()..]).is_empty(), "{recovered:?}");
        assert_eq!(world.server.received().len(), 5);
        let retry = sent_messages(&world.server, 3);
        for original in [
            "Original question.",
            "Remember the original answer.",
            "Second question.",
            "Remember the second answer.",
            "Third question.",
            "Fourth question.",
        ] {
            // The failed turn's question and the next go as one message (successive user messages).
            let kept = |m: &Value| {
                m["content"].as_str().is_some_and(|c| c.split("\n\n").any(|part| part == original))
            };
            assert!(retry.iter().any(kept), "lost {original}: {retry:?}");
        }
        for compact_index in [2, 3] {
            let request = wire(&world, compact_index);
            assert!(request.get("tools").is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)));
            assert_eq!(request["max_tokens"], 8192);
            assert_eq!(
                request["reasoning"]["effort"], "low",
                "a summary is asked for at the session's effort"
            );
        }
        let after = sent_messages(&world.server, 4);
        assert!(
            after.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("A complete replacement summary.")))
        );
        assert!(!after.iter().any(|m| m["content"] == "Remember the original answer."));
        assert_eq!(text(&recovered[failed.len()..]), "Recovered after compaction.");
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn compaction_consumes_the_last_model_step_instead_of_making_an_extra_call() {
    let mut world = boot_options("1", false, &["--compact-at-tokens", "1"], false);
    for answer_text in ["First answer.", "Second answer.", "A complete summary."] {
        world.server.script(answer(completion(MODEL, answer_text), false));
    }
    let mut ui = session(&world);
    for turn in 1..=3 {
        ui.say("default", &format!("step-{turn}"), "Continue the discussion.").unwrap();
        ui.wait_turns("default", turn);
    }
    let log = ui.history("default");
    assert_eq!(world.server.received().len(), 3);
    assert_eq!(errors(&log).len(), 1, "{log:?}");
    assert!(errors(&log)[0].contains("step limit"));
    assert!(log.iter().any(|r| r["kind"] == "compacted"));
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}

#[test]
fn large_utf8_answers_cross_the_conversation_record_limit_without_loss_or_duplicate_deltas() {
    let large = "€∑∞".repeat(10_000); // 90 KiB; the 64 KiB cutoff splits a UTF-8 code point.
    for streaming in [false, true] {
        let mut world = boot_with("4", streaming);
        let reply = answer(completion(MODEL, &large), streaming);
        let gate = if streaming {
            Some(world.server.script_gated_stream(reply))
        } else {
            world.server.script(reply);
            None
        };
        world.server.script(answer(completion(MODEL, "Still available."), streaming));
        let mut ui = session(&world);
        ui.say("default", "large", "Produce the complete answer.").unwrap();
        if let Some(gate) = gate {
            assert!(
                wait_for(TIMEOUT, || {
                    ui.history("default")
                        .iter()
                        .filter(|record| record["kind"] == "assistant_delta")
                        .map(|record| record["text"].as_str().unwrap())
                        .collect::<String>()
                        == large
                }),
                "the whole UTF-8 delta must be visible before finalization"
            );
            gate.release();
        }
        let log = ui.wait_turns("default", 1);
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(text(&log), large, "streaming={streaming}");
        let chunks: Vec<_> = log.iter().filter(|r| r["kind"] == "assistant").collect();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|r| r["text"].as_str().unwrap().len() <= 65_536));
        let base = chunks[0]["message_id"].as_str().expect("final text has an identity");
        assert!(!base.is_empty());
        for (index, chunk) in chunks.iter().enumerate().skip(1) {
            assert_eq!(chunk["message_id"], format!("{base}:chunk:{index}"));
        }
        let disk = world.scenario.conversation_log("default");
        let deltas: Vec<_> = disk.iter().filter(|r| r["kind"] == "assistant_delta").collect();
        assert_eq!(!deltas.is_empty(), streaming);
        assert!(deltas.iter().all(|r| r["text"].as_str().unwrap().len() <= 65_536));
        assert!(deltas.iter().all(|r| r["message_id"] == base));
        assert!(!log.iter().any(|r| r["kind"] == "assistant_delta"), "completed deltas must be replaced");
        ui.say("default", "large-followup", "Refer to your previous answer.").unwrap();
        let next = ui.wait_turns("default", 2);
        assert!(errors(&next).is_empty(), "{next:?}");
        assert_eq!(world.server.received().len(), 2);
        let sent = sent_messages(&world.server, 1);
        assert_eq!(sent.iter().filter(|m| m["role"] == "assistant" && m["content"] == large).count(), 1);
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn a_continuation_can_reissue_a_complete_tool_call_without_replaying_finished_work() {
    for streaming in [false, true] {
        let mut world = boot_options("6", streaming, &[], true);
        let append = |suffix: &str| json!({"path":"work/notes.txt", "content":suffix});
        world.server.script(answer(
            tool_request(MODEL, &[("already_done", "fs_append", append(" FIRST"))]),
            streaming,
        ));
        let mut cut = tool_request(
            MODEL,
            &[
                ("already_done", "fs_append", append(" FIRST")),
                ("incomplete_call", "fs_append", append(" SECOND")),
            ],
        );
        cut["choices"][0]["finish_reason"] = json!("length");
        cut["choices"][0]["message"]["content"] = json!("One change is done. ");
        cut["choices"][0]["message"]["tool_calls"][1]["function"]["arguments"] = json!("{\"path\":");
        world.server.script(answer(cut, streaming));
        world.server.script(answer(
            tool_request(MODEL, &[("fresh_complete_call", "fs_append", append(" SECOND"))]),
            streaming,
        ));
        world.server.script(answer(completion(MODEL, "Both changes are done."), streaming));
        let mut ui = session(&world);
        ui.say("default", "reissue", "Append two distinct markers.").unwrap();
        let log = ui.wait_turns("default", 1);
        assert!(errors(&log).is_empty(), "{log:?}");
        assert_eq!(world.server.received().len(), 4);
        assert_eq!(world.scenario.access_log("data").len(), 2);
        assert_eq!(
            std::fs::read_to_string(world.scenario.host("data/work/notes.txt")).unwrap(),
            "the launch code is 42 FIRST SECOND"
        );
        assert_eq!(text(&log), "One change is done. Both changes are done.");
        let tools: Vec<_> = log.iter().filter(|r| r["kind"] == "tool_call").collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["call_id"], "already_done");
        assert_eq!(tools[1]["call_id"], "fresh_complete_call");
        let resumed = sent_messages(&world.server, 2);
        let previous_tools: Vec<_> = resumed.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(previous_tools.len(), 1);
        assert_eq!(previous_tools[0]["tool_call_id"], "already_done");
        assert_eq!(resumed.iter().filter_map(|m| m["tool_calls"].as_array()).map(Vec::len).sum::<usize>(), 1);
        assert!(!serde_json::to_string(&resumed).unwrap().contains("incomplete_call"));
        let finished = sent_messages(&world.server, 3);
        let results: Vec<_> = finished.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1]["tool_call_id"], "fresh_complete_call");
        drop(ui);
        assert!(world.supervisor.shutdown().is_clean());
    }
}

#[test]
fn large_tool_arguments_use_a_bounded_preview_but_execute_and_retain_the_original() {
    let content = "∀∃∑∏√∞".repeat(5_000); // 90 KiB of UTF-8 content.
    let arguments = json!({"path":"work/large-tool.txt", "content":content});
    assert!(arguments.to_string().len() > 65_536);
    let mut world = boot_options("4", true, &[], true);
    world
        .server
        .script(answer(tool_request(MODEL, &[("large_valid_call", "fs_create", arguments.clone())]), true));
    world.server.script(answer(completion(MODEL, "The entire file was created."), true));
    let mut ui = session(&world);
    ui.say("default", "large-tool", "Write the complete document.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(errors(&log).is_empty(), "{log:?}");
    assert_eq!(std::fs::read_to_string(world.scenario.host("data/work/large-tool.txt")).unwrap(), content);
    let calls: Vec<_> = log.iter().filter(|r| r["kind"] == "tool_call").collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0]["arguments"].to_string().len() <= 65_536, "the conversation keeps a bounded preview");
    let sent = sent_messages(&world.server, 1);
    let retained: Vec<_> = sent.iter().filter_map(|m| m["tool_calls"].as_array()).flatten().collect();
    assert_eq!(retained.len(), 1);
    let original: Value =
        serde_json::from_str(retained[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(original, arguments, "the model must retain full arguments, not their GUI preview");
    drop(ui);
    assert!(world.supervisor.shutdown().is_clean());
}
