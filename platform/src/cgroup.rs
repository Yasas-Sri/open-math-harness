//! Every program a cgroup of its own, every project one more.
//!
//! Once a program runs as the projects' uid, the runtime can no longer signal
//! it: `kill` and the lifetime watcher's process-group kill both need the
//! same uid. A cgroup does not: whoever may write a cgroup's `cgroup.kill`
//! kills every process in it, whatever its uid, also one that left its
//! session or process group. `nucleus.service` delegates its cgroup to the
//! runtime (`Delegate=yes`), so the supervisor creates one child cgroup per
//! program beneath its own, places the program's first process in it before
//! that process executes anything (`nucleus_preexec::enter_cgroup`), stops
//! it with `cgroup.kill`, and confirms cleanup when the cgroup reports itself
//! unpopulated and is removed.
//!
//! What it needs, checked at startup by creating and removing a cgroup: the
//! unified (v2) hierarchy, a cgroup this process may create children in, and
//! `cgroup.kill` (Linux 5.14). Without them the runtime does not start when
//! it was configured for projects.
//!
//! A project's processes are in its own cgroup, `project-<name>`, each of
//! them in its program's cgroup beneath: killing the project's cgroup ends
//! every process of the project (its Stop, and its deletion), and the
//! project cgroups that exist are the projects with processes, of which at
//! most a configured number may exist at once ([`room_for_project`]). One is
//! made with the first process of its project and removed with its last.
//!
//! A program's cgroup is named after its process (`p<id>`).

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long removal of a stopped program's cgroup may take.
pub(crate) const CGROUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
/// The prefix of a project's cgroup.
const PROJECT: &str = "project-";

/// This process's cgroup in the unified hierarchy.
fn own() -> Result<PathBuf, String> {
    let text = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("cannot read this process's cgroup: {e}"))?;
    let own = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("the unified cgroup hierarchy (cgroup v2) is not mounted")?;
    Ok(Path::new("/sys/fs/cgroup").join(own.trim_start_matches('/')))
}

/// The cgroup the runtime runs in, beneath which every program gets its own.
#[derive(Debug)]
pub struct CgroupRoot {
    path: PathBuf,
}

impl CgroupRoot {
    /// This process's own cgroup, checked to be usable: a cgroup created in it
    /// has `cgroup.kill` and can be removed again. Child cgroups an earlier
    /// runtime left there are stopped and removed first.
    pub fn open() -> Result<Self, String> {
        Self::checked(own()?, true)
    }

    /// [`Self::open`] without touching other child cgroups, for tests that
    /// share their cgroup with other tests.
    #[cfg(test)]
    pub(crate) fn open_shared() -> Result<Self, String> {
        Self::checked(own()?, false)
    }

    fn checked(path: PathBuf, sweep: bool) -> Result<Self, String> {
        let root = Self { path };
        if sweep {
            root.sweep();
        }
        let probe = root.create(&format!("probe-{}", std::process::id())).map_err(|e| {
            format!(
                "cannot create a cgroup beneath {}: {e} (the service needs Delegate=yes and Linux 5.14)",
                root.path.display()
            )
        })?;
        if !probe.path.join("cgroup.kill").is_file() {
            let _ = probe.remove();
            return Err("the kernel has no cgroup.kill (Linux 5.14 or later is needed)".into());
        }
        probe.remove()?;
        Ok(root)
    }

    /// A new, empty cgroup `name` beneath the root. One left by an earlier
    /// runtime under the same name is killed and removed first.
    pub fn create(&self, name: &str) -> Result<ProgramCgroup, String> {
        let cgroup = ProgramCgroup { path: self.path.join(name) };
        match std::fs::create_dir(&cgroup.path) {
            Ok(()) => return Ok(cgroup),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create cgroup {}: {e}", cgroup.path.display())),
        }
        cgroup.stop()?;
        std::fs::create_dir(&cgroup.path)
            .map_err(|e| format!("cannot create cgroup {}: {e}", cgroup.path.display()))?;
        Ok(cgroup)
    }

