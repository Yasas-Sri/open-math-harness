//! The OpenRouter adapter as a real confined process, talking to a scripted
//! local server. No network beyond loopback, no key, no cost.
//!
//! `live_deepseek_completion` is the only test that contacts OpenRouter. It is
//! ignored by default; its doc comment says how to opt in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use nucleus_e2e::http::{FakeHttp, Scripted, completion};
use nucleus_e2e::{Scenario, cap, error_code, ok_value, program};
use serde_json::{Value, json};

const KEY: &str = "sk-or-test-DO-NOT-LEAK-0123456789";
const MODEL: &str = "deepseek/deepseek-v4-flash";

fn fixture(calls: Value) -> Value {
    json!({ "turns": [{ "tool_calls": calls }] })
}

fn ask(model: &str, prompt: &str) -> Value {
    json!({"tool": "model.complete", "model": model, "prompt": prompt, "max_output_tokens": 32})
}

/// Everything a test can observe, as one string, to search for leaks.
fn everything_observable(scenario: &Scenario, roles: &[&str]) -> String {
    let mut all = format!("{:?}", scenario.events());
    for role in roles {
        all.push_str(&format!("{:?}", scenario.transcript(role)));
    }
    all
}

#[test]
fn a_completion_passes_through_the_capability_check_to_the_provider() {
    let server = FakeHttp::start();
    server.script(Scripted::json(200, &completion(MODEL, "hi there")));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    scenario.add_agent(
        "agent",
        &fixture(json!([
            ask(MODEL, "Say hi"),
            ask("openai/gpt-4o", "Say hi"),
            ask("deepseek/../openai/gpt-4o", "x")
        ])),
        None,
    );

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    let lines = scenario.wait_done("agent");

    let answer = ok_value(&lines[0]).expect("completion succeeds");
    assert_eq!(answer["message"]["content"], "hi there");
    assert_eq!(answer["message"]["role"], "assistant");
    assert_eq!(answer["finish_reason"], "stop");
    assert_eq!(answer["usage"], json!({"input_tokens": 9, "output_tokens": 4, "cost_nano_usd": 12_000}));
    assert_eq!(answer["served_by"], MODEL);
    assert_eq!(error_code(&lines[1]), Some("permission_denied"), "outside the capability's model scope");
    assert_eq!(error_code(&lines[2]), Some("invalid_input"), "traversal is not a model id");

    let received = server.received();
    assert_eq!(received.len(), 1, "the denied calls never left the kernel: {received:?}");
    assert_eq!(
        (received[0].method.as_str(), received[0].path.as_str()),
        ("POST", "/api/v1/chat/completions")
    );
    assert_eq!(received[0].headers["authorization"], format!("Bearer {KEY}"));
    let body: Value = serde_json::from_str(&received[0].body).unwrap();
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["messages"], json!([{"role": "user", "content": "Say hi"}]));
    assert_eq!(body["max_tokens"], 32);
    // Asked for as a stream; a provider that answers with one JSON object, as this server does, is accepted.
    assert_eq!((&body["stream"], &body["stream_options"]["include_usage"]), (&json!(true), &json!(true)));

    assert!(supervisor.shutdown().is_clean());
    assert!(!everything_observable(&scenario, &["agent"]).contains(KEY), "the key leaked");
}

