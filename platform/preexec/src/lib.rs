//! The process plumbing that needs `unsafe`.
//!
//! - [`bind_parent_lifetime`] and [`bind_parent_lifetime_in_cgroup`]: a
//!   program started by the supervisor ends when the supervisor does.
//! - [`enter_cgroup`]: a program's first process is in its own cgroup before
//!   it executes anything.
//! - [`inherit_descriptors`]: the descriptors a sandbox is handed.
//! - [`reaper`]: everything a command started ends when the command ends,
//!   also what left its process group or session.
//!
//! **This crate holds the Linux pre-exec `unsafe` boundary** of the arrangement
//! (`CommandExt::pre_exec`, unsafe because the closure runs between fork and
//! exec, where only async-signal-safe work is allowed). What runs there is a
//! handful of system calls and, on failure, an error the standard library
//! turns into a failed spawn: no allocation that can block, no lock, and
//! never a silent pass. That is why it is a crate of its own, outside the
//! workspace's `unsafe_code = "forbid"`.

mod parent_lifetime;
pub use parent_lifetime::{bind_parent_lifetime, bind_parent_lifetime_in_cgroup};

pub mod reaper;

/// Moves the child `command` forks into the cgroup whose `cgroup.procs` is
/// `procs`, before anything else runs in it: before any exec, so before a
/// setuid helper changes its uid, and before the lifetime watcher forks, so
/// the watcher is in the cgroup too. A child that cannot be moved does not
/// run.
pub fn enter_cgroup(command: &mut std::process::Command, procs: std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec and makes one write(2) on
    // a descriptor this closure owns: no allocation, lock or destructor. "0"
    // names the writing process itself.
    unsafe {
        command.pre_exec(move || {
            let zero = b'0';
            if libc::write(procs.as_raw_fd(), (&zero as *const u8).cast(), 1) == 1 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// Lets the program `command` execs inherit `descriptors` at their current
/// numbers. They stay close-on-exec in this process, so no concurrently
/// spawned program can inherit them; they are closed here when `command` is
/// dropped. Pre-exec hooks registered earlier (the lifetime watcher) fork
/// before these flags change.
pub fn inherit_descriptors(command: &mut std::process::Command, descriptors: Vec<std::os::fd::OwnedFd>) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec and only calls fcntl on
    // descriptors this closure owns; no allocation, lock or destructor runs.
    unsafe {
        command.pre_exec(move || {
            for descriptor in &descriptors {
                if libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}
