//! Run without its setuid bit, as tests and an unconfigured host run it, the
//! helper refuses before it executes anything: it never becomes a plain
//! launcher for whoever calls it.

#![allow(clippy::unwrap_used)]

use std::process::Command;

#[test]
fn without_privilege_or_policy_it_runs_nothing() {
    let helper = env!("CARGO_BIN_EXE_nucleus-as");
    for args in [vec!["--", "/usr/bin/id"], vec!["--", "id"], vec!["/usr/bin/id"], vec![]] {
        let output = Command::new(helper).args(&args).env("PATH", "/nonexistent").output().unwrap();
        assert_eq!(output.status.code(), Some(126), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: nothing ran: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("nucleus-as: "), "{output:?}");
    }
}
