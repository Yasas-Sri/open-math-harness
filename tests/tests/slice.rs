//! Acceptance scenarios with real confined processes: delegation and denial
//! before execution, crash containment and stale handles, a terminated
//! subtree's release, confinement and protocol violations, confinement that
//! fails closed, a process that stops reading, and a child refused a host
//! directory its parent was never given. Each test
//! owns a private directory and supervisor, so they run in parallel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_e2e::{
    Scenario, TIMEOUT, agent_grants, endpoint, error_code, mentions, ok_value, program, wait_for,
};
use nucleus_e2e::{pid_gone, sigkill};
use nucleus_kernel::SecurityEvent;
use nucleus_platform::{ProgramSpec, StartError};
use nucleus_protocol::ErrorCode;
use serde_json::json;

fn fixture(calls: serde_json::Value) -> serde_json::Value {
    json!({ "turns": [{ "tool_calls": calls }] })
}

#[test]
fn delegation_and_denial_before_execution() {
    let mut scenario = Scenario::new();
    scenario.write("data/work/sub/hello.txt", "hello from sub");
    scenario.write("data/work/top.txt", "top");
    scenario.write("data/secret.txt", "secret");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_agent(
        "agent",
        &fixture(json!([
            {"tool": "fs.read", "path": ["work", "sub", "hello.txt"]},
            {"tool": "fs.read", "path": ["secret.txt"]},
            {"tool": "spawn", "program": "worker",
             "grants": [{"endpoint": "fs", "actions": ["read"], "path": ["work", "sub"]}]},
            {"tool": "wait_child"},
            {"tool": "spawn", "program": "worker",
             "grants": [{"endpoint": "fs", "actions": ["read", "remove"], "path": ["work"]}]},
            {"tool": "spawn", "program": "worker",
             "grants": [{"endpoint": "fs", "actions": ["read"], "path": []}]},
            {"tool": "fs.create", "path": ["work", "report.txt"], "content": "done"},
            {"tool": "fs.list", "path": ["work"]},
        ])),
        None,
    );
    // Files that exist on the host but were never bound into the worker's sandbox.
    let (unbound_secret, unbound_top) =
        (scenario.host("data/secret.txt"), scenario.host("data/work/top.txt"));
    scenario.add_agent(
        "worker",
        &fixture(json!([
            {"tool": "fs.read", "path": ["work", "sub", "hello.txt"]},
            {"tool": "fs.read", "path": ["work", "top.txt"]},
            {"tool": "fs.create", "path": ["work", "sub", "x.txt"], "content": "planted"},
            {"tool": "fs.read", "path": ["work", "sub", "hello.txt"], "handle": 1},
            {"tool": "host.read", "path": unbound_secret.to_string_lossy()},
            {"tool": "host.read", "path": unbound_top.to_string_lossy()},
        ])),
        None,
    );

    scenario.child_workspace("agent", "worker");

    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "fs", "fs");
    let agent = supervisor
        .boot(program("agent"), agent_grants("fs", &["read", "list", "create"], &["work"]))
        .unwrap();

    let lines = scenario.wait_done("agent");
    assert_eq!(lines.len(), 8, "{lines:?}");
    assert_eq!(ok_value(&lines[0]).unwrap()["content"], "hello from sub");
    assert_eq!(error_code(&lines[1]), Some("permission_denied"));
    assert!(ok_value(&lines[2]).is_some(), "narrowed spawn succeeds: {:?}", lines[2]);
    assert!(ok_value(&lines[3]).is_some());
    assert_eq!(error_code(&lines[4]), Some("permission_denied"), "widened actions");
    assert_eq!(error_code(&lines[5]), Some("permission_denied"), "widened scope");
    assert!(ok_value(&lines[6]).is_some());
    assert_eq!(std::fs::read_to_string(scenario.host("data/work/report.txt")).unwrap(), "done");
    let listing = lines
        .iter()
        .find(|line| line["call"]["tool"] == "fs.list")
        .expect("the list call is in the transcript");
    let listed = ok_value(listing).expect("listing succeeds on every host");
    assert_eq!(listed["entries"], json!(["report.txt", "sub", "top.txt"]));

    let worker_lines = scenario.wait_done("worker");
    assert_eq!(worker_lines.len(), 6, "{worker_lines:?}");
    assert_eq!(ok_value(&worker_lines[0]).unwrap()["content"], "hello from sub");
    assert_eq!(error_code(&worker_lines[1]), Some("permission_denied"));
    assert_eq!(error_code(&worker_lines[2]), Some("permission_denied"));
    assert_eq!(
        error_code(&worker_lines[3]),
        Some("unknown_handle"),
        "the parent's handle numbers mean nothing here"
    );
    assert!(error_code(&worker_lines[4]).is_some(), "the sandbox has no /etc");
    assert!(error_code(&worker_lines[5]).is_some(), "the worker's sandbox has no /data");

    // Denied before execution: the service never received those requests.
    let log = scenario.access_log("data");
    for forbidden in ["secret.txt", "top.txt", "x.txt"] {
        assert!(!log.iter().any(|l| mentions(l, forbidden)), "fs saw {forbidden}: {log:?}");
    }
    assert_eq!(log.iter().filter(|l| mentions(l, "hello.txt")).count(), 2);
    assert!(!scenario.host("data/work/sub/x.txt").exists());

    let workers = scenario.children_of(agent);
    assert_eq!(workers.len(), 1, "rejected spawns start nothing");
    let events = scenario.events();
    assert!(events.iter().any(|e| matches!(
        e,
        SecurityEvent::Denied { process, reason: ErrorCode::PermissionDenied, .. } if *process == workers[0]
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        SecurityEvent::Delegated { from, to, .. } if *from == agent && *to == workers[0]
    )));

    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(agent)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(supervisor.live_count(), 0);
}

