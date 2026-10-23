//! A process starts a real confined grandchild from an executable in its own
//! namespace, with a subset of its own authority, and the kernel refuses a
//! request wider than the parent. Uses real confined processes: the fixture
//! agent starts a second copy of itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::{Scenario, TIMEOUT, cap, error_code, ok_value, program};
use nucleus_kernel::SecurityEvent;
use serde_json::{Value, json};

fn fixture(calls: Value) -> Value {
    json!({ "turns": [{ "tool_calls": calls }] })
}

#[test]
fn a_process_starts_a_confined_grandchild_from_an_executable() {
    let mut scenario = Scenario::new();
    scenario.write("data/work/notes.txt", "grandchild here");
    scenario.add_fs("fs", "data", "fs");

    // The grandchild: a second fixture agent that reads one file through the
    // capability its parent delegated, then exits.
    scenario.add_agent(
        "child",
        &fixture(json!([{ "tool": "fs.read", "path": ["work", "notes.txt"] }])),
        None,
    );

    // The parent starts the grandchild from the fixture-agent executable, giving
    // it a subset of the parent's own namespace and fs authority; then tries a
    // second start that reaches for a directory the parent cannot see.
    let exe = Scenario::fixture_executable_guest();
    scenario.add_agent(
        "parent",
        &fixture(json!([
            {
                "tool": "spawn_executable",
                "executable": exe.clone(),
                "args": ["--fixture", "/fixture/child.json", "--transcript", "/out/child.jsonl"],
                "binds": [{ "path": "/fixture" }, { "path": "/out", "writable": true }],
                "grants": [{ "endpoint": "fs", "actions": ["read"], "path": ["work"] }]
            },
            { "tool": "wait_child" },
            {
                "tool": "spawn_executable",
                "executable": exe.clone(),
                "args": ["--fixture", "/fixture/child.json", "--transcript", "/out/evil.jsonl"],
                "binds": [{ "path": "/etc" }]
            }
        ])),
        None,
    );
    let bound = scenario.bind_programs("parent");
    assert_eq!(bound, exe, "the executable guest path is stable");

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "fs", "fs");
    let parent = supervisor
        .boot(
            program("parent"),
            vec![cap("fs", &["read"], &["work"]), cap("kernel", &["spawn_executable"], &["executable"])],
        )
        .unwrap();

    let lines = scenario.wait_done("parent");
    assert_eq!(lines.len(), 3, "{lines:?}");
    let started = ok_value(&lines[0]).expect("the grandchild starts");
    let grandchild = started["child"].as_u64().expect("a child id");
    assert!(ok_value(&lines[1]).is_some(), "the grandchild exits and the parent is told: {:?}", lines[1]);
    assert_eq!(
        error_code(&lines[2]),
        Some("permission_denied"),
        "a bind the parent cannot see is refused: {:?}",
        lines[2]
    );

    // The grandchild really ran, confined, and used only the delegated capability.
    let child_log: Vec<Value> = std::fs::read_to_string(scenario.out_dir("parent").join("child.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let read = child_log.iter().find(|l| l["call"]["tool"] == "fs.read").expect("the read is in the log");
    assert_eq!(ok_value(read).unwrap()["content"], "grandchild here");

    // It is the parent's child in the kernel's eyes, started from the executable.
    let events = scenario.events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SecurityEvent::ProcessStarted { process, parent: Some(p), program, .. }
                if process.0 == grandchild && *p == parent && program.contains("fixture")
        )),
        "grandchild attributed to the parent: {events:?}"
    );
    // The refused start created nothing: the parent has exactly one child.
    assert_eq!(scenario.children_of(parent).len(), 1, "the refused spawn started nothing");
    // The refused grandchild wrote nothing.
    assert!(!scenario.out_dir("parent").join("evil.jsonl").exists());

    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(parent)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}
