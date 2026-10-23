//! The projects service under a real kernel and sandbox. In its user namespace
//! every project's folder shows as owned by nobody, so it finds projects by
//! their names, and a host's project is the name it asks the kernel for.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use nucleus_e2e::{Scenario, cap, error_code, ok_value, program};
use nucleus_kernel::CapSpec;
use serde_json::json;

/// What only this CI can run of the path from a project's folder to its
/// host running as the project: the projects service under real bwrap. A
/// folder owned by another user (root's `/usr/share` stands in for the
/// projects' user's) is a project there. A creation asks the kernel for a
/// process of the new project, its host, which this kernel, with no
/// projects' processes (no setuid helper here), refuses, and nothing of the
/// project is left. The rest, the host running as the projects' user and
/// making the folder, needs a VM.
#[test]
fn the_projects_service_finds_homes_by_name_in_its_user_namespace() {
    let mut scenario = Scenario::new();
    std::fs::create_dir_all(scenario.host("projects/alpha")).unwrap();
    scenario.add_projects("projects", "projects", "home", &[(Path::new("/usr/share"), "/projects/alpha")]);
    let call = |action: &str, path: &[&str]| json!({"tool": "call", "endpoint": "projects", "action": action, "path": path, "payload": {}});
    scenario.add_agent(
        "window",
        &json!({"turns": [{"tool_calls": [call("list", &["projects"]), call("create", &["projects", "beta"])]}]}),
        None,
    );
    let mut supervisor = scenario.start();
    let delegable = |endpoint: &str, actions: &[&str], resource: &[&str]| CapSpec {
        delegable: true,
        ..cap(endpoint, actions, resource)
    };
    let fixed = |endpoint: &str, actions: &[&str], resource: &[&str]| CapSpec {
        delegable: false,
        ..cap(endpoint, actions, resource)
    };
    supervisor
        .boot(
            program("projects"),
            vec![
                fixed("kernel", &["provide"], &["endpoint", "projects"]),
                delegable("kernel", &["create_endpoint"], &["endpoint"]),
                fixed("kernel", &["spawn_executable"], &["executable", "programs"]),
                fixed("kernel", &["spawn_as"], &["identity"]),
                fixed("kernel", &["stop_project"], &["identity"]),
            ],
        )
        .unwrap();
    let projects = nucleus_e2e::endpoint("projects");
    assert!(supervisor.wait_until(nucleus_e2e::TIMEOUT, |k| k.provider_of(&projects).is_some()));
    supervisor.boot(program("window"), vec![fixed("projects", &["list", "create"], &["projects"])]).unwrap();
    let transcript = scenario.wait_done_within("window", Duration::from_secs(60));
    assert_eq!(
        ok_value(&transcript[0]),
        Some(&json!({"projects": [{"name": "alpha", "dir": "alpha"}]})),
        "a home owned by nobody in the sandbox is still a project"
    );
    // Not `permission_denied` (the authority is held): asked of a kernel
    // with no projects' processes, which refuses (a provider reports the
    // kernel's refusal as its own failure).
    assert_eq!(error_code(&transcript[1]), Some("service_error"), "{:?}", transcript[1]);
    assert!(transcript[1].to_string().contains("no project's processes"), "{:?}", transcript[1]);
    let left: Vec<_> =
        std::fs::read_dir(scenario.host("projects")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(left, ["alpha"], "nothing of beta is left");
    assert!(supervisor.shutdown().is_clean());
}

/// Deleting a project has every service that keeps its records forget it, here
/// through the real projects, conversation and approvals services and the UI
/// service, booted in the VM's order (the approvals guard after the projects
/// service, whose `fs` it uses; the UI service, whose deletion workflow asks
/// them all, last), the deletion asked over the platform's service link as the
/// platform asks it: its sessions and the approvals journal's lines about it
/// go, anything said into it after that makes no session again, and a new
/// project of the same name starts with none. The VM's path, with the projects'
/// user and the project's cgroup, needs a VM.
#[test]
fn a_deleted_project_leaves_no_record_and_its_name_starts_empty() {
    let mut scenario = Scenario::new();
    scenario.add_projects_without_identities("projects", "projects", "home");
    scenario.add_conversation("conversation");
    scenario.bind("conversation", "projects", "/projects", false);
    scenario.add_args("conversation", &["--projects", "/projects"]);
    scenario.add_approvals("approvals", &json!({"version": 1, "rules": []}));
    let line = |project: &str, seq: u64| {
        json!({"v": 1, "seq": seq, "at_ms": 1, "event": "denied_by_policy", "policy_version": 1,
               "call": {"requester": 7, "endpoint": "fs", "action": "remove", "resource": ["projects", project, "repo", "x"]}})
        .to_string()
    };
    scenario.write("approvals-log/approvals.jsonl", &format!("{}\n{}\n", line("alpha", 0), line("beta", 1)));
    scenario.add_labelled_ui("ui");
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "conversation", "conversation");
    let fixed = |endpoint: &str, actions: &[&str], resource: &[&str]| CapSpec {
        delegable: false,
        ..cap(endpoint, actions, resource)
    };
    supervisor
        .boot(
            program("projects"),
            vec![
                fixed("kernel", &["provide"], &["endpoint", "projects"]),
                CapSpec { delegable: true, ..cap("kernel", &["create_endpoint"], &["endpoint"]) },
                fixed("kernel", &["spawn_executable"], &["executable", "programs"]),
            ],
        )
        .unwrap();
    let projects = nucleus_e2e::endpoint("projects");
    assert!(supervisor.wait_until(nucleus_e2e::TIMEOUT, |k| k.provider_of(&projects).is_some()));
    scenario.boot_service(&supervisor, "approvals", "approvals");
    supervisor
        .boot(
            program("ui"),
            vec![
                fixed("projects", &["list", "create", "delete", "purge"], &["projects"]),
                fixed("conversation", &["say", "history", "list", "ensure"], &[]),
                fixed("approvals", &["forget"], &[]),
                fixed("conversation", &["forget"], &[]),
            ],
        )
        .unwrap();
    let mut ui = scenario.connect_ui_labelled("ui", &nucleus_e2e::window_label("Ada", "owner"));
    let ada = nucleus_e2e::account_of("Ada");
    let mut platform = scenario.connect_ui_labelled(
        "ui",
        &json!({"v": 1, "kind": "service", "account": ada, "nickname": "Ada", "role": "service",
                "project": "alpha", "as": ada}),
    );
    let say = |session: &str| json!({"op": "conversation.say", "project": "alpha", "session": session, "message_id": "u1", "text": "remember me"});
    ui.request(json!({"op": "projects.create", "name": "alpha"})).unwrap();
    ui.request(say("s1")).unwrap();
    assert!(scenario.host("sessions/alpha/s1/log.jsonl").is_file());

    let deleting = platform.request(json!({"op": "projects.delete", "project": "alpha"})).unwrap();
    assert_eq!(deleting["state"], "deleting", "{deleting}");
    let mut gone = || {
        let listed = ui.request(json!({"op": "projects.list"})).unwrap();
        listed["projects"].as_array().unwrap().is_empty()
    };
    assert!(nucleus_e2e::wait_for(nucleus_e2e::TIMEOUT, &mut gone), "alpha is never gone");
    let deleted = platform.request(json!({"op": "projects.delete", "project": "alpha"})).unwrap();
    assert_eq!(deleted["state"], "deleted");
    assert!(!scenario.host("sessions/alpha").exists(), "its sessions are forgotten");
    let journal = std::fs::read_to_string(scenario.host("approvals-log/approvals.jsonl")).unwrap();
    assert_eq!(journal, format!("{}\n", line("beta", 1)), "only another project's line is kept");
    assert!(ui.request(say("s2")).is_err(), "said into a project that is no more");
    assert!(!scenario.host("sessions/alpha").exists());

    ui.request(json!({"op": "projects.create", "name": "alpha"})).unwrap();
    let listed = ui.request(json!({"op": "conversation.list", "project": "alpha"})).unwrap();
    assert_eq!(listed["sessions"], json!([]), "a project of the same name starts with no session");
    drop((ui, platform));
    assert!(supervisor.shutdown().is_clean());
}