#[test]
fn service_crash_is_contained_and_handles_go_stale() {
    let mut scenario = Scenario::new();
    for data in ["data1", "data2"] {
        scenario.mkfifo(&format!("{data}/work/pipe"));
        scenario.write(&format!("{data}/work/a.txt"), "a");
    }
    scenario.add_fs("fs", "data1", "fs");
    scenario.add_fs("fs2", "data2", "fs2");
    scenario.add_agent(
        "agent",
        &fixture(json!([
            {"tool": "fs.read", "path": ["work", "pipe"], "deadline_ms": 60000},
            {"tool": "await_operator", "signal": "replaced"},
            {"tool": "fs.read", "path": ["work", "a.txt"]},
            {"tool": "rebind", "endpoint": "fs"},
            {"tool": "fs.read", "path": ["work", "a.txt"]},
        ])),
        None,
    );
    scenario.add_agent(
        "bystander",
        &fixture(json!([{"tool": "fs.read", "path": ["work", "pipe"], "deadline_ms": 60000}])),
        Some("fs2"),
    );

    let mut supervisor = scenario.start();
    let fs = scenario.boot_service(&supervisor, "fs", "fs");
    let fs2 = scenario.boot_service(&supervisor, "fs2", "fs2");
    let agent = supervisor.boot(program("agent"), agent_grants("fs", &["read"], &["work"])).unwrap();
    let bystander = supervisor.boot(program("bystander"), agent_grants("fs2", &["read"], &["work"])).unwrap();
    scenario.wait_access("data1", "pipe");
    scenario.wait_access("data2", "pipe");

    // Crash the first service while a call is in flight inside it.
    sigkill(supervisor.host_pid(fs).expect("fs is running"));
    let lines = scenario.wait_transcript("agent", 1);
    assert_eq!(error_code(&lines[0]), Some("unavailable"), "an interrupted observation did nothing");
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(fs)));
    assert!(supervisor.is_live(agent) && supervisor.is_live(bystander) && supervisor.is_live(fs2));
    assert!(scenario.transcript("bystander").is_empty(), "the unrelated call is still in flight");

    // The unrelated in-flight call completes normally.
    scenario.feed_fifo("data2/work/pipe", "hello");
    let lines = scenario.wait_done("bystander");
    assert_eq!(ok_value(&lines[0]).unwrap()["content"], "hello");

    // Replacement: new generation, old handle stale until explicitly rebound.
    let replacement = scenario.boot_service(&supervisor, "fs", "fs");
    assert_eq!(supervisor.provider_of(&endpoint("fs")).map(|(p, g)| (p, g.0)), Some((replacement, 2)));
    scenario.signal("agent", "replaced");
    let lines = scenario.wait_done("agent");
    assert_eq!(error_code(&lines[2]), Some("stale_handle"));
    assert_eq!(ok_value(&lines[3]).unwrap()["generation"], 2);
    assert_eq!(ok_value(&lines[4]).unwrap()["content"], "a");

    // Nothing was replayed to the replacement: it saw only the final read.
    let log = scenario.access_log("data1");
    assert_eq!(log.iter().filter(|l| mentions(l, "pipe")).count(), 1, "{log:?}");
    assert_eq!(log.iter().filter(|l| mentions(l, "a.txt")).count(), 1, "{log:?}");

    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn terminating_an_agent_releases_its_subtree_only() {
    let mut scenario = Scenario::new();
    scenario.mkfifo("data/work/pipe");
    scenario.write("data/work/a.txt", "a");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_agent(
        "agent",
        &fixture(json!([
            {"tool": "spawn", "program": "worker",
             "grants": [{"endpoint": "fs", "actions": ["read"], "path": ["work"]}]},
            {"tool": "wait_child"},
        ])),
        None,
    );
    scenario.add_agent(
        "worker",
        &fixture(json!([{"tool": "fs.read", "path": ["work", "pipe"], "deadline_ms": 60000}])),
        None,
    );
    scenario.add_agent(
        "bystander",
        &fixture(json!([
            {"tool": "await_operator", "signal": "go"},
            {"tool": "fs.read", "path": ["work", "a.txt"]},
        ])),
        None,
    );

    scenario.child_workspace("agent", "worker");

    let mut supervisor = scenario.start();
    let fs = scenario.boot_service(&supervisor, "fs", "fs");
    let agent = supervisor.boot(program("agent"), agent_grants("fs", &["read"], &["work"])).unwrap();
    let bystander = supervisor.boot(program("bystander"), agent_grants("fs", &["read"], &["work"])).unwrap();
    scenario.wait_access("data", "pipe");
    let worker = scenario.children_of(agent)[0];

    let pid = |process| supervisor.host_pid(process).expect("running");
    let (agent_pid, worker_pid, fs_pid, bystander_pid) = (pid(agent), pid(worker), pid(fs), pid(bystander));

    supervisor.terminate(agent);
    assert!(!supervisor.is_live(agent) && !supervisor.is_live(worker), "authority is withdrawn at once");
    // Terminate interrupts the SDK wait, so both parent and worker exit gracefully.
    assert!(wait_for(TIMEOUT, || pid_gone(agent_pid) && pid_gone(worker_pid)));
    assert!(supervisor.is_live(fs) && supervisor.is_live(bystander));
    assert!(!pid_gone(fs_pid) && !pid_gone(bystander_pid));

    // Let the single-threaded service finish the orphaned read; its reply goes nowhere.
    scenario.feed_fifo("data/work/pipe", "late");
    scenario.signal("bystander", "go");
    let lines = scenario.wait_done("bystander");
    assert_eq!(ok_value(&lines[1]).unwrap()["content"], "a");
    assert!(scenario.transcript("worker").is_empty(), "the dead worker received nothing");

    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(bystander)));
    let report = supervisor.shutdown();
    assert!(report.is_clean(), "{report:?}");
    assert!(report.reaped.iter().any(|r| r.process == worker && !r.killed), "{report:?}");
    assert!(report.reaped.iter().any(|r| r.process == agent && !r.killed), "{report:?}");
    assert_eq!(supervisor.live_count(), 0);
    for host_pid in [agent_pid, worker_pid, fs_pid, bystander_pid] {
        assert!(pid_gone(host_pid), "host process {host_pid} outlived shutdown");
    }
}

