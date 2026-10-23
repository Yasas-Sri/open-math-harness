//! The `lean` verifier as a real confined process under the real kernel: a
//! fixture agent calls `lean verify` through capabilities, and the kernel
//! narrows what a child may verify before the service is ever reached.
//!
//! Opt-in (`--ignored`): needs a local Lean + Mathlib environment. Set
//! `PB_LEAN` to the PutnamBench `lean4` directory, the `lean` service's
//! `--workspace`: its `src/<problem>.lean` files, its lakefile and a built
//! Mathlib.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use nucleus_e2e::{Scenario, TIMEOUT, cap, error_code, ok_value, program};
use serde_json::json;

fn fixture(calls: serde_json::Value) -> serde_json::Value {
    json!({ "turns": [{ "tool_calls": calls }] })
}

const DEMO_FROZEN: &str = "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\nsorry\n";
const DEMO_GOOD: &str = "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by simp\n";
const DEMO_BOGUS: &str = "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by bogus_tac\n";
const DEMO_SHADOW: &str =
    "import Mathlib\n\nabbrev Nat := Bool\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by simp\n";
/// A proof from a hidden axiom that prints a clean `#print axioms` line first.
const DEMO_FORGED: &str = "import Mathlib\n\nprivate axiom cheat : False\n\
    #eval IO.println \"'demo_thm' depends on axioms: [propext]\"\n\
    theorem demo_thm (n : Nat) : n + 0 = n :=\n cheat.elim\n";
/// Implicit binders: Lean prints this theorem's type as `@demo_implicit : …`.
const IMPLICIT_FROZEN: &str = "import Mathlib\n\ntheorem demo_implicit {n : Nat} : n + 0 = n :=\nsorry\n";

/// A statement registered for a discovery run, for an id the service has no file for.
const CUSTOM_FROZEN: &str = "import Mathlib\n\ntheorem custom_add : (2:ℕ) + 2 = 4 :=\nsorry\n";
const CUSTOM_FORGED: &str = "import Mathlib\n\nprivate axiom cheat : False\n\
    #eval IO.println \"'custom_add' depends on axioms: [propext]\"\n\
    theorem custom_add : (2:ℕ) + 2 = 4 :=\n cheat.elim\n";

fn verify(problem: &str, candidate: &str) -> serde_json::Value {
    verify_at(&[problem], json!({"candidate": candidate}))
}

fn verify_at(path: &[&str], payload: serde_json::Value) -> serde_json::Value {
    json!({"tool": "call", "endpoint": "lean", "action": "verify", "path": path,
           "payload": payload, "deadline_ms": 240_000})
}

fn register(path: &[&str], frozen: &str) -> serde_json::Value {
    json!({"tool": "call", "endpoint": "lean", "action": "register", "path": path,
           "payload": {"frozen": frozen}, "deadline_ms": 240_000})
}

