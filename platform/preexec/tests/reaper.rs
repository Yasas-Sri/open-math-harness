//! The reaper in a process of its own: its state is the process's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::process::{Command, Stdio};

use nucleus_preexec::reaper;

/// A service outside a sandbox has a child it did not start for a command:
/// its supervisor's lifetime watcher, there before it ran anything. Ending a
/// command leaves that child alone and still ends what the command left.
#[test]
fn a_child_from_before_the_commands_is_left_alone_and_what_a_command_left_is_ended() {
    let mut before = Command::new("sleep").arg("600").spawn().unwrap();
    reaper::adopt_orphans().unwrap();

    let mut command = Command::new("sh");
    command.args(["-c", "sleep 600 >/dev/null 2>&1 & echo started"]).stdout(Stdio::piped());
    let mut tree = reaper::spawn(&mut command).unwrap();
    let mut said = String::new();
    tree.child().stdout.take().unwrap().read_to_string(&mut said).unwrap();
    let ended = tree.end().unwrap();
    assert_eq!((said.as_str(), ended.status.code(), ended.leftovers), ("started\n", Some(0), 1));

    assert!(
        matches!(before.try_wait(), Ok(None)),
        "the child this process had before its commands was taken for what one left"
    );
    before.kill().unwrap();
    before.wait().unwrap();
}
