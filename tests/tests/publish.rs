//! The scan of a session's records, end to end with real confined processes:
//! the conversation service, the publish service holding nothing but
//! `conversation history`, and a caller in the place of the platform's mirror.
//! The findings come by `seq`, the secrets masked and named only by their
//! salted fingerprints.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::{Scenario, TIMEOUT, cap, endpoint, error_code, ok_value, program};
use nucleus_kernel::CapSpec;
use serde_json::{Value, json};

const TOOL_KEY: &str = "sk-proj-Ab12Cd34Ef56Gh78Ij90Kl";
const SALT: &str = "00112233445566778899aabbccddeeff";

fn fixed(endpoint: &str, actions: &[&str], resource: &[&str]) -> CapSpec {
    CapSpec { delegable: false, ..cap(endpoint, actions, resource) }
}

/// One turn of tool calls on `endpoint` `action` in session `s1` of the
/// default project.
fn calls(endpoint: &str, action: &str, payloads: &[Value]) -> Value {
    let calls: Vec<Value> = payloads
        .iter()
        .map(|payload| json!({"tool": "call", "endpoint": endpoint, "action": action, "path": ["default", "s1"], "payload": payload}))
        .collect();
    json!({"turns": [{"tool_calls": calls}]})
}

#[test]
fn the_scan_names_a_sessions_secrets_by_seq_and_never_the_secret() {
    let mut scenario = Scenario::new();
    let summary = format!("HOME=/home/default\nOPENAI_API_KEY={TOOL_KEY}\nLANG=C\n");
    scenario.add_agent(
        "agent",
        &calls(
            "conversation",
            "post",
            &[
                json!({"kind": "tool_call", "call_id": "c1", "name": "shell_exec", "arguments": {"command": "printenv"}}),
                json!({"kind": "tool_result", "call_id": "c1", "error_code": null, "summary": summary}),
                json!({"kind": "turn_complete"}),
            ],
        ),
        None,
    );
    scenario.add_agent(
        "mirror",
        &calls("publish", "findings", &[json!({"salt": SALT}), json!({"salt": SALT, "after": 2})]),
        None,
    );
    scenario.add_conversation("conversation");
    scenario.add_publish("publish");
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "conversation", "conversation");
    // The publish service's whole authority (the VM's composition gives the same).
    supervisor
        .boot(
            program("publish"),
            vec![
                fixed("kernel", &["provide"], &["endpoint", "publish"]),
                fixed("conversation", &["history"], &[]),
            ],
        )
        .unwrap();
    let target = endpoint("publish");
    assert!(supervisor.wait_until(TIMEOUT, |k| k.provider_of(&target).is_some()));
    supervisor.boot(program("agent"), vec![fixed("conversation", &["post"], &["default"])]).unwrap();
    let posted = scenario.wait_done("agent");
    assert!(posted.iter().all(|line| error_code(line).is_none()), "{posted:?}");
    supervisor.boot(program("mirror"), vec![fixed("publish", &["findings"], &[])]).unwrap();
    let read = scenario.wait_done("mirror");
    let first = ok_value(&read[0]).unwrap_or_else(|| panic!("{:?}", read[0])).clone();
    let text = first.to_string();
    assert!(!text.contains(TOOL_KEY), "{text}");
    let seqs: Vec<&Value> = first["records"].as_array().unwrap().iter().map(|r| &r["seq"]).collect();
    assert_eq!(seqs, [&json!(1)], "{first}");
    assert_eq!((&first["through"], &first.get("next")), (&json!(2), &None), "{first}");
    assert_eq!(first["records"][0]["findings"][0]["fingerprint"].as_str().unwrap().len(), 64);
    let rest = ok_value(&read[1]).unwrap_or_else(|| panic!("{:?}", read[1])).clone();
    assert_eq!(rest["records"], json!([]), "{rest}");
    assert!(supervisor.shutdown().is_clean());
}
