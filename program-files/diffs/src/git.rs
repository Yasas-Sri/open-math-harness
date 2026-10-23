//! Running `/usr/bin/git` as a child: a fixed environment, overrides that
//! keep it from running configured programs we can switch off, a hard
//! deadline, and bounded reading of what it prints.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use nucleus_protocol::{Error, ErrorCode};

/// The only git this service runs; never looked up on `PATH`.
pub const GIT: &str = "/usr/bin/git";

/// The longest one git run may take, whatever the caller's deadline.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(20);

/// What is kept of git's standard error for an error message.
const STDERR_KEPT: usize = 2048;
const CHUNK: usize = 64 * 1024;

/// Given before every subcommand, and handed on to any git that git starts.
/// Command-line configuration outranks the repository's own and the person's
/// global one, so these hold whatever either says: no file-system monitor
/// hook, no untracked cache to write, raw paths, any owner, no external diff
/// program, a changed submodule shown as its two commits (the other formats
/// start a git inside the submodule, which runs the submodule's own diff
/// programs), and `git diff` never rewrites the index to save stat
/// information (it does so otherwise, even with optional locks off).
const OVERRIDES: &[&str] = &[
    "--no-pager",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.quotepath=false",
    "-c",
    "safe.directory=*",
    "-c",
    "diff.external=",
    "-c",
    "diff.submodule=short",
    "-c",
    "diff.autoRefreshIndex=false",
];

/// Whether a stream consumer wants more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    More,
    Stop,
}

/// How a git run ended.
#[derive(Debug)]
pub struct Exit {
    pub code: Option<i32>,
    /// The start of its standard error, for messages.
    pub stderr: String,
}

impl Exit {
    /// A service error naming the subcommand, its status and what it said.
    pub fn failure(&self, what: &str) -> Error {
        let code = self.code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"));
        let said = self.stderr.trim();
        let said = if said.is_empty() { String::new() } else { format!(": {said}") };
        Error::new(ErrorCode::ServiceError, format!("git {what} ended with {code}{said}"))
    }
}

enum Message {
    Out(Vec<u8>),
    OutEnd,
    Err(Vec<u8>),
    ErrEnd,
    Exited(std::io::Result<ExitStatus>),
}

/// Runs git with an environment of its own: nothing inherited, no system
/// configuration, the person's global configuration only from the home the
/// operator names, no prompts, no optional locks, no lazy fetch.
pub struct Git {
    /// An empty directory this service owns: git's `HOME` without the
    /// person's home, and `own/tmp` its `TMPDIR`.
    own: tempfile::TempDir,
    scratch_path: PathBuf,
    /// The person's home: its global configuration and excludes count, as
    /// they do for the person's own git.
    home: Option<PathBuf>,
    /// Git never looks for a repository above the workspace.
    ceiling: OsString,
}

impl Git {
    pub fn new(workspace: &Path, home: Option<&Path>) -> Result<Self, String> {
        let own = tempfile::Builder::new()
            .prefix("nucleus-diffs")
            .tempdir()
            .map_err(|e| format!("cannot create git's empty home: {e}"))?;
        let scratch_path = own.path().join("tmp");
        std::fs::create_dir(&scratch_path)
            .map_err(|e| format!("cannot create git's temporary directory: {e}"))?;
        Ok(Self {
            own,
            scratch_path,
            home: home.map(Path::to_owned),
            ceiling: workspace.parent().unwrap_or(workspace).as_os_str().to_owned(),
        })
    }

    fn command(&self, repository: BorrowedFd<'_>, args: &[&OsStr], stdin: bool) -> Result<Command, Error> {
        let mut command = Command::new(GIT);
        command
            .args(OVERRIDES)
            .args(args)
            // The directory that was opened and checked, not a name that could
            // have been replaced since: the child changes into the descriptor.
            .current_dir(format!("/proc/self/fd/{}", repository.as_raw_fd()))
            .env_clear()
            .env("GIT_DIR", ".git")
            .env("GIT_WORK_TREE", ".")
            .env("GIT_CEILING_DIRECTORIES", &self.ceiling)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("TMPDIR", &self.scratch_path)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C")
            .env("PATH", "/usr/bin:/bin")
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match &self.home {
            Some(home) => command.env("HOME", home).env("XDG_CONFIG_HOME", home.join(".config")),
            None => command.env("HOME", self.own.path()).env("GIT_CONFIG_GLOBAL", "/dev/null"),
        };
        {
            use std::os::unix::process::CommandExt;
            // Its own process group, so what it starts ends with it.
            command.process_group(0);
        }
        Ok(command)
    }

