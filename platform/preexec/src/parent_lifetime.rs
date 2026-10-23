//! A non-execing watchdog closes the AppArmor/bubblewrap bootstrap gap.
//!
//! AppArmor clears PDEATHSIG when exec changes profiles. Bubblewrap reinstalls
//! it only after cloning its namespace child, so a supervisor can die between
//! those operations. Keep an independent watcher of both original processes.
//!
//! Limits. Signals are blocked in the watcher, but nothing replaces it if
//! something outside kills only it (a SIGKILL, the OOM killer): the binding is
//! then gone, and only a sandbox's own `--die-with-parent` remains. It needs
//! `pidfd_open`, `close_range`, `poll` and a raw `clone`: where a host policy
//! refuses one, the program does not start or is killed at once; nothing falls
//! back to the parent-death signal alone.

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Bind a Linux sandbox and its process group to the launching process.
///
/// Before exec, establish a new session and a non-execing watchdog. The watcher
/// uses pidfds, survives exec security transitions, and kills the entire group
/// when either the owner or the sandbox launcher exits. It stays in that group
/// itself, pinning the group ID until the kill (which also terminates it).
///
/// The sandbox's namespace init MUST remain in this process group: do not give
/// bubblewrap `--new-session`. Killing init also destroys descendants that have
/// made their own process groups inside the PID namespace. This is not a
/// general containment mechanism for programs outside a PID namespace.
///
/// Setup failures refuse execution. The watcher retains no application pipes,
/// files or sockets, and acknowledges readiness before the launcher can exec.
pub fn bind_parent_lifetime(command: &mut Command) {
    let parent = std::process::id();
    // SAFETY: only async-signal-safe libc calls/raw syscalls and fixed stack
    // storage below. No allocation, Rust destructors, locks or atfork handlers
    // execute in the watchdog. The original child may report a fixed errno via
    // Command's normal exec-error pipe; the watchdog closes that pipe.
    unsafe { command.pre_exec(move || prepare(parent, -1)) };
}

/// [`bind_parent_lifetime`] for a sandbox that is a cgroup of its own: the
/// watcher also writes `1` to `cgroup_kill`, the sandbox's `cgroup.kill` opened
/// before the fork, which kills every process in it whatever its uid, where the
/// process-group kill reaches only the runtime's own. Register
/// [`crate::enter_cgroup`] before this, so the watcher itself is in the cgroup
/// and goes with it.
pub fn bind_parent_lifetime_in_cgroup(command: &mut Command, cgroup_kill: std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    let parent = std::process::id();
    // SAFETY: as for `bind_parent_lifetime`. The descriptor is owned by the
    // closure, so it stays open in this process until the command is dropped,
    // and the child inherits its number across the fork.
    unsafe { command.pre_exec(move || prepare(parent, cgroup_kill.as_raw_fd())) };
}

unsafe fn prepare(parent: u32, cgroup_kill: i32) -> std::io::Result<()> {
    unsafe {
        // The watcher never execs and must not run inherited application signal
        // handlers. Block signals across clone; only the launcher restores its
        // original mask. SIGKILL remains effective for group cleanup.
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        let mut original: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut blocked);
        if libc::sigprocmask(libc::SIG_SETMASK, &blocked, &mut original) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let owner = libc::syscall(libc::SYS_pidfd_open, parent, 0) as i32;
        if owner < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::getppid() as u32 != parent {
            libc::close(owner);
            return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
        }
        let group = libc::getpid();
        let launcher = libc::syscall(libc::SYS_pidfd_open, group, 0) as i32;
        if launcher < 0 {
            let error = std::io::Error::last_os_error();
            libc::close(owner);
            return Err(error);
        }
        let mut ready = [-1; 2];
        if libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) != 0 || libc::setsid() < 0 {
            let error = std::io::Error::last_os_error();
            libc::close(owner);
            libc::close(launcher);
            libc::close(ready[0]);
            libc::close(ready[1]);
            return Err(error);
        }
        // Own/reap the watcher even if the caller ignored SIGCHLD or installed
        // SA_NOCLDWAIT. This changes only the already-forked child. It prevents
        // auto-reaping and PID reuse on a failed readiness handshake.
        let mut disposition: libc::sigaction = std::mem::zeroed();
        disposition.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut disposition.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &disposition, std::ptr::null_mut()) != 0 {
            let error = std::io::Error::last_os_error();
            libc::close(owner);
            libc::close(launcher);
            libc::close(ready[0]);
            libc::close(ready[1]);
            return Err(error);
        }
        // Raw clone with fork semantics avoids libc's inherited pthread_atfork
        // handlers, which may lock a mutex belonging to a vanished thread.
        let watcher = libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) as i32;
        if watcher == 0 {
            watch(owner, launcher, ready[1], group, cgroup_kill);
        }
        let fork_error = std::io::Error::last_os_error();
        libc::close(owner);
        libc::close(launcher);
        libc::close(ready[1]);
        if watcher < 0 {
            libc::close(ready[0]);
            return Err(fork_error);
        }
        let admitted = libc::sigprocmask(libc::SIG_SETMASK, &original, std::ptr::null_mut()) == 0
            && await_ready(ready[0]);
        libc::close(ready[0]);
        if !admitted {
            // No exec happened, so there are no namespace descendants yet.
            libc::kill(watcher, libc::SIGKILL);
            while libc::waitpid(watcher, std::ptr::null_mut(), 0) < 0 && errno() == libc::EINTR {}
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}

