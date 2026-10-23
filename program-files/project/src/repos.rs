//! The project's repository, with git (`git.rs`): an empty repository in a
//! new home, and the check of a repository URL.

use std::ffi::OsStr;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use nucleus_projects_interface::{MAX_REMOTE_URL_BYTES, REPOSITORY};
use nucleus_protocol::{Error, ErrorCode};
use rustix::fs::{AtFlags, Mode, RenameFlags};

use crate::git::Git;
use crate::home::{Home, errno, fresh_name, open_dir};

/// Most bytes of git's answer to `init`.
const MAX_SHORT: usize = 4096;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

fn os<'a>(args: &[&'a str]) -> Vec<&'a OsStr> {
    args.iter().map(|a| OsStr::new(*a)).collect()
}

/// Refuses a folder that already holds a repository.
fn fresh(home: &Home) -> Result<(), Error> {
    match rustix::fs::statat(home.fd(), REPOSITORY, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(errno(REPOSITORY, e)),
        Ok(_) => Err(invalid("the project has a repository already")),
    }
}

/// An empty repository: `repo/`, `git init`, initial branch `main`, no
/// commit, no hooks or templates.
pub fn init(git: &Git, home: &Home, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
    fresh(home)?;
    let staging = home.staging("clones")?;
    let name = fresh_name("init");
    rustix::fs::mkdirat(&staging, name.as_str(), Mode::from_raw_mode(0o777))
        .map_err(|e| errno("the new repository", e))?;
    let dir = open_dir(&staging, &name).map_err(|e| errno("the new repository", e))?;
    let out = git.run(
        dir.as_fd(),
        &os(&["init", "--quiet", "--template=", "--initial-branch=main"]),
        MAX_SHORT,
        deadline,
        cancel,
    );
    finish(
        home,
        &staging,
        &name,
        out.and_then(|out| if out.ok() { Ok(()) } else { Err(out.failure("init")) }),
    )
}

/// Checks a repository URL (`git_remote`'s): `https://` with a host, no user
/// or password, no query or fragment, nothing a shell or git would read
/// otherwise.
pub fn check_url(url: &str) -> Result<(), Error> {
    let refuse = |why: &str| Err(invalid(format!("the repository URL {why}")));
    if url.len() > MAX_REMOTE_URL_BYTES {
        return refuse(&format!("is over {MAX_REMOTE_URL_BYTES} bytes"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refuse("contains whitespace or control characters");
    }
    let Some(rest) = url.strip_prefix("https://") else { return refuse("must start with https://") };
    if url.contains(['?', '#']) {
        return refuse("must have no query or fragment");
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return refuse("must name a host, with no user or password");
    }
    Ok(())
}

/// Publishes the staged repository as `repo/`, or removes it.
fn finish(home: &Home, staging: &OwnedFd, name: &str, outcome: Result<(), Error>) -> Result<(), Error> {
    let published = outcome.and_then(|()| {
        rustix::fs::renameat_with(staging, name, home.fd(), REPOSITORY, RenameFlags::NOREPLACE)
            .map_err(|e| errno("the repository", e))
    });
    if published.is_err() {
        let dir = cap_std::fs::Dir::from_std_file(std::fs::File::from(
            staging.try_clone().map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))?,
        ));
        let _ = dir.remove_dir_all(name);
    }
    published
}