#[test]
fn confinement_and_protocol_violations() {
    let mut scenario = Scenario::new();
    scenario.write("outside.txt", "OUTSIDE-CONTENT");
    scenario.write("data/work/a.txt", "a");
    std::os::unix::fs::symlink("../..", scenario.host("data/work/up")).unwrap();
    scenario.add_fs("fs", "data", "fs");
    let own_fixture = "/fixture/agent.json";
    let unbound = scenario.host("outside.txt");
    scenario.add_agent(
        "agent",
        &fixture(json!([
            {"tool": "fs.read", "path": ["work", "up", "outside.txt"]},
            {"tool": "host.read", "path": unbound.to_string_lossy()},
            {"tool": "host.read", "path": own_fixture},
            {"tool": "protocol.oversized_frame"},
        ])),
        None,
    );

    let mut supervisor = scenario.start();
    let fs = scenario.boot_service(&supervisor, "fs", "fs");
    let agent = supervisor.boot(program("agent"), agent_grants("fs", &["read"], &["work"])).unwrap();

    let lines = scenario.wait_transcript("agent", 3);
    // The kernel allowed this path (it is under /work); the service refused at actual access.
    assert!(error_code(&lines[0]).is_some(), "{:?}", lines[0]);
    assert!(scenario.access_log("data").iter().any(|l| mentions(l, "outside.txt")));
    let raw = std::fs::read_to_string(scenario.out_dir("agent").join("transcript.jsonl")).unwrap();
    assert!(!raw.contains("OUTSIDE-CONTENT"));
    assert!(error_code(&lines[1]).is_some(), "the sandbox has no /etc");
    assert!(ok_value(&lines[2]).is_some(), "host.read works for what the sandbox does contain");

    // An unreadable frame costs the sender its channel and nothing else.
    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(agent)));
    assert!(wait_for(TIMEOUT, || {
        scenario
            .events()
            .iter()
            .any(|e| matches!(e, SecurityEvent::ProtocolViolation { process, .. } if *process == agent))
    }));
    assert_eq!(supervisor.provider_of(&endpoint("fs")).map(|(p, _)| p), Some(fs));
    assert!(supervisor.shutdown().is_clean());
}

