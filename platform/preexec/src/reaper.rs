//! Nothing a command starts outlives it.
//!
//! A service that runs commands for others (the shell) ends
//! everything a command started when the command ends. A process group or a
//! session does not hold that: GNU `timeout` makes a group of its own,
//! `setsid` and `nohup setsid` a session, a daemon both, and once their
//! parent is gone such processes belong to the sandbox's init and live as
//! long as the sandbox. A child subreaper holds them, and needs no
//! privilege: a process whose parent dies becomes the child of its nearest
//! living ancestor marked as a subreaper, and nothing it does can make it
//! skip one.
//!
//! - The service marks itself ([`adopt_orphans`]) before it starts a command.
//! - It starts each command with [`spawn`], which marks the command's own
//!   process as well (the mark survives `exec`): while the command runs,
//!   what its processes leave stays beneath it.
//! - When the command ends, [`Tree::end`] (or dropping the [`Tree`]) kills
//!   the command's process if it still runs and reaps it. What the command
//!   started is then the service's child, and the service kills and reaps
//!   what commands left among its children, round after round (a killed
//!   process's children are the service's next), until none is left.
//!
//! What a command left is a child of the service that came to it after it
//! began to adopt orphans and is not a running command's own process. The
//! children it had before are not: nothing a command starts can become one of
//! them. Outside a sandbox the service has such a child, its supervisor's
//! lifetime watcher, which runs as the supervisor's user and which the
//! service must neither end nor wait for. So the service starts every child
//! of its own with [`spawn`], after [`adopt_orphans`]: any other that appears
//! later is taken for something an ended command left. A [`Tree`] is a
//! command's processes: whoever holds it owns them, and moving it to another
//! owner (a background job, say) hands them over without killing any.
//!
//! Not covered: when a command's process clears its own mark, what its
//! processes leave comes to the service at once, as does a process it starts
//! beside itself (`CLONE_PARENT`); either is killed when any command ends,
//! perhaps before its own does. A command that kills a sandboxed service
//! ends the sandbox, and everything in it.

use std::collections::BTreeMap;
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};

/// How long ending a command may wait for its processes to die.
const END_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(1);

/// The variable each command's processes carry in their environment, naming
/// the command: what one leaves is counted for it, whichever command's end
/// finds it ([`Ended::leftovers`]). A process that changes its environment
/// is counted for the command whose end finds it.
pub const TAG: &str = "NUCLEUS_COMMAND";

/// The commands that run, and what was found of the ended ones.
#[derive(Default)]
struct State {
    /// The running commands' processes, by pid, and their tags. Held while a
    /// command starts until it is listed, and while leftovers are swept, so
    /// a sweep never takes a starting command for a leftover.
    running: Vec<(u32, u64)>,
    last_tag: u64,
    /// Processes still running that sweeps stopped, by the tag of the
    /// command that started them.
    stopped: BTreeMap<u64, u64>,
    /// The children this process had when it began to adopt orphans: no
    /// command left them. A sweep never reaps one, so its pid stays its own
    /// for as long as this process lives.
    before: Option<Vec<u32>>,
}

static STATE: Mutex<State> =
    Mutex::new(State { running: Vec::new(), last_tag: 0, stopped: BTreeMap::new(), before: None });

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn pid(raw: u32) -> io::Result<Pid> {
    i32::try_from(raw)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{raw} is no process id")))
}

/// Makes this process the one that what its commands leave comes to. Fails
/// where leftovers cannot be found: `/proc` must be this PID namespace's, and
/// must list each thread's children ([`children`]).
pub fn adopt_orphans() -> io::Result<()> {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    if stat.split(' ').next() != Some(std::process::id().to_string().as_str()) {
        return Err(io::Error::other("/proc does not show this process's own PID namespace"));
    }
    let mut state = state();
    let children = listed_children()
        .map_err(|e| io::Error::other(format!("/proc does not list a thread's children: {e}")))?;
    if state.before.is_none() {
        let running = |child: &u32| state.running.iter().any(|(pid, _)| pid == child);
        state.before = Some(children.into_iter().filter(|child| !running(child)).collect());
    }
    Ok(())
}

/// A command's processes: its own and everything it starts. They end
/// together, with [`Tree::end`] or when the tree is dropped.
pub struct Tree {
    child: Child,
    tag: u64,
    ended: bool,
}

