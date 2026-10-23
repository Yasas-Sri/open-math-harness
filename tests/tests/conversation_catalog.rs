//! Durable session discovery through the real kernel and confined conversation
//! process. A stored conversation is independent of a running chat agent, is
//! keyed by its project and name, and catalog calls obey the same resource
//! capabilities as history calls.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::{Scenario, TIMEOUT, cap, endpoint, error_code, ok_value, path, program, sigkill};
use nucleus_kernel::SecurityEvent;
use nucleus_protocol::ErrorCode;
use serde_json::{Value, json};

fn call(action: &str, resource: &[&str], payload: Value) -> Value {
    json!({"tool": "call", "endpoint": "conversation", "action": action,
           "path": resource, "payload": payload})
}

fn fixture(calls: Vec<Value>) -> Value {
    json!({"turns": [{"tool_calls": calls}]})
}

/// A catalog page of `project/name` keys, as `list` answers it.
fn catalog(keys: &[&str], next_after: Option<&str>) -> Value {
    json!({"sessions": keys.iter().map(|key| session(key)).collect::<Vec<_>>(), "next_after": next_after})
}

fn session(key: &str) -> Value {
    let (project, name) = key.split_once('/').unwrap();
    json!({"project": project, "name": name})
}

/// What a `list` or an `ensure` answered.
fn named(line: &Value) -> Option<Value> {
    ok_value(line).cloned()
}

