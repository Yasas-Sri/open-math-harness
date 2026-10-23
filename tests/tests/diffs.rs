//! The git diff service under a real kernel and sandbox: it reads a
//! repository in the workspace through a read-only bind, answers with what
//! `git diff` shows, and leaves the repository exactly as it was.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nucleus_e2e::{Scenario, TIMEOUT, cap, endpoint, ok_value, program};
use serde_json::json;

/// The host's git, as the person would run it.
fn git(scenario: &Scenario, dir: &Path, args: &[&str]) {
    let out = Command::new("/usr/bin/git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", scenario.host("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Every file beneath `dir`, `.git` included, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                files.insert(entry.path(), std::fs::read(entry.path()).unwrap());
            }
        }
    }
    files
}

#[test]
fn review_reads_a_repositorys_uncommitted_change_and_leaves_it_untouched() {
    let mut scenario = Scenario::new();
    scenario.add_diffs("diffs", "work", "chat");
    std::fs::create_dir_all(scenario.host("home")).unwrap();
    let repository = scenario.host("work/chat");
    scenario.write("work/chat/a.txt", "one\ntwo\n");
    git(&scenario, &repository, &["init", "-q"]);
    git(&scenario, &repository, &["add", "a.txt"]);
    git(&scenario, &repository, &["commit", "-q", "-m", "first"]);
    scenario.write("work/chat/a.txt", "ONE\ntwo\n");
    scenario.write("work/chat/new.txt", "fresh\n");
    let before = snapshot(&repository);

    let call = |action: &str, payload: serde_json::Value| json!({"tool": "call", "endpoint": "diffs", "action": action, "path": [], "payload": payload});
    scenario.add_agent(
        "reader",
        &json!({"turns": [{"tool_calls": [
            call("repositories", json!({})),
            call("changes", json!({"repository": "chat"})),
            call("file", json!({"repository": "chat", "path": "a.txt"})),
        ]}]}),
        None,
    );
    let mut supervisor = scenario.start();
    scenario.boot_service(&supervisor, "diffs", "diffs");
    supervisor
        .boot(program("reader"), vec![cap("diffs", &["repositories", "changes", "file"], &[])])
        .unwrap();
    assert!(supervisor.wait_until(TIMEOUT, |k| k.provider_of(&endpoint("diffs")).is_some()));
    let transcript = scenario.wait_done_within("reader", Duration::from_secs(60));
    let ok = |i: usize| ok_value(&transcript[i]).unwrap_or_else(|| panic!("{:?}", transcript[i])).clone();

    // A version is opaque; the page of a file carries the one its summary listed.
    let (mut changes, mut file) = (ok(1), ok(2));
    let version = |value: &mut serde_json::Value| value.as_object_mut().unwrap().remove("version").unwrap();
    let listed: Vec<_> = changes["files"].as_array_mut().unwrap().iter_mut().map(version).collect();
    assert_eq!(version(&mut file), listed[0]);

    assert_eq!(ok(0), json!({"repositories": [{"path": "chat", "branch": "main"}]}));
    assert_eq!(
        changes,
        json!({"repository": "chat", "files": [
            {"path": "a.txt", "status": "modified", "additions": 1, "deletions": 1},
            {"path": "new.txt", "status": "added", "additions": 1, "deletions": 0, "untracked": true},
        ]})
    );
    assert_eq!(file, json!({"path": "a.txt", "hunks": [{"line": 1, "deleted": ["one"], "added": ["ONE"]}]}));
    assert_eq!(snapshot(&repository), before, "the service changed the repository");
    assert!(supervisor.shutdown().is_clean());
}