/// How a command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended {
    /// Its own process's.
    pub status: ExitStatus,
    /// Processes it had started that were still running when it ended, and
    /// were stopped with it: in the background, under `setsid`, `nohup` or
    /// `timeout`, daemons. Its own process is not counted.
    pub leftovers: u64,
}

/// Starts `command` as a [`Tree`] of its own (module documentation).
pub fn spawn(command: &mut Command) -> io::Result<Tree> {
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec and makes two system
    // calls (getpid, prctl): no allocation, lock or destructor.
    unsafe {
        command.pre_exec(|| {
            rustix::process::set_child_subreaper(Some(rustix::process::getpid())).map_err(io::Error::from)
        });
    }
    let mut state = state();
    state.last_tag += 1;
    let tag = state.last_tag;
    command.env(TAG, tag.to_string());
    let child = command.spawn()?;
    state.running.push((child.id(), tag));
    Ok(Tree { child, tag, ended: false })
}

/// Waits until the process `pid` of a [`Tree`] has exited, without reaping
/// it ([`Tree::end`] does): for a thread that tells the tree's owner.
pub fn wait_exited(pid: u32) -> io::Result<()> {
    let pid = self::pid(pid)?;
    loop {
        match rustix::process::waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Err(rustix::io::Errno::INTR) => {}
            done => return done.map(drop).map_err(io::Error::from),
        }
    }
}

impl Tree {
    /// The command's own process.
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Ends the command: its process is killed if it still runs, and nothing
    /// it started is left once this returns. How it ended, or why something
    /// could not be ended.
    pub fn end(mut self) -> io::Result<Ended> {
        self.ended = true;
        end(&mut self.child, self.tag)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if !self.ended
            && let Err(e) = end(&mut self.child, self.tag)
        {
            eprintln!("nucleus: a command's processes may still run: {e}");
        }
    }
}

fn end(child: &mut Child, tag: u64) -> io::Result<Ended> {
    let deadline = Instant::now() + END_WAIT;
    let status = reap(child, deadline);
    let mut state = state();
    if let Some(at) = state.running.iter().position(|(pid, _)| *pid == child.id()) {
        state.running.swap_remove(at);
    }
    let swept = sweep(&mut state, tag, deadline);
    let leftovers = state.stopped.remove(&tag).unwrap_or(0);
    // What ended commands left is counted until their own end takes it.
    let running: Vec<u64> = state.running.iter().map(|(_, tag)| *tag).collect();
    state.stopped.retain(|owner, _| running.contains(owner));
    drop(state);
    let status = status?;
    swept.map(|()| Ended { status, leftovers })
}

/// The command's own process, killed if it still runs, once it has ended.
fn reap(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    child.kill()?;
    loop {
        std::thread::sleep(POLL);
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the command's process did not end"));
        }
    }
}

/// What ended commands left among this process's `children`: all of them but
/// the running commands' own processes and the children it had before it ran
/// any (module documentation).
fn left_behind(children: Vec<(u32, bool)>, state: &State) -> Vec<(u32, bool)> {
    let running = |child: &u32| state.running.iter().any(|(pid, _)| pid == child);
    let before = |child: &u32| state.before.iter().flatten().any(|pid| pid == child);
    children.into_iter().filter(|(child, _)| !running(child) && !before(child)).collect()
}