    /// The cgroup of `project`'s processes, made when it has none; each of
    /// its programs gets one of its own beneath it ([`Self::create`]).
    pub fn project(&self, project: &str) -> Result<CgroupRoot, String> {
        if !nucleus_protocol::valid_project(project) {
            return Err(format!("{project:?} is not a project's name"));
        }
        let path = self.path.join(format!("{PROJECT}{project}"));
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(CgroupRoot { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(CgroupRoot { path }),
            Err(e) => Err(format!("cannot create cgroup {}: {e}", path.display())),
        }
    }

    /// The cgroup of `project`'s processes, whether or not it exists.
    pub fn project_cgroup(&self, project: &str) -> ProgramCgroup {
        ProgramCgroup { path: self.path.join(format!("{PROJECT}{project}")) }
    }

    /// Whether `project` may have processes: it has them already, or fewer
    /// than `max` projects have.
    pub fn room_for_project(&self, project: &str, max: usize) -> Result<(), String> {
        room_for_project(&self.path, project, max)
    }

    /// Stops and removes the child cgroups an earlier runtime left: those
    /// named as this module names them, and no other. Best effort: one that
    /// stays is dealt with when its name is needed again.
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.path) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if generated(&name) && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let cgroup = ProgramCgroup { path: entry.path() };
                if name.starts_with(PROJECT) {
                    // Killing it ends every process beneath; its programs'
                    // cgroups go before it can.
                    let _ = cgroup.kill();
                    CgroupRoot { path: entry.path() }.sweep();
                }
                let _ = cgroup.stop();
            }
        }
    }
}

/// Whether one more process of `project` may start under the cgroup root
/// `root`: the project's cgroup exists (its processes always start), or
/// fewer than `max` project cgroups do. At most `max` projects have
/// processes at once, however many there are.
pub(crate) fn room_for_project(root: &Path, project: &str, max: usize) -> Result<(), String> {
    let entries = std::fs::read_dir(root)
        .map_err(|e| format!("cannot count the projects' cgroups in {}: {e}", root.display()))?;
    let mut active = 0;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot count the projects' cgroups: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|n| n.strip_prefix(PROJECT)) else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if name == project {
            return Ok(());
        }
        active += 1;
    }
    if active < max {
        return Ok(());
    }
    Err(format!(
        "at most {max} projects may have processes at once in this VM, and {active} do: a project has \
         processes while anything of it runs (a host of its files or shells, an agent, a job); stop \
         another project, or wait until its idle hosts stop"
    ))
}

/// Whether this module gives a cgroup the name `name` (`p<id>`, `probe-…`,
/// `project-…`), so that a sweep removes it.
fn generated(name: &str) -> bool {
    name.strip_prefix('p').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        || name.starts_with("probe-")
        || name.starts_with(PROJECT)
}

/// One program's cgroup.
#[derive(Debug)]
pub struct ProgramCgroup {
    path: PathBuf,
}