#[test]
fn byte_bounded_history_crosses_real_service_ipc_without_losing_its_provider_or_tail() {
    use nucleus_conversation_interface::{Entry, HISTORY_PAGE_BYTES, MAX_TEXT_BYTES};
    let mut scenario = Scenario::new();
    let entry = Entry::Assistant {
        text: "\0".repeat(MAX_TEXT_BYTES),
        message_id: None,
        images: Vec::new(),
        images_refused: Vec::new(),
    };
    entry.validate().unwrap();
    let mut original = String::new();
    for seq in 0..22 {
        let mut record = serde_json::to_value(&entry).unwrap();
        record["v"] = json!(1);
        record["seq"] = json!(seq);
        record["at_ms"] = json!(1);
        original.push_str(&record.to_string());
        original.push('\n');
    }
    assert!(
        original.len() > nucleus_protocol::MAX_FRAME_BYTES,
        "a single history response could not hold it"
    );
    scenario.write("sessions/default/large/log.jsonl", &original);
    scenario.add_conversation("conversation");
    // Each escaped record is about 384KiB: two fit the 1MiB target, a
    // third does not. Read all pages and explicitly observe EOF once more.
    let large = ["default", "large"];
    let mut calls = vec![call("history", &large, json!({}))];
    calls.extend((1..22).step_by(2).map(|seq| call("history", &large, json!({"after_seq":seq}))));
    calls.push(call("list", &[], json!({})));
    scenario.add_agent("reader", &fixture(calls), None);
    let mut supervisor = scenario.start();
    let provider = scenario.boot_service(&supervisor, "conversation", "conversation");
    let reader =
        supervisor.boot(program("reader"), vec![cap("conversation", &["history", "list"], &[])]).unwrap();
    let replies = scenario.wait_done_within("reader", std::time::Duration::from_secs(30));
    assert_eq!(replies.len(), 13);
    let mut seqs = Vec::new();
    for (index, reply) in replies[..11].iter().enumerate() {
        let payload = ok_value(reply).expect("every short page crosses the real SDK and kernel");
        assert!(serde_json::to_vec(payload).unwrap().len() <= HISTORY_PAGE_BYTES);
        assert_eq!(payload["has_more"], json!(index < 10));
        let records = payload["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        for record in records {
            assert_eq!(record["text"].as_str().unwrap(), "\0".repeat(MAX_TEXT_BYTES));
            seqs.push(record["seq"].as_u64().unwrap());
        }
    }
    assert_eq!(seqs, (0..22).collect::<Vec<u64>>());
    assert_eq!(ok_value(&replies[11]), Some(&json!({"records":[], "has_more":false})));
    assert_eq!(named(&replies[12]), Some(catalog(&["default/large"], None)));
    assert_eq!(supervisor.provider_of(&endpoint("conversation")).map(|(process, _)| process), Some(provider));
    assert!(supervisor.is_live(provider));
    assert_eq!(std::fs::read_to_string(scenario.host("sessions/default/large/log.jsonl")).unwrap(), original);
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(reader)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn empty_sessions_survive_provider_restart_and_catalog_authority_stays_scoped() {
    let mut scenario = Scenario::new();
    // An existing version-1 transcript is already a session, with no catalog
    // migration or rewriting needed. All files belong to this test's temp root.
    let existing =
        "{\"v\":1,\"seq\":0,\"at_ms\":1,\"kind\":\"user\",\"id\":\"u1\",\"text\":\"existing beta\"}\n";
    scenario.write("sessions/default/beta/log.jsonl", existing);
    scenario.add_conversation("conversation");
    scenario.add_agent(
        "writer",
        &fixture(vec![
            call("list", &[], json!({"after": null})),
            call("history", &["default", "missing"], json!({"after_seq": null})),
            call("ensure", &["default", "alpha"], json!({})),
            call("ensure", &["default", "alpha"], json!({})),
            // The same name in another project is another session.
            call("ensure", &["other", "alpha"], json!({})),
            call("history", &["default", "alpha"], json!({})),
            call("list", &[], json!({})),
            call("list", &["other"], json!({})),
            call("list", &["default", "alpha", "child"], json!({})),
            call("ensure", &[], json!({})),
            call("ensure", &["alpha"], json!({})),
            call("ensure", &["default", "alpha", "child"], json!({})),
        ]),
        None,
    );
    scenario.add_agent(
        "reader",
        &fixture(vec![
            call("list", &[], json!({})),
            call("history", &["default", "beta"], json!({})),
            call("history", &["default", "missing"], json!({})),
            call("list", &[], json!({"after": "default/alpha"})),
            call("list", &[], json!({"after": "other/alpha"})),
            call("ensure", &["default", "beta"], json!({})),
            call("history", &["default", "alpha"], json!({})),
        ]),
        None,
    );
    scenario.add_agent(
        "limited",
        &fixture(vec![
            call("list", &[], json!({})),
            call("list", &["default", "alpha"], json!({})),
            call("list", &["default", "alpha"], json!({"after": "default/alpha"})),
            call("ensure", &["default", "alpha"], json!({})),
            call("history", &["default", "alpha"], json!({})),
            call("list", &["default", "beta"], json!({})),
            call("ensure", &["default", "beta"], json!({})),
            call("history", &["default", "beta"], json!({})),
            call("ensure", &["other", "alpha"], json!({})),
        ]),
        None,
    );

    let mut supervisor = scenario.start();
    let original = scenario.boot_service(&supervisor, "conversation", "conversation");
    let writer = supervisor
        .boot(program("writer"), vec![cap("conversation", &["list", "ensure", "history"], &[])])
        .unwrap();
    let written = scenario.wait_done("writer");
    assert_eq!(written.len(), 12, "{written:?}");
    assert_eq!(named(&written[0]), Some(catalog(&["default/beta"], None)));
    assert_eq!(ok_value(&written[1]), Some(&json!({"records": [], "has_more": false})));
    for line in &written[2..4] {
        assert_eq!(named(line), Some(session("default/alpha")), "ensure is idempotent: {line}");
    }
    assert_eq!(named(&written[4]), Some(session("other/alpha")));
    assert_eq!(ok_value(&written[5]), Some(&json!({"records": [], "has_more": false})));
    assert_eq!(named(&written[6]), Some(catalog(&["default/alpha", "default/beta", "other/alpha"], None)));
    assert_eq!(named(&written[7]), Some(catalog(&["other/alpha"], None)));
    for line in &written[8..12] {
        assert_eq!(error_code(line), Some("invalid_input"), "invalid resource shape: {line}");
    }
    assert!(!scenario.host("sessions/default/missing").exists(), "history must not create a missing session");
    assert!(!scenario.host("sessions/default/alpha/child").exists());
    assert!(!scenario.host("sessions/alpha").exists());
    assert_eq!(std::fs::read(scenario.host("sessions/default/alpha/log.jsonl")).unwrap(), b"");
    assert_eq!(std::fs::read_to_string(scenario.host("sessions/default/beta/log.jsonl")).unwrap(), existing);
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(writer)));

    // Abruptly replace the actual provider, losing every in-memory session.
    // A fresh caller gets the replacement generation through ordinary boot.
    sigkill(supervisor.host_pid(original).expect("the conversation process is running"));
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(original)));
    let replacement = scenario.boot_service(&supervisor, "conversation", "conversation");
    assert_ne!(replacement, original);
    assert_eq!(
        supervisor
            .provider_of(&endpoint("conversation"))
            .map(|(process, generation)| (process, generation.0)),
        Some((replacement, 2))
    );
    let reader = supervisor
        .boot(program("reader"), vec![cap("conversation", &["list", "ensure", "history"], &[])])
        .unwrap();
    let recovered = scenario.wait_done("reader");
    assert_eq!(recovered.len(), 7, "{recovered:?}");
    assert_eq!(named(&recovered[0]), Some(catalog(&["default/alpha", "default/beta", "other/alpha"], None)));
    assert_eq!(
        ok_value(&recovered[1]),
        Some(&json!({"records": [serde_json::from_str::<Value>(existing).unwrap()], "has_more": false}))
    );
    assert_eq!(ok_value(&recovered[2]), Some(&json!({"records": [], "has_more": false})));
    assert_eq!(named(&recovered[3]), Some(catalog(&["default/beta", "other/alpha"], None)));
    assert_eq!(named(&recovered[4]), Some(catalog(&[], None)));
    assert_eq!(named(&recovered[5]), Some(session("default/beta")));
    assert_eq!(ok_value(&recovered[6]), Some(&json!({"records": [], "has_more": false})));
    assert!(!scenario.host("sessions/default/missing").exists());
    assert_eq!(std::fs::read(scenario.host("sessions/default/alpha/log.jsonl")).unwrap(), b"");
    assert_eq!(std::fs::read_to_string(scenario.host("sessions/default/beta/log.jsonl")).unwrap(), existing);

    let limited = supervisor
        .boot(
            program("limited"),
            vec![cap("conversation", &["list", "ensure", "history"], &["default", "alpha"])],
        )
        .unwrap();
    let scoped = scenario.wait_done("limited");
    assert_eq!(scoped.len(), 9, "{scoped:?}");
    for index in [0, 5, 6, 7, 8] {
        assert_eq!(error_code(&scoped[index]), Some("permission_denied"), "{:?}", scoped[index]);
    }
    assert_eq!(named(&scoped[1]), Some(catalog(&["default/alpha"], None)));
    assert_eq!(named(&scoped[2]), Some(catalog(&[], None)));
    assert_eq!(named(&scoped[3]), Some(session("default/alpha")));
    assert_eq!(ok_value(&scoped[4]), Some(&json!({"records": [], "has_more": false})));

    let events = scenario.events();
    for (action_name, resource) in [
        ("list", vec![]),
        ("list", vec!["default", "beta"]),
        ("ensure", vec!["default", "beta"]),
        ("history", vec!["default", "beta"]),
        ("ensure", vec!["other", "alpha"]),
    ] {
        assert!(
            events.iter().any(|event| matches!(
                event,
                SecurityEvent::Denied {
                    process, endpoint: Some(target), action: Some(action), resource: Some(denied_path),
                    reason: ErrorCode::PermissionDenied,
                } if *process == limited && *target == endpoint("conversation")
                    && action.as_str() == action_name && *denied_path == path(&resource)
            )),
            "the real kernel must reject {action_name} {resource:?} before dispatch: {events:?}"
        );
    }
    assert!(
        !events.iter().any(|event| matches!(event, SecurityEvent::ProcessStarted { parent: Some(_), .. })),
        "catalog creation must not start an agent: {events:?}"
    );
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(reader) && !s.is_live(limited)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn catalog_pages_have_a_stable_lexical_cursor_and_a_500_session_bound() {
    let mut scenario = Scenario::new();
    // Pages run across projects, in order of project, then name.
    let keys: Vec<String> = (0..502)
        .map(|number| format!("{}/session-{number:04}", if number < 300 { "default" } else { "second" }))
        .collect();
    // Creation order deliberately differs from lexical order. Empty logs are
    // already durable sessions; listing must not depend on loading history.
    for key in keys.iter().rev() {
        scenario.write(&format!("sessions/{key}/log.jsonl"), "");
    }
    scenario.add_conversation("conversation");
    scenario.add_agent(
        "reader",
        &fixture(vec![
            call("list", &[], json!({})),
            call("list", &[], json!({"after": null})),
            call("list", &[], json!({"after": "second/session-0499"})),
            call("list", &[], json!({"after": "second/session-0501"})),
        ]),
        None,
    );
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let reader = supervisor.boot(program("reader"), vec![cap("conversation", &["list"], &[])]).unwrap();
    let pages = scenario.wait_done("reader");
    assert_eq!(pages.len(), 4, "{pages:?}");
    let first_keys: Vec<&str> = keys[..500].iter().map(String::as_str).collect();
    let first_page = Some(catalog(&first_keys, Some("second/session-0499")));
    assert_eq!(named(&pages[0]), first_page);
    assert_eq!(named(&pages[1]), first_page, "omitted and null cursors are equivalent");
    assert_eq!(named(&pages[2]), Some(catalog(&["second/session-0500", "second/session-0501"], None)));
    assert_eq!(named(&pages[3]), Some(catalog(&[], None)));
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(reader)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}
