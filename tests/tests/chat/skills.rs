//! A project's skills reach the model (the `skills` service): the agent's
//! instructions list them, and a person's `/skill NAME` is the agent's own
//! `skills_load` call, after the message and before the model's first step,
//! shown in the conversation like any tool call.

use super::*;

#[test]
fn the_instructions_list_the_skills_and_a_skill_command_loads_one_before_the_model() {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    let skill = scenario.host("work/.skill/proof/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: proof\ndescription: Prove a lemma step by step.\n---\nState the lemma first.\n",
    )
    .unwrap();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    scenario.add_skills("skills", "work");
    scenario.add_chat_agent("chat", MODEL, &["--max-steps", "4"]);
    scenario.add_sessions("sessions", "chat");
    scenario.add_ui("ui");
    let mut supervisor = scenario.start();
    for (name, endpoint) in [("openrouter", "model"), ("conversation", "conversation"), ("skills", "skills")]
    {
        scenario.boot_service(&supervisor, name, endpoint);
    }
    let mut grants = sessions_grants();
    grants.push(cap("skills", &["list", "load"], &[]));
    let sessions = supervisor.boot(program("sessions"), grants).unwrap();
    let target = nucleus_e2e::endpoint("sessions");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == sessions)));
    server.script(Scripted::json(200, &completion(MODEL, "The lemma first, then.")));
    supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();
    let say = json!({"op": "conversation.say", "session": "default", "message_id": "u1",
                     "text": "/skill proof for the lemma"});
    ui.request(say).unwrap();
    let log = ui.wait_turns("default", 1);

    let sent = sent_messages(&server, 0);
    let instructions = sent[0]["content"].as_str().unwrap();
    // The instructions say how to use skills; the list, with its descriptions, is a message
    // of its own that says it is data.
    assert!(instructions.contains("skills_load") && !instructions.contains("- proof:"), "{instructions}");
    let listed = sent
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.starts_with(nucleus_prompts::skills::CATALOG_FRAME))
        .unwrap_or_default();
    assert!(listed.contains("This project's skills:\n- proof: Prove a lemma step by step."), "{listed}");
    // The message as the person wrote it, the agent's own load, its result,
    // and only then the model.
    let last = sent.len() - 3;
    assert_eq!(sent[last]["role"], "user");
    assert!(sent[last]["content"].as_str().unwrap().starts_with("/skill proof for the lemma"));
    assert_eq!(sent[last + 1]["tool_calls"][0]["function"]["name"], "skills_load");
    assert_eq!(sent[last + 2]["role"], "tool");
    assert!(sent[last + 2]["content"].as_str().unwrap().contains("State the lemma first."));
    let call = log.iter().find(|r| r["kind"] == "tool_call").unwrap();
    assert_eq!((call["name"].as_str(), &call["arguments"]), (Some("skills_load"), &json!({"name": "proof"})));
    assert!(log.iter().any(|r| r["kind"] == "tool_result" && r["error_code"].is_null()), "{log:?}");
    assert_eq!(log.last().unwrap()["kind"], "turn_complete");

    // Tags use consecutive commands. Every selected skill is loaded (and a
    // missing one reported) before the next model step, with the user text intact.
    let audit = scenario.host("work/.skill/audit/SKILL.md");
    std::fs::create_dir_all(audit.parent().unwrap()).unwrap();
    std::fs::write(&audit, "---\nname: audit\ndescription: Audit statements.\n---\nCheck the hypotheses.\n")
        .unwrap();
    server.script(Scripted::json(200, &completion(MODEL, "Both instructions read.")));
    let text = "/skill proof\n/skill audit\n/skill missing\nProve and audit this lemma.";
    ui.request(json!({"op":"conversation.say", "session":"default", "message_id":"u2", "text":text}))
        .unwrap();
    let log = ui.wait_turns("default", 2);
    let sent = sent_messages(&server, 1);
    let user = sent.iter().rposition(|m| m["role"] == "user").unwrap();
    assert!(sent[user]["content"].as_str().unwrap().starts_with(text));
    for (offset, name) in [(1, "proof"), (3, "audit"), (5, "missing")] {
        let call = &sent[user + offset]["tool_calls"][0]["function"];
        assert_eq!(call["name"], "skills_load");
        let args: serde_json::Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["name"], name);
        assert_eq!(sent[user + offset + 1]["role"], "tool");
    }
    assert!(sent[user + 2]["content"].as_str().unwrap().contains("State the lemma first."));
    assert!(sent[user + 4]["content"].as_str().unwrap().contains("Check the hypotheses."));
    assert!(log.iter().any(|r| r["kind"] == "tool_result" && !r["error_code"].is_null()));
    assert_eq!(log.last().unwrap()["kind"], "turn_complete");

    drop(ui);
    assert!(supervisor.shutdown().is_clean());
}