impl ProgramCgroup {
    fn open(&self, file: &str) -> Result<OwnedFd, String> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.path.join(file))
            .map(OwnedFd::from)
            .map_err(|e| format!("cannot open {}/{file}: {e}", self.path.display()))
    }

    /// `cgroup.procs`, for the child to enter before it executes.
    pub fn procs(&self) -> Result<OwnedFd, String> {
        self.open("cgroup.procs")
    }

    /// `cgroup.kill`, for the lifetime watcher.
    pub fn kill_file(&self) -> Result<OwnedFd, String> {
        self.open("cgroup.kill")
    }

    /// Kills every process in the cgroup, whatever its uid.
    pub fn kill(&self) -> Result<(), String> {
        std::fs::write(self.path.join("cgroup.kill"), "1")
            .map_err(|e| format!("cannot kill cgroup {}: {e}", self.path.display()))
    }

    /// The processes in the cgroup, as the kernel lists them: what the
    /// runtime can see of a program that runs as another user, whose `/proc`
    /// entries are hidden from it.
    #[cfg(test)]
    pub(crate) fn members(&self) -> Vec<u32> {
        let listed = std::fs::read_to_string(self.path.join("cgroup.procs")).unwrap_or_default();
        listed.lines().filter_map(|pid| pid.parse().ok()).collect()
    }

    /// Whether the cgroup exists.
    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// Whether a process is still in the cgroup (zombies are not).
    pub fn populated(&self) -> Result<bool, String> {
        let events = std::fs::read_to_string(self.path.join("cgroup.events"))
            .map_err(|e| format!("cannot read {}/cgroup.events: {e}", self.path.display()))?;
        events
            .lines()
            .find_map(|line| line.strip_prefix("populated "))
            .map(|value| value.trim() != "0")
            .ok_or_else(|| format!("{}/cgroup.events has no populated line", self.path.display()))
    }

    /// Waits until no process is left, then removes the cgroup: the cleanup
    /// confirmation of a program that runs in one. A deadline or an error keeps
    /// the obligation and reports it; it is never taken for cleanup.
    pub fn remove_when_empty(&self, deadline: Instant) -> Result<(), String> {
        loop {
            if !self.populated()? {
                return self.remove();
            }
            if Instant::now() >= deadline {
                return Err(format!("cgroup {} still has processes", self.path.display()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until no process is left, or the cgroup is gone: once a
    /// project's cgroup was killed, until every process of it has ended. A
    /// deadline is reported, never taken for an empty cgroup.
    pub fn emptied(&self, deadline: Instant) -> Result<(), String> {
        loop {
            match std::fs::read_to_string(self.path.join("cgroup.events")) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                _ if !self.populated()? => return Ok(()),
                _ if Instant::now() >= deadline => {
                    return Err(format!("cgroup {} still has processes", self.path.display()));
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Removes the cgroup if nothing uses it any more: no process, and no
    /// cgroup beneath (a project's, with none of its programs left).
    pub fn remove_if_unused(&self) -> Result<(), String> {
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // Still holding a process or a program's cgroup.
            Err(e)
                if e.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error())
                    || e.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                Ok(())
            }
            Err(e) => Err(format!("cannot remove cgroup {}: {e}", self.path.display())),
        }
    }

    /// Removes the (empty) cgroup.
    pub fn remove(&self) -> Result<(), String> {
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove cgroup {}: {e}", self.path.display())),
        }
    }

    /// Kills what is left and removes the cgroup.
    fn stop(&self) -> Result<(), String> {
        if self.populated()? {
            self.kill()?;
        }
        self.remove_when_empty(Instant::now() + CGROUP_CLEANUP_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::process::{Command, Stdio};

    /// At most `max` projects have processes at once, counted by their
    /// cgroups: one more project is refused with the limit named, a project
    /// that has a cgroup always gets its processes, and the runtime's own
    /// programs' cgroups count for nothing.
    #[test]
    fn at_most_the_configured_number_of_projects_have_processes_at_once() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..96 {
            std::fs::create_dir(root.path().join(format!("project-p{n}"))).unwrap();
        }
        for other in ["p12", "probe-7"] {
            std::fs::create_dir(root.path().join(other)).unwrap();
        }
        std::fs::write(root.path().join("project-file"), "").unwrap();
        assert_eq!(room_for_project(root.path(), "p95", 96), Ok(()), "an active project's process starts");
        let refused = room_for_project(root.path(), "one-more", 96).unwrap_err();
        assert!(refused.contains("at most 96 projects may have processes at once"), "{refused}");
        std::fs::remove_dir(root.path().join("project-p0")).unwrap();
        assert_eq!(room_for_project(root.path(), "one-more", 96), Ok(()), "once a project has none left");
    }

    /// A process that leaves its session and process group (as a daemon or a
    /// `setsid` command does) is still in its program's cgroup: when the launcher
    /// dies the lifetime watcher kills the cgroup, and nothing is left.
    #[test]
    #[ignore = "needs a cgroup this user may create children in (a systemd user session, or Delegate=yes)"]
    fn the_watcher_kills_a_program_s_whole_cgroup_when_its_launcher_dies() {
        let root = CgroupRoot::open_shared().expect("a delegated cgroup");
        let cgroup = root.create(&format!("test-{}", std::process::id())).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "setsid sleep 300 & sleep 300"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        nucleus_preexec::enter_cgroup(&mut command, cgroup.procs().unwrap());
        nucleus_preexec::bind_parent_lifetime_in_cgroup(&mut command, cgroup.kill_file().unwrap());
        let mut launcher = command.spawn().unwrap();
        let started = Instant::now();
        while std::fs::read_to_string(cgroup.path.join("cgroup.procs")).unwrap().lines().count() < 4 {
            assert!(started.elapsed() < Duration::from_secs(5), "the program did not start in its cgroup");
            std::thread::sleep(Duration::from_millis(10));
        }
        launcher.kill().unwrap();
        launcher.wait().unwrap();
        cgroup.remove_when_empty(Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(!cgroup.path.exists());
    }
}
