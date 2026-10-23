//! Real process tests, including the gap before bubblewrap arms PDEATHSIG.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_preexec::bind_parent_lifetime;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

fn wait_for(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < until, "process condition did not become true");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn state(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

fn alive(pid: u32) -> bool {
    state(pid).is_some_and(|stat| !stat.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
}

fn children(pid: u32) -> Vec<u32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect()
}

fn tree(pid: u32) -> Vec<u32> {
    let mut all = vec![pid];
    for child in children(pid) {
        all.extend(tree(child));
    }
    all
}

fn stop(pid: u32) {
    // Only identities captured from this test's own newly spawned children.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

fn bwrap() -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    bind_parent_lifetime(&mut command);
    command.args([
        "--unshare-all",
        "--die-with-parent",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
    ]);
    command
}

struct Fixture {
    directory: PathBuf,
    runtime: Child,
    sandbox: Option<u32>,
}

impl Fixture {
    fn start(mode: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "nucleus-lifetime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let runtime = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lifetime_fixture", "--nocapture"])
            .env("NUCLEUS_LIFETIME_FIXTURE", mode)
            .env("NUCLEUS_LIFETIME_DIRECTORY", &directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self { directory, runtime, sandbox: None }
    }

    fn sandbox(&mut self) -> u32 {
        wait_for(|| {
            self.sandbox =
                fs::read_to_string(self.directory.join("pid")).ok().and_then(|pid| pid.parse().ok());
            self.sandbox.is_some()
        });
        self.sandbox.unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A failing regression must not add another running orphan. A sandbox
        // may still be blocked before exec, so explicitly resume before kill.
        if let Some(pid) = self.sandbox {
            unsafe {
                libc::kill(pid as i32, libc::SIGCONT);
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.runtime.kill();
        let _ = self.runtime.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn lifetime_fixture() {
    let Ok(mode) = std::env::var("NUCLEUS_LIFETIME_FIXTURE") else { return };
    let directory = PathBuf::from(std::env::var_os("NUCLEUS_LIFETIME_DIRECTORY").unwrap());
    let mut command = if mode == "blocked" {
        let mut command = Command::new("/usr/bin/bwrap");
        bind_parent_lifetime(&mut command);
        // Bubblewrap must read the NUL-separated arguments to EOF before it
        // creates namespaces or arms its own parent-death signal. Holding this
        // pipe open makes the exec/profile-transition race deterministic.
        command.args(["--args", "0"]);
        command
    } else {
        let mut command = bwrap();
        command.args(["--", "/usr/bin/sleep", "60"]);
        command
    };
    command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    if mode == "before_exec" {
        // Run AFTER the watcher readiness gate but before exec. The outer test
        // discovers this stopped child through /proc rather than using a delay.
        unsafe {
            command.pre_exec(|| {
                libc::raise(libc::SIGSTOP);
                Ok(())
            });
        }
    }
    let mut child = command.spawn().unwrap();
    fs::write(directory.join("pid.pending"), child.id().to_string()).unwrap();
    fs::rename(directory.join("pid.pending"), directory.join("pid")).unwrap();
    // Child::wait closes its own stdin first; retain the gate explicitly.
    let _gate = child.stdin.take();
    let _ = child.wait();
}

#[test]
fn owner_death_after_bwrap_exec_before_its_handler_kills_the_group() {
    let mut fixture = Fixture::start("blocked");
    let sandbox = fixture.sandbox();
    wait_for(|| fs::read_to_string(format!("/proc/{sandbox}/wchan")).is_ok_and(|w| w.contains("pipe_read")));
    let owned = tree(sandbox);
    assert_eq!(owned.len(), 2, "launcher and its watcher only, before namespace setup: {owned:?}");
    let watcher = owned[1];
    let fds = fs::read_dir(format!("/proc/{watcher}/fd"))
        .unwrap()
        .map(|entry| fs::read_link(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(fds.len(), 2, "watcher must not keep stdio, exec-error pipe or unrelated handles");
    assert!(fds.iter().all(|fd| fd == Path::new("anon_inode:[pidfd]")), "{fds:?}");
    unsafe {
        libc::kill(watcher as i32, libc::SIGTERM);
        libc::kill(watcher as i32, libc::SIGUSR1);
    }
    assert!(alive(watcher), "watcher must not execute inherited application signal handlers");
    let mut sibling = Command::new("/usr/bin/sleep").arg("60").spawn().unwrap();
    fixture.runtime.kill().unwrap();
    fixture.runtime.wait().unwrap();
    wait_for(|| owned.iter().all(|pid| !alive(*pid)));
    fixture.sandbox = None;
    assert!(sibling.try_wait().unwrap().is_none(), "unrelated process must survive");
    sibling.kill().unwrap();
    sibling.wait().unwrap();
}

#[test]
fn owner_death_before_exec_does_not_leave_the_ready_watcher() {
    let mut fixture = Fixture::start("before_exec");
    wait_for(|| {
        // The harness runs the fixture on a test thread, so inspect every task.
        fixture.sandbox = fs::read_dir(format!("/proc/{}/task", fixture.runtime.id()))
            .unwrap()
            .filter_map(Result::ok)
            .flat_map(|entry| {
                fs::read_to_string(entry.path().join("children"))
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .find(|pid| {
                state(*pid).is_some_and(|stat| stat.rsplit_once(')').unwrap().1.trim_start().starts_with('T'))
            });
        fixture.sandbox.is_some()
    });
    let owned = tree(fixture.sandbox.unwrap());
    assert_eq!(owned.len(), 2, "ready watcher exists before exec: {owned:?}");
    fixture.runtime.kill().unwrap();
    fixture.runtime.wait().unwrap();
    wait_for(|| owned.iter().all(|pid| !alive(*pid)));
    fixture.sandbox = None;
}

#[test]
fn killing_bwrap_reaps_its_running_namespace_and_watcher() {
    let mut fixture = Fixture::start("running");
    let sandbox = fixture.sandbox();
    let mut owned = Vec::new();
    wait_for(|| {
        owned = tree(sandbox);
        owned.len() >= 4
            && owned.iter().any(|pid| {
                fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "sleep")
            })
    });
    stop(sandbox);
    wait_for(|| owned.iter().all(|pid| !alive(*pid)));
    fixture.sandbox = None;
    assert!(fixture.runtime.wait().unwrap().success());
}

#[test]
fn normal_exit_releases_watcher_and_application_pipes() {
    let mut command = bwrap();
    command.args(["--", "/usr/bin/cat"]).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut owned = Vec::new();
    wait_for(|| {
        owned = tree(child.id());
        owned.len() >= 4
    });
    drop(child.stdin.take());
    assert!(child.wait_with_output().unwrap().status.success());
    wait_for(|| owned.iter().all(|pid| !alive(*pid)));
    assert!(bwrap().args(["--", "/usr/bin/true"]).status().unwrap().success());
}

#[test]
fn setup_and_exec_errors_remain_spawn_errors() {
    let mut invalid = Command::new("/nucleus-no-such-parent-lifetime-test-program");
    bind_parent_lifetime(&mut invalid);
    assert_eq!(invalid.spawn().unwrap_err().kind(), std::io::ErrorKind::NotFound);

    let mut session_leader = Command::new("/usr/bin/true");
    // Force setsid() in bind_parent_lifetime to fail, before any watcher forks.
    unsafe {
        session_leader.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    bind_parent_lifetime(&mut session_leader);
    assert_eq!(session_leader.spawn().unwrap_err().raw_os_error(), Some(libc::EPERM));
}

#[test]
fn watcher_setup_failure_is_reported_before_the_program_can_exec() {
    let mut command = Command::new("/usr/bin/true");
    unsafe {
        command.pre_exec(|| {
            // Deny close_range only in this private child and its watcher, to
            // exercise an actual post-fork watcher setup failure, not a mock.
            let mut filter = [
                libc::sock_filter {
                    code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                libc::sock_filter {
                    code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 1,
                    k: libc::SYS_close_range as u32,
                },
                libc::sock_filter {
                    code: (libc::BPF_RET | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                },
                libc::sock_filter {
                    code: (libc::BPF_RET | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ALLOW,
                },
            ];
            let filter = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &filter) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // This inherited disposition would otherwise auto-reap the watcher
            // and invalidate the launcher's kill/wait ownership on failure.
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }
    bind_parent_lifetime(&mut command);
    assert_eq!(command.spawn().unwrap_err().raw_os_error(), Some(libc::EIO));
}
