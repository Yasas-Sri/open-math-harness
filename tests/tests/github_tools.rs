//! A new session discovers and invokes the real project-scoped GitHub broker.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use nucleus_e2e::http::{FakeHttp, Scripted, completion, tool_request};
use nucleus_e2e::{Scenario, TIMEOUT, cap, endpoint, program};
use serde_json::{Value, json};

#[test]
fn a_new_session_discovers_github_and_gets_a_tool_answer_without_credentials() {
    let model = "deepseek/deepseek-v4-flash";
    let server = FakeHttp::start();
    server.script(Scripted::json(
        200,
        &tool_request(
            model,
            &[
                ("g1", "github_status", json!({})),
                ("g2", "github_pull_request", json!({"reference":"https://github.com/Lab/paper/pull/7"})),
                ("g3", "github_issue", json!({"reference":8})),
                ("g4", "github_add_pull_request_labels", json!({"reference":7,"labels":["bug"]})),
                ("g5", "github_remove_pull_request_label", json!({"reference":7,"label":"bug"})),
            ],
        ),
    ));
    server.script(Scripted::json(200, &completion(model, "Connection checked.")));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("model", &server.base_url(), "sk-or-test-GITHUB-TOOLS-0123456789", &[], true);
    scenario.add_conversation("conversation");
    scenario.add_chat_agent("chat", model, &["--max-steps", "3"]);
    scenario.add_sessions("sessions", "chat");
    scenario.add_args("sessions", &["--project-scope", "github"]);
    scenario.add_ui("ui");
    // Missing link configuration must become a tool error, never a stuck dispatch.
    scenario.add_github_broker("github", &json!({}));
    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "model", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    scenario.boot_service(&supervisor, "github", "github");
    let sessions = supervisor
        .boot(
            program("sessions"),
            vec![
                cap("kernel", &["provide"], &["endpoint", "sessions"]),
                nucleus_e2e::session_spawn(),
                cap("conversation", &["list", "receive", "post", "history"], &[]),
                cap("model", &["complete", "start"], &["deepseek"]),
                cap(
                    "github",
                    &[
                        "status",
                        "fetch",
                        "push",
                        "pull_request",
                        "issue",
                        "fetch_pull_request",
                        "add_pull_request_labels",
                        "remove_pull_request_label",
                    ],
                    &["projects"],
                ),
            ],
        )
        .unwrap();
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s
            .provider_of(&endpoint("sessions"))
            .is_some_and(|(p, _)| p == sessions))
    );
    supervisor
        .boot(
            program("ui"),
            vec![
                cap("conversation", &["say", "history"], &[]),
                cap("sessions", &["create", "stop", "list"], &[]),
            ],
        )
        .unwrap();
    let mut ui = scenario.connect_ui("ui");
    let created = ui.request(json!({"op":"sessions.create", "session":"default"})).unwrap();
    let github = created["grants"].as_array().unwrap().iter().find(|g| g["endpoint"] == "github").unwrap();
    assert_eq!(github["resource"], json!(["projects", "default"]));
    ui.say("default", "u1", "Get the connected repository.").unwrap();
    let log = ui.wait_turns("default", 1);
    assert!(log.iter().any(|r| r["kind"] == "assistant" && r["text"] == "Connection checked."), "{log:?}");
    let received = server.received();
    assert_eq!(received.len(), 2);
    let first: Value = serde_json::from_str(&received[0].body).unwrap();
    let tools = first["tools"].as_array().unwrap();
    for name in [
        "github_status",
        "github_fetch",
        "github_push",
        "github_pull_request",
        "github_issue",
        "github_fetch_pull_request",
        "github_add_pull_request_labels",
        "github_remove_pull_request_label",
    ] {
        let tool = tools.iter().find(|t| t["function"]["name"] == name).unwrap();
        assert!(tool["function"]["parameters"]["properties"].get("project").is_none());
        assert!(tool["function"]["parameters"]["properties"].get("token").is_none());
    }
    let second: Value = serde_json::from_str(&received[1].body).unwrap();
    assert!(second["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool"), "{second}");
    for id in ["g1", "g2", "g3", "g4", "g5"] {
        assert!(
            second["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == id),
            "{second}"
        );
    }
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}