unsafe fn await_ready(fd: i32) -> bool {
    unsafe {
        let mut started = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut started) != 0 {
            return false;
        }
        loop {
            let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) != 0 {
                return false;
            }
            let elapsed = (now.tv_sec - started.tv_sec) * 1000 + (now.tv_nsec - started.tv_nsec) / 1_000_000;
            if elapsed >= 5000 {
                return false;
            }
            let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let result = libc::poll(&mut poll, 1, (5000 - elapsed) as i32);
            if result < 0 && errno() == libc::EINTR {
                continue;
            }
            if result <= 0 {
                return false;
            }
            let mut byte = 0_u8;
            let count = libc::read(fd, (&mut byte as *mut u8).cast(), 1);
            if count < 0 && errno() == libc::EINTR {
                continue;
            }
            return count == 1 && byte == 1;
        }
    }
}

unsafe fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

unsafe fn watch(owner: i32, launcher: i32, ready: i32, group: i32, cgroup_kill: i32) -> ! {
    unsafe {
        // These descriptors, and the cgroup's kill file when there is one, are
        // the ONLY ones the watchdog keeps. In particular it releases
        // stdin/stdout/stderr and the Rust spawn-error pipe, otherwise a
        // successful spawn would hang. The kill file was opened before the
        // fork, so its number may come first: sort without allocating.
        let mut keep = [owner as u32, launcher as u32, ready as u32, ready as u32];
        if cgroup_kill >= 0 {
            keep[3] = cgroup_kill as u32;
        }
        for i in 1..keep.len() {
            let mut j = i;
            while j > 0 && keep[j - 1] > keep[j] {
                keep.swap(j - 1, j);
                j -= 1;
            }
        }
        let mut first = 0;
        for fd in keep {
            if first < fd && libc::syscall(libc::SYS_close_range, first, fd - 1, 0) != 0 {
                libc::_exit(127);
            }
            first = fd + 1;
        }
        if libc::syscall(libc::SYS_close_range, first, u32::MAX, 0) != 0 {
            libc::_exit(127);
        }
        let byte = 1_u8;
        let written = loop {
            let count = libc::write(ready, (&byte as *const u8).cast(), 1);
            if count >= 0 || errno() != libc::EINTR {
                break count;
            }
        };
        libc::close(ready);
        if written != 1 {
            libc::_exit(127);
        }
        let mut fds = [
            libc::pollfd { fd: owner, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: launcher, events: libc::POLLIN, revents: 0 },
        ];
        loop {
            let result = libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1);
            if result < 0 && errno() == libc::EINTR {
                continue;
            }
            // pidfd exit, an invalid descriptor or any poll error all fail
            // closed. The watcher pins this process group, so its numeric ID
            // cannot have been reassigned even if the launcher was reaped. In
            // a cgroup of its own the cgroup goes first: it also holds what
            // runs as another uid, which no signal from here may reach.
            if cgroup_kill >= 0 {
                let one = b'1';
                libc::write(cgroup_kill, (&one as *const u8).cast(), 1);
            }
            libc::kill(-group, libc::SIGKILL);
            libc::_exit(127);
        }
    }
}