#[test]
fn unavailable_confinement_fails_closed() {
    let mut scenario = Scenario::new();
    scenario.add_fs("fs", "data", "fs");

    let mut no_backend = scenario.config(|_| {}, 64);
    no_backend.sandbox_tool = Some("/nonexistent/sandbox-tool".into());
    assert!(matches!(scenario.start_with(no_backend), Err(StartError::Confinement(_))));

    let mut missing_program = scenario.config(|_| {}, 64);
    missing_program.programs.insert(
        program("ghost"),
        ProgramSpec {
            executable: "/nonexistent/program".into(),
            args: vec![],
            binds: vec![],
            network: false,
        },
    );
    assert!(matches!(scenario.start_with(missing_program), Err(StartError::Confinement(_))));
    assert!(scenario.events().is_empty(), "nothing was started");
}

#[test]
fn a_process_that_stops_reading_is_terminated() {
    // The service blocks on a FIFO and stops reading its channel. Each writer's
    // dispatch carries ~120 KiB. The service holds one and its pipe 64 KiB, so the
    // second large dispatch already stalls the channel writer, two more fill the
    // outbound queue of 2, and the next one finds it full. Seven writers leave
    // a margin of two over the five that are needed.
    const WRITERS: usize = 7;
    let mut scenario = Scenario::new();
    scenario.mkfifo("data/work/pipe");
    scenario.add_fs("fs", "data", "fs");
    scenario.add_agent(
        "blocker",
        &fixture(json!([{"tool": "fs.read", "path": ["work", "pipe"], "deadline_ms": 60000}])),
        None,
    );
    let content = "x".repeat(120 * 1024);
    let writers: Vec<String> = (0..WRITERS).map(|i| format!("writer{i}")).collect();
    for (i, role) in writers.iter().enumerate() {
        let file = format!("big{i}.txt");
        scenario.add_agent(
            role,
            &fixture(json!([{"tool": "fs.create", "path": ["work", file], "content": content}])),
            None,
        );
    }

    let config = scenario.config(|limits| limits.max_inflight_per_endpoint = 8, 2);
    let mut supervisor = scenario.start_with(config).unwrap();
    let fs = scenario.boot_service(&supervisor, "fs", "fs");
    supervisor.boot(program("blocker"), agent_grants("fs", &["read"], &["work"])).unwrap();
    scenario.wait_access("data", "pipe");
    for role in &writers {
        supervisor.boot(program(role), agent_grants("fs", &["create"], &["work"])).unwrap();
    }

    assert!(supervisor.wait_until(TIMEOUT, |s| !s.is_live(fs)), "the stalled service was never terminated");
    assert_eq!(supervisor.provider_of(&endpoint("fs")), None);

    let blocked = scenario.wait_done("blocker");
    assert_eq!(error_code(&blocked[0]), Some("unavailable"));
    let mut unknown = 0;
    for (i, role) in writers.iter().enumerate() {
        let lines = scenario.wait_done(role);
        match error_code(&lines[0]) {
            // Dispatched before the service was dropped: a mutation may have happened.
            Some("unknown_outcome") => unknown += 1,
            // Arrived after the service was dropped: known not to have run.
            Some("unavailable") => {}
            other => panic!("{role}: unexpected result {other:?}: {:?}", lines[0]["result"]),
        }
        assert!(!scenario.host(&format!("data/work/big{i}.txt")).exists());
    }
    assert!(unknown >= 3, "only {unknown} dispatched creates were reported as unknown outcome");
    let report = supervisor.shutdown();
    assert!(report.is_clean());
    assert!(
        report.reaped.iter().any(|r| r.process == fs && r.killed),
        "a service blocked in host I/O still requires forceful teardown: {report:?}"
    );
}

#[test]
fn a_named_program_cannot_supply_a_child_with_an_uninherited_host_directory() {
    let mut scenario = Scenario::new();
    scenario.add_agent(
        "parent",
        &fixture(json!([
            {"tool": "spawn", "program": "worker", "grants": []}
        ])),
        None,
    );
    // Its output directory is a sibling of the parent's. Merely registering
    // this program must not authorize a child to write there.
    scenario.add_agent("worker", &fixture(json!([])), None);
    let mut supervisor = scenario.start();
    let parent = supervisor
        .boot(program("parent"), vec![nucleus_e2e::cap("kernel", &["spawn"], &["program", "worker"])])
        .unwrap();
    let lines = scenario.wait_done("parent");
    assert_eq!(error_code(&lines[0]), Some("permission_denied"));
    assert!(scenario.children_of(parent).is_empty());
    assert!(!scenario.out_dir("worker").join("transcript.jsonl").exists());
    assert!(supervisor.shutdown().is_clean());
}