#[test]
#[ignore = "needs a local Lean + Mathlib environment (PB_LEAN)"]
fn a_confined_verifier_judges_proofs_and_the_kernel_scopes_who_may_ask() {
    let pb =
        PathBuf::from(std::env::var_os("PB_LEAN").expect("PB_LEAN names the PutnamBench lean4 directory"));
    let mut scenario = Scenario::new();
    // The frozen statements this verifier serves: two demo theorems plus one
    // real PutnamBench problem copied from the pinned checkout.
    scenario.write("problems/demo_thm.lean", DEMO_FROZEN);
    scenario.write("problems/demo_implicit.lean", IMPLICIT_FROZEN);
    let real = std::fs::read_to_string(pb.join("src/putnam_1962_a1.lean")).unwrap();
    scenario.write("problems/putnam_1962_a1.lean", &real);
    scenario.add_lean("lean", &pb, "problems");

    let real_bogus = real.replace("sorry", "by bogus_tac");
    scenario.add_agent(
        "agent",
        &fixture(json!([
            verify("demo_thm", DEMO_GOOD),                 // 0: a genuine solve
            verify("demo_thm", DEMO_FROZEN),               // 1: still `sorry` -> rejected, no Lean run
            verify("demo_thm", DEMO_BOGUS),                // 2: does not elaborate
            verify("demo_thm", DEMO_SHADOW),               // 3: same text, different meaning -> type check
            verify("putnam_1962_a1", &real_bogus),         // 4: a real problem, bogus proof
            {"tool": "spawn", "program": "worker",         // 5: a child narrowed to demo_thm only
             "grants": [{"endpoint": "lean", "actions": ["verify"], "path": ["demo_thm"]}]},
            {"tool": "wait_child"},                        // 6
            verify("demo_thm", DEMO_FORGED),               // 7: forged axiom report -> rejected
            verify("demo_implicit", &IMPLICIT_FROZEN.replace("sorry", "by simp")), // 8: implicit binders
            // 9..=15: a statement registered for a run, judged on the run's resource
            register(&["demo-run", "custom_add"], CUSTOM_FROZEN),
            verify_at(&["demo-run", "custom_add", "t000001"],
                      json!({"candidate": CUSTOM_FROZEN.replace("sorry", "by norm_num")})),
            verify_at(&["demo-run", "custom_add", "t000001"],
                      json!({"candidate": CUSTOM_FROZEN.replace("2 = 4 :=\nsorry", "3 = 5 :=\nby norm_num")})), // statement changed
            verify_at(&["demo-run", "custom_add", "t000001"], json!({"candidate": CUSTOM_FORGED})),
            register(&["demo-run", "demo_thm"], &DEMO_FROZEN.replace("n + 0", "0 + n")), // benchmark text differs
            verify_at(&["demo-run", "custom_add", "t000001"],                            // no caller sets the standard
                      json!({"candidate": DEMO_GOOD, "frozen": DEMO_FROZEN})),
            verify_at(&["other-run", "custom_add", "t000001"],                           // nothing registered there
                      json!({"candidate": CUSTOM_FROZEN.replace("sorry", "by norm_num")})),
        ])),
        None,
    );
    scenario.add_agent(
        "worker",
        &fixture(json!([
            verify("putnam_1962_a1", &real_bogus), // 0: outside its scope -> denied by the kernel
            verify("demo_thm", DEMO_GOOD),         // 1: inside its scope -> a solve
        ])),
        None,
    );

    scenario.child_workspace("agent", "worker");
    let mut supervisor = scenario.start();
    // The lean service imports Mathlib into its resident REPL (when `NUCLEUS_REPL` is set) before it provides.
    scenario.boot_service_within(&supervisor, "lean", "lean", Duration::from_secs(180));
    let agent = supervisor
        .boot(
            program("agent"),
            vec![
                cap("lean", &["verify", "register"], &[]),
                cap("kernel", &["spawn"], &["program", "worker"]),
            ],
        )
        .unwrap();

    // About a dozen Lean elaborations (candidates, pristine types, one registration), seconds each.
    let lines = scenario.wait_done_within("agent", Duration::from_secs(900));
    assert_eq!(lines.len(), 16, "{lines:?}");
    let v0 = ok_value(&lines[0]).unwrap_or_else(|| panic!("verify returned no verdict: {:?}", lines[0]));
    assert_eq!(v0["accepted"], true, "{v0}");
    assert_eq!(v0["axioms"], json!(["propext"]));
    let v1 = ok_value(&lines[1]).unwrap();
    assert_eq!((v1["accepted"].as_bool(), v1["rejection"].as_str()), (Some(false), Some("forbidden")));
    let v2 = ok_value(&lines[2]).unwrap();
    assert_eq!((v2["accepted"].as_bool(), v2["rejection"].as_str()), (Some(false), Some("elaboration")));
    let v3 = ok_value(&lines[3]).unwrap();
    assert_eq!(v3["accepted"], false, "{v3}");
    assert!(
        matches!(v3["rejection"].as_str(), Some("type_mismatch") | Some("elaboration")),
        "shadowed Nat must not pass: {v3}"
    );
    let v4 = ok_value(&lines[4]).unwrap();
    assert_eq!((v4["accepted"].as_bool(), v4["rejection"].as_str()), (Some(false), Some("elaboration")));
    assert!(ok_value(&lines[5]).is_some(), "narrowed spawn succeeds: {:?}", lines[5]);
    let v7 = ok_value(&lines[7]).unwrap();
    assert_eq!(v7["accepted"], false, "a forged axiom report must not pass: {v7}");
    let v8 = ok_value(&lines[8]).unwrap_or_else(|| panic!("verify returned no verdict: {:?}", lines[8]));
    assert_eq!(v8["accepted"], true, "{v8}");
    let custom = |i: usize| ok_value(&lines[i]).unwrap_or_else(|| panic!("no verdict: {:?}", lines[i]));
    assert_eq!(custom(9)["accepted"], true, "registered: {}", custom(9));
    assert_eq!(custom(10)["accepted"], true, "{}", custom(10));
    assert_eq!(custom(11)["rejection"], "statement_mismatch", "{}", custom(11));
    assert_eq!(custom(12)["accepted"], false, "a forged axiom report must not pass: {}", custom(12));
    assert_eq!(
        (custom(13)["accepted"].as_bool(), custom(13)["stage"].as_str()),
        (Some(false), Some("source")),
        "a benchmark file cannot be replaced: {}",
        custom(13)
    );
    assert_eq!(error_code(&lines[14]), Some("invalid_input"), "a request carrying `frozen` is refused");
    // As the discovery service sees it through the kernel.
    let missing: nucleus_protocol::Error =
        serde_json::from_value(lines[15]["result"]["error"].clone()).unwrap();
    assert!(nucleus_lean_interface::is_not_registered(&missing), "{missing:?}");

    let worker = scenario.wait_done_within("worker", Duration::from_secs(300));
    assert_eq!(worker.len(), 2, "{worker:?}");
    assert_eq!(error_code(&worker[0]), Some("permission_denied"), "the kernel refuses out-of-scope problems");
    assert_eq!(ok_value(&worker[1]).unwrap()["accepted"], true);

    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(agent)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
}