    /// Runs git in `repository`, handing its standard output to `sink` as it
    /// arrives. Returns `None` when `sink` stopped it early (the rest is not
    /// read and the process group is killed), otherwise how it ended. Past
    /// `deadline`, or [`RUN_TIMEOUT`], the group is killed and the run is a
    /// timeout.
    pub fn stream(
        &self,
        repository: BorrowedFd<'_>,
        args: &[&OsStr],
        stdin: Option<Vec<u8>>,
        deadline: Instant,
        sink: &mut dyn FnMut(&[u8]) -> Result<Flow, Error>,
    ) -> Result<Option<Exit>, Error> {
        let what = args.first().map_or_else(String::new, |a| a.to_string_lossy().into_owned());
        let started = Instant::now();
        let limit = deadline.min(started + RUN_TIMEOUT);
        if limit <= started {
            return Err(timeout(&what));
        }
        let mut child = self
            .command(repository, args, stdin.is_some())?
            .spawn()
            .map_err(|e| Error::new(ErrorCode::ServiceError, format!("cannot run {GIT}: {e}")))?;
        let group = rustix::process::Pid::from_child(&child);
        let (tx, rx) = mpsc::channel();
        if let (Some(mut pipe), Some(input)) = (child.stdin.take(), stdin) {
            // Written apart from the reading, so neither side can fill a pipe
            // the other is not emptying.
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, tx.clone(), Message::Out, Message::OutEnd);
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, tx.clone(), Message::Err, Message::ErrEnd);
        }
        std::thread::spawn(move || {
            let _ = tx.send(Message::Exited(child.wait()));
        });

        let kill = || {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        };
        let (mut out_open, mut err_open, mut exited) = (true, true, None);
        let mut stderr = Vec::new();
        let mut stopped = false;
        loop {
            // Stopped on purpose: only the exit matters now, not what is left
            // in the pipes (a descendant that escaped the group could hold them).
            if exited.is_some() && (stopped || (!out_open && !err_open)) {
                break;
            }
            let now = Instant::now();
            let message = match rx.recv_timeout(limit.saturating_duration_since(now)) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    kill();
                    return Err(timeout(&what));
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match message {
                Message::Out(chunk) if !stopped => match sink(&chunk) {
                    Ok(Flow::More) => {}
                    Ok(Flow::Stop) => {
                        stopped = true;
                        kill();
                    }
                    Err(e) => {
                        kill();
                        return Err(e);
                    }
                },
                Message::Out(_) => {}
                Message::OutEnd => out_open = false,
                Message::Err(chunk) => {
                    let room = STDERR_KEPT.saturating_sub(stderr.len());
                    stderr.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Message::ErrEnd => err_open = false,
                Message::Exited(status) => {
                    exited = Some(status);
                    // Git is gone; anything it left running in its group goes
                    // too, which also closes pipes a leftover could hold.
                    kill();
                }
            }
        }
        kill();
        if stopped {
            return Ok(None);
        }
        let status = exited
            .ok_or_else(|| Error::new(ErrorCode::ServiceError, format!("git {what}: lost its status")))?
            .map_err(|e| Error::new(ErrorCode::ServiceError, format!("git {what}: wait: {e}")))?;
        Ok(Some(Exit { code: status.code(), stderr: String::from_utf8_lossy(&stderr).into_owned() }))
    }

    /// Runs git and collects its standard output. More than `limit` bytes is
    /// an error, never a shortened answer.
    pub fn output(
        &self,
        repository: BorrowedFd<'_>,
        args: &[&OsStr],
        stdin: Option<Vec<u8>>,
        limit: usize,
        deadline: Instant,
    ) -> Result<(Exit, Vec<u8>), Error> {
        let what = args.first().map_or_else(String::new, |a| a.to_string_lossy().into_owned());
        let mut out = Vec::new();
        let mut sink = |chunk: &[u8]| {
            if out.len() + chunk.len() > limit {
                return Err(Error::new(
                    ErrorCode::ServiceError,
                    format!("git {what} printed more than {limit} bytes, more than this service reads"),
                ));
            }
            out.extend_from_slice(chunk);
            Ok(Flow::More)
        };
        let exit = self.stream(repository, args, stdin, deadline, &mut sink)?;
        let exit = exit.ok_or_else(|| Error::new(ErrorCode::ServiceError, "git output stopped"))?;
        Ok((exit, out))
    }
}

fn timeout(what: &str) -> Error {
    Error::new(ErrorCode::Timeout, format!("git {what} did not finish in time"))
}

/// Reads a pipe to its end on its own thread. Once nobody listens, the rest is
/// drained and dropped so the writer never blocks on a full pipe.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    tx: Sender<Message>,
    chunk: fn(Vec<u8>) -> Message,
    end: Message,
) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk(buffer[..n].to_vec())).is_err() {
                        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
                        return;
                    }
                }
            }
        }
        let _ = tx.send(end);
    });
}