/// Kills and reaps what ended commands left ([`left_behind`]) until nothing
/// is, counting each process that still ran for the command that started it
/// (`sweeper`'s when its environment does not say). Called with the state
/// locked.
fn sweep(state: &mut State, sweeper: u64, deadline: Instant) -> io::Result<()> {
    loop {
        let left = left_behind(children(std::process::id())?, state);
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let why = format!("{} processes are still left, more being started", left.len());
            return Err(io::Error::new(io::ErrorKind::TimedOut, why));
        }
        let mut killed = Vec::with_capacity(left.len());
        for (child, live) in left {
            if live {
                *state.stopped.entry(tag_of(child).unwrap_or(sweeper)).or_default() += 1;
            }
            // Still this process's child, a zombie at worst: only a sweep
            // reaps it, so its pid names nothing else.
            let child = pid(child)?;
            let _ = rustix::process::kill_process(child, Signal::KILL);
            killed.push(child);
        }
        for child in killed {
            loop {
                match rustix::process::waitpid(Some(child), WaitOptions::NOHANG) {
                    Ok(Some(_)) | Err(rustix::io::Errno::CHILD) => break,
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                    Ok(None) => {
                        let why = format!("process {} did not end", child.as_raw_nonzero());
                        return Err(io::Error::new(io::ErrorKind::TimedOut, why));
                    }
                    Err(rustix::io::Errno::INTR) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

/// The command a process was started by, as its environment names it.
fn tag_of(process: u32) -> Option<u64> {
    let environ = std::fs::read(format!("/proc/{process}/environ")).ok()?;
    let prefix = format!("{TAG}=");
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok())
}

/// The children of this process, `parent`, and whether each still runs (a
/// zombie does not).
///
/// Every process `/proc` shows is asked for its parent. `/proc` does not
/// show them all where it hides what its reader may not trace (`hidepid`, as
/// in the VM): a child that made itself undumpable, or runs a setuid, setgid
/// or capability program, is then missing, and any command can be one. So
/// the children the kernel lists for each of this process's threads are
/// added, which no mount option hides; one whose state cannot be read counts
/// as running.
fn children(parent: u32) -> io::Result<Vec<(u32, bool)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(process) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if let Some((of, running)) = parent_and_state(process)
            && of == parent
        {
            found.push((process, running));
        }
    }
    for process in listed_children()? {
        if !found.iter().any(|(known, _)| *known == process) {
            found.push((process, parent_and_state(process).is_none_or(|(_, running)| running)));
        }
    }
    Ok(found)
}

/// This process's children as the kernel lists them for each of its threads,
/// read from its own entry and so never hidden from it.
fn listed_children() -> io::Result<Vec<u32>> {
    let mut found = Vec::new();
    for thread in std::fs::read_dir("/proc/self/task")? {
        // A thread that ended since the listing: its children are another's.
        let Ok(listed) = std::fs::read_to_string(thread?.path().join("children")) else { continue };
        found.extend(listed.split_ascii_whitespace().filter_map(|pid| pid.parse::<u32>().ok()));
    }
    Ok(found)
}

/// The parent of `process` and whether it still runs, as its `stat` says;
/// `None` when that cannot be read (it is gone, or hidden).
fn parent_and_state(process: u32) -> Option<(u32, bool)> {
    let stat = std::fs::read(format!("/proc/{process}/stat")).ok()?;
    // After the command name, which may hold anything, bytes that are no
    // text included (a process names itself): state, then parent.
    let after = stat.iter().rposition(|byte| *byte == b')').map_or(&[][..], |at| &stat[at + 1..]);
    let mut fields = after.split(u8::is_ascii_whitespace).filter(|field| !field.is_empty());
    let (state, of) = (fields.next(), fields.next());
    let of = of.and_then(|of| std::str::from_utf8(of).ok()).and_then(|of| of.parse::<u32>().ok())?;
    Some((of, !matches!(state, Some(b"Z" | b"X"))))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Where `/proc` hides processes its reader may not trace, a command's
    /// leftover that made itself undumpable has no entry there. The sweep
    /// finds it all the same: the kernel lists a process's children in the
    /// process's own entry, whichever thread started them.
    #[test]
    fn the_kernel_lists_this_process_s_children_in_its_own_entry() {
        let mut here = Command::new("sleep").arg("30").spawn().unwrap();
        let mut elsewhere =
            std::thread::spawn(|| Command::new("sleep").arg("30").spawn().unwrap()).join().unwrap();
        let listed = listed_children().unwrap();
        for child in [&mut here, &mut elsewhere] {
            assert!(listed.contains(&child.id()), "{} is not among {listed:?}", child.id());
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// The sweep's rule over a process table. A service outside a sandbox has
    /// its supervisor's lifetime watcher as a child from before it ran any
    /// command, as another user, which it can neither kill nor reap: it is
    /// not what a command left, and neither is a running command's process.
    /// What came later is, running or a zombie.
    #[test]
    fn what_a_command_left_is_a_child_that_came_later_and_is_no_running_command() {
        let (watcher, command, daemon, zombie) = (10, 11, 12, 13);
        let state = State {
            running: vec![(command, 1)],
            last_tag: 1,
            stopped: BTreeMap::new(),
            before: Some(vec![watcher]),
        };
        let children = vec![(watcher, true), (command, true), (daemon, true), (zombie, false)];
        assert_eq!(left_behind(children, &state), [(daemon, true), (zombie, false)]);
    }
}
