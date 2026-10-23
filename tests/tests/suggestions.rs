//! The suggestions service as a real confined process under the real kernel,
//! asking the real OpenRouter adapter, which talks to a scripted local server.
//! No key, no network beyond loopback, no cost.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::http::{FakeHttp, Scripted, completion};
use nucleus_e2e::{Scenario, TIMEOUT, cap, endpoint, ok_value, program, provider_grants, wait_for};
use serde_json::{Value, json};

const MODEL: &str = "deepseek/deepseek-v4-flash";

/// A caller that asks once for the next message of session `s1` of the
/// default project, as the UI service does for a window.
fn caller(said: &str) -> Value {
    let messages = json!([{"role": "user", "text": said}, {"role": "assistant", "text": "Done."}]);
    json!({"turns": [{"tool_calls": [{"tool": "call", "endpoint": "suggestions", "action": "next",
                                      "path": ["default", "s1"], "payload": {"messages": messages}}]}]})
}

/// A newer request for a session interrupts the older one at once: the older
/// is answered with no text, and its model call is cancelled through the
/// kernel. The adapter cannot take back an HTTP request it already sent, so
/// what the cancellation makes observable is the adapter's only slot being
/// given up: the newer model call is answered while the older one's reply is
/// still held.
#[test]
fn a_newer_request_for_a_session_answers_the_older_with_nothing_and_cancels_its_model_call() {
    let server = FakeHttp::start();
    let (never, _held) = Scripted::json(200, &completion(MODEL, "Never offered.")).held();
    server.script_for("first question", never);
    server.script_for(
        "second question",
        Scripted::json(200, &completion(MODEL, "\"Run the tests.\"\nBecause")),
    );
    let mut scenario = Scenario::new();
    scenario.add_openrouter("model", &server.base_url(), "sk-or-test", &["--max-inflight", "1"], true);
    scenario.add_suggestions("suggestions", MODEL, &[]);
    scenario.add_agent("older", &caller("first question"), None);
    scenario.add_agent("newer", &caller("second question"), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "model", "model");
    let mut grants = provider_grants("suggestions");
    grants.push(cap("model", &["complete"], &["deepseek", "deepseek-v4-flash"]));
    let service = supervisor.boot(program("suggestions"), grants).unwrap();
    assert!(
        supervisor.wait_until(TIMEOUT, |s| s
            .provider_of(&endpoint("suggestions"))
            .is_some_and(|(p, _)| p == service)),
        "the service never provided suggestions"
    );
    let asks = vec![cap("suggestions", &["next"], &[])];
    supervisor.boot(program("older"), asks.clone()).unwrap();
    assert!(wait_for(TIMEOUT, || server.received().len() == 1), "the older request reached the provider");
    supervisor.boot(program("newer"), asks).unwrap();

    let older = scenario.wait_done("older");
    assert_eq!(
        ok_value(&older[0]),
        Some(&json!({"project": "default", "session": "s1"})),
        "superseded: no text; {older:?}"
    );
    let newer = scenario.wait_done("newer");
    assert_eq!(
        ok_value(&newer[0]),
        Some(&json!({"project": "default", "session": "s1", "text": "Run the tests."})),
        "{newer:?}"
    );

    let received = server.received();
    assert_eq!(received.len(), 2);
    let asked: Value = serde_json::from_str(&received[1].body).unwrap();
    assert!(asked["tools"].as_array().is_none_or(Vec::is_empty), "a suggestion offers no tools");
    assert_eq!(
        (&asked["messages"][0]["role"], &asked["messages"][1]["role"]),
        (&json!("system"), &json!("user"))
    );
    let transcript = asked["messages"][1]["content"].as_str().unwrap();
    assert!(transcript.contains("second question") && !transcript.contains("first question"), "{transcript}");
    assert!(supervisor.shutdown().is_clean());
}