#[test]
fn redirects_are_not_followed_and_provider_errors_do_not_leak_the_key() {
    let server = FakeHttp::start();
    let elsewhere = FakeHttp::start();
    elsewhere.script(Scripted::json(200, &completion(MODEL, "from the wrong host")));
    server.script(Scripted::redirect(&elsewhere.url("/api/v1/chat/completions")));
    server.script(Scripted::json(429, &json!({"error": {"code": 429, "message": "slow down"}})));
    server.script(Scripted::json(401, &json!({"error": {"message": format!("no such key: {KEY}")}})));
    server.script(Scripted::json(200, &json!({"error": {"code": 502, "message": "upstream down"}})));
    server.script(Scripted::json(200, &completion(MODEL, "recovered")));

    let mut scenario = Scenario::new();
    // Retries off: this test maps each scripted status to one call's outcome.
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    let calls: Vec<Value> = (0..5).map(|_| ask(MODEL, "hi")).collect();
    scenario.add_agent("agent", &fixture(json!(calls)), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    let lines = scenario.wait_done("agent");

    assert_eq!(error_code(&lines[0]), Some("service_error"), "{:?}", lines[0]);
    assert!(elsewhere.received().is_empty(), "the key and prompt were re-sent to the redirect target");
    assert_eq!(error_code(&lines[1]), Some("service_error"));
    assert!(lines[1].to_string().contains("rate limit"), "{:?}", lines[1]);
    assert_eq!(error_code(&lines[2]), Some("service_error"));
    assert!(lines[2].to_string().contains("[redacted]"), "{:?}", lines[2]);
    assert_eq!(
        error_code(&lines[3]),
        Some("service_error"),
        "an error object with status 200 is still an error"
    );
    assert_eq!(
        ok_value(&lines[4]).unwrap()["message"]["content"],
        "recovered",
        "no retries, and no lasting damage"
    );
    assert_eq!(server.received().len(), 5, "exactly one attempt per call");

    assert!(supervisor.shutdown().is_clean());
    assert!(!everything_observable(&scenario, &["agent"]).contains(KEY), "the key leaked");
}

#[test]
fn without_the_operators_network_grant_the_adapter_reaches_nothing() {
    let server = FakeHttp::start();
    server.script(Scripted::json(200, &completion(MODEL, "should be unreachable")));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], false);
    // Some hosts refuse the connection at once, others drop it silently; bound the wait.
    let mut bounded = ask(MODEL, "hi");
    bounded["deadline_ms"] = json!(3_000);
    scenario.add_agent("agent", &fixture(json!([bounded])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    let lines = scenario.wait_done("agent");

    assert!(matches!(error_code(&lines[0]), Some("service_error" | "timeout")), "{:?}", lines[0]);
    assert!(server.received().is_empty(), "a sandbox without the network grant reached the host's loopback");
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn a_slow_provider_is_bounded_by_the_callers_deadline() {
    let server = FakeHttp::start();
    let (too_late, _held) = Scripted::json(200, &completion(MODEL, "too late")).held();
    server.script(too_late);
    server.script(Scripted::json(200, &completion(MODEL, "on time")));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    let mut slow = ask(MODEL, "hi");
    slow["deadline_ms"] = json!(2_000);
    scenario.add_agent("agent", &fixture(json!([slow, ask(MODEL, "hi again")])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    let first = scenario.wait_transcript("agent", 1);
    // The adapter gives up just before the kernel deadline; either may report first.
    assert!(matches!(error_code(&first[0]), Some("service_error" | "timeout")), "{:?}", first[0]);

    let lines = scenario.wait_done("agent");
    assert_eq!(
        ok_value(&lines[1]).unwrap()["message"]["content"],
        "on time",
        "the adapter survived the timeout"
    );
    assert!(supervisor.shutdown().is_clean());
}

/// Real request to OpenRouter. Costs money and needs the network.
///
/// `OPENROUTER_API_KEY=... cargo test -p nucleus-e2e --test openrouter -- --ignored live`
/// Optional: `NUCLEUS_OPENROUTER_MODEL` (default `deepseek/deepseek-v4-flash`).
/// The key is written to a private temporary directory that is deleted when
/// the test ends. The capability allows exactly one call.
#[test]
#[ignore = "contacts OpenRouter; needs OPENROUTER_API_KEY"]
fn live_deepseek_completion() {
    let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set for the live test");
    let model = std::env::var("NUCLEUS_OPENROUTER_MODEL").unwrap_or_else(|_| MODEL.to_owned());
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", "https://openrouter.ai/api/v1", key.trim(), &[], true);
    let mut call = ask(&model, "Reply with the single word: pong");
    call["max_output_tokens"] = json!(512);
    call["deadline_ms"] = json!(90_000);
    scenario.add_agent("agent", &fixture(json!([call.clone(), call])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    let mut one_call = cap("model", &["complete"], &["deepseek"]);
    one_call.max_calls = Some(1);
    one_call.delegable = false;
    supervisor.boot(program("agent"), vec![one_call]).unwrap();

    assert!(nucleus_e2e::wait_for(Duration::from_secs(100), || scenario.transcript("agent").len() >= 3));
    let lines = scenario.transcript("agent");
    let answer = ok_value(&lines[0]).unwrap_or_else(|| panic!("live call failed: {:?}", lines[0]));
    println!(
        "served_by={} usage={} content={:?}",
        answer["served_by"], answer["usage"], answer["message"]["content"]
    );
    assert_eq!(answer["message"]["role"], "assistant");
    assert_eq!(error_code(&lines[1]), Some("budget_exhausted"), "the spending bound held");
    assert!(supervisor.shutdown().is_clean());
    assert!(!everything_observable(&scenario, &["agent"]).contains(key.trim()), "the key leaked");
}

// ---- streaming, built from ordinary calls ---------------------------------------

/// Budgeted `start`, unbudgeted pulls: two capabilities, as the interface asks.
fn streaming_grants(starts: u64) -> Vec<nucleus_kernel::CapSpec> {
    let mut budgeted = cap("model", &["complete", "start"], &["deepseek"]);
    budgeted.max_calls = Some(starts);
    vec![budgeted, cap("model", &["next", "cancel"], &["deepseek"])]
}

#[test]
fn a_completion_is_streamed_through_ordinary_calls_and_pulls_are_not_charged() {
    let server = FakeHttp::start();
    let pieces = ["Stream", "ing ", "works ", "through ", "plain ", "calls."];
    server.script(Scripted::stream(MODEL, &pieces, Duration::from_millis(120)));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    let stream = json!({"tool": "model.stream", "model": MODEL, "prompt": "Say it in pieces", "max_output_tokens": 64});
    scenario.add_agent("agent", &fixture(json!([stream.clone(), stream])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    // One `start` allowed. The many pulls of the first stream must not use it up,
    // and the second `start` must be refused.
    supervisor.boot(program("agent"), streaming_grants(1)).unwrap();
    let lines = scenario.wait_done("agent");

    let first = ok_value(&lines[0]).unwrap_or_else(|| panic!("streaming failed: {:?}", lines[0]));
    assert_eq!(first["text"], pieces.concat());
    let arrived = first["pieces"].as_array().unwrap();
    assert!(arrived.len() >= 3, "the text arrived in several pulls, not at once: {arrived:?}");
    assert_eq!(arrived.iter().map(|p| p.as_str().unwrap()).collect::<String>(), pieces.concat());
    assert!(first["pulls"].as_u64().unwrap() > 1);
    assert_eq!(first["usage"], json!({"input_tokens": 9, "output_tokens": 6, "cost_nano_usd": 18_000}));
    assert_eq!(
        error_code(&lines[1]),
        Some("budget_exhausted"),
        "spending is bounded by `start`, not by pulls"
    );

    let sent: Value = serde_json::from_str(&server.received()[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert!(supervisor.shutdown().is_clean());
    assert!(!everything_observable(&scenario, &["agent"]).contains(KEY), "the key leaked");
}

/// Readable reasoning streams on its own channel. Tool-call fragments are
/// never text, but they move `produced` as they arrive, or a long tool call
/// looks like a stall to a puller watching for progress.
#[test]
fn readable_reasoning_is_streamed_separately_and_reconciles_with_the_final_response() {
    let server = FakeHttp::start();
    let hidden = json!([{"type":"reasoning.encrypted","data":"opaque-block","index":0}]);
    let call = |function: Value| json!([{"index":0,"id":"call_1","function":function}]);
    let mut response = Scripted::stream(MODEL, &["answer"], Duration::ZERO);
    let (events, _) = response.events.as_mut().unwrap();
    events.insert(
        0,
        json!({"choices":[{"delta":{"reasoning":"Thinking🙂","reasoning_details":hidden,
            "tool_calls":call(json!({"name":"shell","arguments":"{\"cmd\":"}))}}]})
        .to_string(),
    );
    events.insert(
        2,
        json!({"choices":[{"delta":{"reasoning":"Checking again.","tool_calls":call(json!({"arguments":"\"make\"}"}))}}]})
            .to_string(),
    );
    let gate = server.script_gated_stream(response);
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    let raw = |action: &str, payload: Value| {
        json!({
            "tool":"call","endpoint":"model","action":action,
            "path":MODEL.split('/').collect::<Vec<_>>(),"payload":payload
        })
    };
    scenario.add_agent(
        "agent",
        &fixture(json!([
            raw("start", json!({"messages":[{"role":"user","content":"think"}],"max_output_tokens":128})),
            raw("next", json!({"stream":"s1","wait_ms":2000})),
            {"tool":"await_operator","signal":"finish"},
            // Each pull returns what has arrived; three cover the two pieces and the end.
            raw("next", json!({"stream":"s1","wait_ms":2000})),
            raw("next", json!({"stream":"s1","wait_ms":2000})),
            raw("next", json!({"stream":"s1","wait_ms":2000}))
        ])),
        None,
    );
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), streaming_grants(1)).unwrap();
    let early = scenario.wait_transcript("agent", 2);
    let first = ok_value(&early[1]).unwrap();
    assert_eq!(first["reasoning_delta"], "Thinking🙂", "visible before the answer exists");
    assert_eq!(first["delta"], "");
    assert!(first["done"].is_null());
    assert_eq!(first["produced"], "Thinking🙂shell{\"cmd\":".len(), "a tool call in progress is not silence");
    gate.release();
    scenario.signal("agent", "finish");
    let lines = scenario.wait_done("agent");
    let (mut delta, mut reasoning) = (String::new(), String::new());
    let final_pull = lines[3..]
        .iter()
        .map(|line| ok_value(line).unwrap())
        .find(|pull| {
            delta.push_str(pull["delta"].as_str().unwrap_or_default());
            reasoning.push_str(pull["reasoning_delta"].as_str().unwrap_or_default());
            !pull["done"].is_null()
        })
        .expect("the stream ends within three pulls");
    assert_eq!((delta.as_str(), reasoning.as_str()), ("answer", "Checking again."));
    assert_eq!(final_pull["done"]["message"]["reasoning"], "Thinking🙂Checking again.");
    assert_eq!(final_pull["done"]["message"]["content"], "answer");
    assert_eq!(final_pull["done"]["message"]["reasoning_details"], hidden);
    assert_eq!(final_pull["done"]["message"]["tool_calls"][0]["arguments"], json!({"cmd": "make"}));
    assert_eq!(final_pull["produced"], "Thinking🙂Checking again.answershell{\"cmd\":\"make\"}".len());
    assert_eq!(server.received().len(), 1);
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn a_stream_cannot_be_pulled_by_another_process() {
    let server = FakeHttp::start();
    let pieces = ["private ", "to ", "its ", "owner"];
    server.script(Scripted::stream(MODEL, &pieces, Duration::from_millis(250)));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    scenario.add_agent(
        "agent",
        &fixture(
            json!([{"tool": "model.stream", "model": MODEL, "prompt": "mine", "max_output_tokens": 64}]),
        ),
        None,
    );
    // Holds the very same capabilities, knows the id (ids are sequential), and still gets nothing.
    scenario.add_agent(
        "thief",
        &fixture(json!([
            {"tool": "await_operator", "signal": "go"},
            {"tool": "model.next", "model": MODEL, "stream": "s1"},
            {"tool": "model.next", "model": MODEL, "stream": "s999"},
        ])),
        None,
    );

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("agent"), streaming_grants(5)).unwrap();
    supervisor.boot(program("thief"), streaming_grants(5)).unwrap();
    assert!(
        nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || !server.received().is_empty()),
        "the stream is in flight"
    );
    scenario.signal("thief", "go");

    let stolen = scenario.wait_done("thief");
    assert_eq!(error_code(&stolen[1]), Some("permission_denied"));
    assert_eq!(
        stolen[1]["result"]["error"], stolen[2]["result"]["error"],
        "a foreign stream and a missing one look the same"
    );
    let owner = scenario.wait_done("agent");
    assert_eq!(ok_value(&owner[0]).unwrap()["text"], pieces.concat(), "the owner lost nothing");
    assert!(supervisor.shutdown().is_clean());
}

// ---- concurrency: `complete` on bounded worker threads --------------------------

/// Eight callers, each with one call whose reply the server holds: all eight
/// reach the provider together, and each reply, once released, reaches its caller.
#[test]
fn completions_are_served_concurrently_and_each_reply_reaches_its_caller() {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &[], true);
    let (roles, held): (Vec<String>, Vec<_>) = (0..8)
        .map(|n| {
            let (role, prompt) = (format!("w{n}"), format!("prompt-{n}"));
            let (reply, held) = Scripted::json(200, &completion(MODEL, &format!("reply-{n}"))).held();
            server.script_for(&prompt, reply);
            scenario.add_agent(&role, &fixture(json!([ask(MODEL, &prompt)])), None);
            (role, held)
        })
        .unzip();

    // The kernel's own bound must allow it; the default of 4 is for the small tests.
    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    scenario.boot_service(&supervisor, "openrouter", "model");
    for role in &roles {
        supervisor.boot(program(role), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    }
    assert!(
        nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || server.received().len() == 8),
        "all eight in flight together: {} arrived",
        server.received().len()
    );
    held.iter().for_each(|held| held.release());
    for (n, role) in roles.iter().enumerate() {
        let lines = scenario.wait_done(role);
        let answer = ok_value(&lines[0]).unwrap_or_else(|| panic!("{role} failed: {:?}", lines[0]));
        assert_eq!(answer["message"]["content"], format!("reply-{n}"), "{role} got someone else's reply");
    }
    assert!(supervisor.shutdown().is_clean());
    assert!(!everything_observable(&scenario, &["w0", "w7"]).contains(KEY), "the key leaked");
}

#[test]
fn a_killed_callers_running_call_gives_up_its_slot() {
    let server = FakeHttp::start();
    // Never answered while the test runs: only the kill can free the slot.
    let (held_reply, _held) = Scripted::json(200, &completion(MODEL, "nobody wants this")).held();
    server.script_for("held", held_reply);
    server.script_for("next", Scripted::json(200, &completion(MODEL, "reply-next")));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--max-inflight", "1"], true);
    scenario.add_agent("held", &fixture(json!([ask(MODEL, "held")])), None);
    scenario.add_agent("next", &fixture(json!([ask(MODEL, "next")])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    let held = supervisor.boot(program("held"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    assert!(
        nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || !server.received().is_empty()),
        "the slot is held"
    );
    supervisor.boot(program("next"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    std::thread::sleep(Duration::from_millis(300)); // let `next` queue behind the held slot
    assert_eq!(server.received().len(), 1, "one slot: the second call waits");

    // Killing the caller cannot take the call back from the provider, but it must not park the slot.
    supervisor.terminate(held);
    let lines = scenario.wait_done("next");
    assert_eq!(ok_value(&lines[0]).unwrap()["message"]["content"], "reply-next", "{:?}", lines[0]);
    assert_eq!(server.received().len(), 2);
    assert!(supervisor.shutdown().is_clean());
}

/// `--max-inflight` bounds HTTP attempts, not calls: a stream waits for a
/// slot like a completion does, saying so with a queue lease that is not
/// model output, and a call in retry backoff is not using the provider and
/// gives its slot to the next caller in line.
#[test]
fn completions_and_streams_share_the_provider_slots_and_backoff_gives_the_slot_up() {
    let server = FakeHttp::start();
    // Flaky's first try holds the one slot until the test lets it fail with
    // an upstream error, which is retried after a backoff.
    let busy = Scripted::events(
        vec![
            json!({"id": "first try", "choices": []}).to_string(),
            json!({"error": {"code": 502, "message": "upstream busy"}}).to_string(),
        ],
        Duration::ZERO,
    );
    let first_try = server.script_gated_stream(busy);
    server.script(Scripted::json(200, &completion(MODEL, "recovered")));
    server.script_for("streamed", Scripted::stream(MODEL, &["after", " the wait"], Duration::ZERO));
    let mut scenario = Scenario::new();
    let args = ["--max-inflight", "1", "--retry-backoff-ms", "1500"];
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &args, true);
    scenario.add_agent("flaky", &fixture(json!([ask(MODEL, "flaky")])), None);
    let raw = |action: &str, payload: Value| {
        json!({
            "tool":"call","endpoint":"model","action":action,
            "path":MODEL.split('/').collect::<Vec<_>>(),"payload":payload
        })
    };
    let next = raw("next", json!({"stream":"s1","wait_ms":2000}));
    scenario.add_agent(
        "streamer",
        &fixture(json!([
            raw("start", json!({"messages":[{"role":"user","content":"streamed"}],"max_output_tokens":64})),
            next.clone(),
            {"tool":"await_operator","signal":"go"},
            next.clone(),
            next.clone(),
            next.clone(),
            next.clone(),
            next
        ])),
        None,
    );

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("flaky"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    assert!(
        nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || server.received().len() == 1),
        "flaky's first try holds the slot"
    );
    supervisor.boot(program("streamer"), streaming_grants(1)).unwrap();
    let queued = ok_value(&scenario.wait_transcript("streamer", 2)[1]).unwrap().clone();
    let lease = queued["queue"]["valid_for_ms"].as_u64();
    assert!(lease.is_some_and(|ms| ms > 0), "waiting for a slot: {queued}");
    assert_eq!((queued["produced"].as_u64(), queued["delta"].as_str()), (Some(0), Some("")));
    assert_eq!(server.received().len(), 1, "a stream waiting for a slot is not sent");
    first_try.release();
    scenario.signal("streamer", "go");

    let lines = scenario.wait_done("streamer");
    let pulls: Vec<&Value> = lines[3..].iter().filter_map(ok_value).collect();
    let done = pulls.iter().find(|pull| !pull["done"].is_null()).expect("the stream ended");
    assert_eq!(done["done"]["message"]["content"], "after the wait");
    assert!(done.get("queue").is_none(), "dispatched: no lease any more");
    assert_eq!(ok_value(&scenario.wait_done("flaky")[0]).unwrap()["message"]["content"], "recovered");
    // The stream was in line before flaky's backoff began, so it was sent
    // during the backoff; a completion that kept its slot through its
    // backoff would have sent its retry first.
    let order: Vec<&str> = server
        .received()
        .iter()
        .map(|r| if r.body.contains("streamed") { "streamed" } else { "flaky" })
        .collect();
    assert_eq!(order, ["flaky", "streamed", "flaky"]);
    assert!(supervisor.shutdown().is_clean());
}

/// The chat agent as a stream's consumer. While the provider holds its call
/// back for a free slot it shows the person a note, withdrawn once the call
/// is sent. A cancel that is not confirmed, here because the provider went
/// away, leaves the call's outcome unknown rather than failed.
#[test]
fn a_queued_chat_call_says_so_and_an_unconfirmed_cancel_is_an_unknown_outcome() {
    let server = FakeHttp::start();
    // The holder's completion keeps the one slot until the test lets it end.
    let holding = server.script_gated_stream(Scripted::stream(MODEL, &["held"], Duration::ZERO));
    let _gate = server.script_gated_stream(Scripted::stream(MODEL, &["partial", " never"], Duration::ZERO));
    let mut scenario = Scenario::new();
    let args = ["--max-inflight", "1", "--retries", "0"];
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &args, true);
    scenario.add_conversation("conversation");
    scenario.add_agent("holder", &fixture(json!([ask(MODEL, "hold the only slot")])), None);
    scenario.add_chat_agent("chat", MODEL, &["--task", "Say something."]);

    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    let openrouter = scenario.boot_service(&supervisor, "openrouter", "model");
    scenario.boot_service(&supervisor, "conversation", "conversation");
    supervisor.boot(program("holder"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    assert!(nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || server.received().len() == 1), "the slot is held");
    let mut grants = streaming_grants(1);
    grants.push(cap("conversation", &["post"], &["default", "default"]));
    supervisor.boot(program("chat"), grants).unwrap();

    let shown = || -> Vec<Value> {
        let log = scenario.conversation_log("default");
        log.into_iter().filter(|r| r["kind"] == "assistant_delta").map(|r| r["text"].clone()).collect()
    };
    // Nothing can be output while the slot is held: what is shown is the note.
    assert!(nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || !shown().is_empty()), "the chat shows it waits");
    assert_eq!(server.received().len(), 1, "the chat's call is held back for the slot");
    holding.release();
    assert!(
        nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || shown().contains(&json!("partial"))),
        "the call was sent after the wait"
    );
    supervisor.terminate(openrouter);
    assert!(nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, || {
        scenario.conversation_log("default").iter().any(|r| r["kind"] == "turn_complete")
    }));

    let log = scenario.conversation_log("default");
    let note = log.iter().find(|r| r["kind"] == "assistant_delta").unwrap();
    assert_ne!(note["text"], "partial", "the first thing shown is the note: {log:?}");
    assert!(
        log.iter()
            .any(|r| r["kind"] == "assistant" && r["message_id"] == note["message_id"] && r["text"] == ""),
        "the note was never withdrawn: {log:?}"
    );
    let error = log.iter().find(|r| r["kind"] == "error").unwrap();
    assert!(error["text"].as_str().unwrap().contains("UnknownOutcome"), "{error}");
    assert!(supervisor.shutdown().is_clean());
}

/// A stream that has delivered nothing is repeatable; one that has already
/// handed text or readable reasoning to its puller is not, because a second
/// attempt would send it again.
#[test]
fn a_stream_is_retried_before_its_first_byte_and_never_after() {
    let server = FakeHttp::start();
    let pieces = ["Second", " attempt", " wins."];
    let gap = Duration::from_millis(20);
    let thought = json!({"model": MODEL, "choices": [{"delta": {"reasoning": "half a thought"}}]});
    // (prompt, the first attempt, attempts made)
    let rows = [
        ("early", Scripted::json(503, &json!({"error": {"message": "upstream busy"}})), 2),
        // Text, then a body that ends mid-event.
        ("late", Scripted::cut_stream(MODEL, &["half a "], gap), 1),
        // Readable reasoning, then the body ends.
        ("thought", Scripted::events(vec![thought.to_string()], gap), 1),
    ];
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retry-backoff-ms", "100"], true);
    for (prompt, first, _) in &rows {
        server.script_for(prompt, first.clone());
        server.script_for(prompt, Scripted::stream(MODEL, &pieces, gap));
        let stream =
            json!({"tool": "model.stream", "model": MODEL, "prompt": prompt, "max_output_tokens": 64});
        scenario.add_agent(prompt, &fixture(json!([stream])), None);
    }

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    for (prompt, _, _) in &rows {
        supervisor.boot(program(prompt), streaming_grants(2)).unwrap();
    }
    for (prompt, _, attempts) in rows {
        let lines = scenario.wait_done(prompt);
        let answer = ok_value(&lines[0]);
        if attempts == 1 {
            assert!(
                answer.is_none(),
                "{prompt}: a stream that delivered output is not repeated: {:?}",
                lines[0]
            );
        } else {
            assert_eq!(answer.unwrap_or_else(|| panic!("{prompt}: {:?}", lines[0]))["text"], pieces.concat());
        }
        let made = server.received().iter().filter(|r| r.body.contains(prompt)).count();
        assert_eq!(made, attempts, "{prompt}");
    }
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn a_transient_provider_failure_is_retried_and_a_bad_request_is_not() {
    let server = FakeHttp::start();
    server.script_for("flaky", Scripted::json(503, &json!({"error": {"message": "upstream busy"}})));
    server.script_for("flaky", Scripted::json(200, &completion(MODEL, "recovered")));
    server.script_for("broken", Scripted::json(400, &json!({"error": {"message": "bad request"}})));
    let mut scenario = Scenario::new();
    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retry-backoff-ms", "100"], true);
    scenario.add_agent("flaky", &fixture(json!([ask(MODEL, "flaky")])), None);
    scenario.add_agent("broken", &fixture(json!([ask(MODEL, "broken")])), None);

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "openrouter", "model");
    supervisor.boot(program("flaky"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();
    supervisor.boot(program("broken"), vec![cap("model", &["complete"], &["deepseek"])]).unwrap();

    let lines = scenario.wait_done("flaky");
    assert_eq!(ok_value(&lines[0]).unwrap()["message"]["content"], "recovered", "{:?}", lines[0]);
    let lines = scenario.wait_done("broken");
    assert_eq!(error_code(&lines[0]), Some("invalid_input"), "{:?}", lines[0]);
    let received = server.received();
    assert_eq!(received.iter().filter(|r| r.body.contains("flaky")).count(), 2, "one retry, same body");
    assert_eq!(
        received.iter().filter(|r| r.body.contains("broken")).count(),
        1,
        "a bad request is not retried"
    );
    assert!(supervisor.shutdown().is_clean());
}
