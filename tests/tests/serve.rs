//! `nucleus serve` as the VM runs it: boot a resident composition, serve a
//! window through the UI service, and exit cleanly on SIGTERM.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use nucleus_e2e::{TIMEOUT, binaries, wait_for};
use serde_json::{Value, json};

#[test]
fn serve_boots_answers_the_window_and_stops_cleanly_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["sessions", "ui"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    let grant = |endpoint: &str, actions: &[&str], resource: &[&str]| json!({"endpoint": endpoint, "actions": actions, "resource": resource, "delegable": false});
    let config = json!({
        "lifetime": "resident",
        "programs": {
            "conversation": {"executable": binaries().conversation, "args": ["--root", "/sessions"],
                "binds": [{"host": dir.path().join("sessions"), "guest": "/sessions", "writable": true}]},
            "ui": {"executable": binaries().ui, "args": ["--socket", "/ui/ui.sock"],
                "binds": [{"host": dir.path().join("ui"), "guest": "/ui", "writable": true}]},
        },
        "boot": [
            {"program": "conversation", "provides": "conversation",
             "grants": [grant("kernel", &["provide"], &["endpoint", "conversation"])]},
            {"program": "ui", "grants": [grant("conversation", &["say", "history"], &[])]},
        ],
    });
    let path = dir.path().join("nucleus.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let launcher = binaries().ui.with_file_name("nucleus");
    let mut child = Command::new(launcher).arg("serve").arg(&path).spawn().unwrap();

    let socket = dir.path().join("ui/ui.sock");
    let mut stream = None;
    assert!(wait_for(TIMEOUT, || {
        stream = UnixStream::connect(&socket).ok();
        stream.is_some()
    }));
    let stream = stream.unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut request = |operation: Value| -> Value {
        writeln!(writer, "{operation}").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let me = request(json!({"id": 1, "op": "whoami"}));
    assert!(me["ok"]["handles"].as_array().unwrap().iter().any(|h| h["endpoint"] == "conversation"));
    let said = request(json!({"id": 2, "op": "conversation.say", "session": "default",
        "message_id": "u1", "text": "hello"}));
    assert!(said.get("ok").is_some(), "{said}");
    let refused = request(json!({"id": 3, "op": "sessions.list"}));
    assert_eq!(refused["error"]["code"], "permission_denied");

    let term = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(term.success());
    let mut status = None;
    assert!(wait_for(TIMEOUT, || {
        status = child.try_wait().unwrap();
        status.is_some()
    }));
    assert!(status.unwrap().success(), "a signalled shutdown is clean: {status:?}");
}
