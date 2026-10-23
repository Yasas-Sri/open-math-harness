//! Running `/usr/bin/git` for the project: in a directory this host opened,
//! with an environment of its own, its own process group, a hard deadline, a
//! way to cancel, and bounded reading of what it prints.
//!
//! The host runs as the projects' user, so what git runs on its behalf (a
//! filter the repository configures, say) is the project's own code with
//! the projects' reach; hooks are switched off only so that the window's
//! operations do the same thing every time.

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nucleus_protocol::{Error, ErrorCode};

/// The only git this host runs; never looked up on `PATH`.
pub const GIT: &str = "/usr/bin/git";

/// Given before every subcommand.
const OVERRIDES: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "safe.directory=*",
    "-c",
    "advice.detachedHead=false",
    // Only https, with no credentials and no redirect.
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "credential.helper=",
    "-c",
    "http.followRedirects=false",
];

/// Most bytes kept of what git prints on standard error, for messages.
const STDERR_KEPT: usize = 4096;

/// How a git run ended.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    /// What it printed on standard output, at most the run's limit.
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// An error naming the subcommand and git's own words.
    pub fn failure(&self, what: &str) -> Error {
        let said = self.stderr.trim();
        let status = self.code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"));
        Error::new(ErrorCode::ServiceError, format!("git {what} ended with {status}: {said}"))
    }
}

/// Runs git for the project whose home this host serves.
pub struct Git {
    home: PathBuf,
    tmp: PathBuf,
}

impl Git {
    pub fn new(home: PathBuf, tmp: PathBuf) -> Self {
        Self { home, tmp }
    }

    /// Runs `git <args>` in `dir`, keeping at most `limit` bytes of its
    /// standard output (more is an error). Past `deadline`, or once `cancel`
    /// is set, its process group is killed. Only `https` transports are allowed.
    pub fn run(
        &self,
        dir: BorrowedFd<'_>,
        args: &[&OsStr],
        limit: usize,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Output, Error> {
        self.run_with(dir, args, Some(limit), deadline, cancel, "https")
    }

    /// Reads all output of Git metadata commands, without a product size quota.
    pub fn run_complete(
        &self,
        dir: BorrowedFd<'_>,
        args: &[&OsStr],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Output, Error> {
        self.run_with(dir, args, None, deadline, cancel, "https")
    }

    /// Reads a staged bundle with local transport alone and complete output.
    pub fn run_local(
        &self,
        dir: BorrowedFd<'_>,
        args: &[&OsStr],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Output, Error> {
        self.run_with(dir, args, None, deadline, cancel, "file")
    }

    fn run_with(
        &self,
        dir: BorrowedFd<'_>,
        args: &[&OsStr],
        limit: Option<usize>,
        deadline: Instant,
        cancel: &AtomicBool,
        protocols: &str,
    ) -> Result<Output, Error> {
        let what = args.first().map_or_else(String::new, |a| a.to_string_lossy().into_owned());
        let mut command = Command::new(GIT);
        command
            .args(OVERRIDES)
            .args(args)
            .current_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // Overrides every `protocol.*.allow` setting.
            .env("GIT_ALLOW_PROTOCOL", protocols)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| Error::new(ErrorCode::ServiceError, format!("cannot run {GIT}: {e}")))?;
        let group = rustix::process::Pid::from_child(&child);
        let kill = || {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        };
        let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
            kill();
            return Err(Error::new(ErrorCode::ServiceError, "git's output could not be read"));
        };
        let reader = std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buffer = [0u8; 16 * 1024];
            let mut over = false;
            loop {
                match out.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if limit.is_none_or(|limit| n <= limit.saturating_sub(kept.len())) => {
                        kept.extend_from_slice(&buffer[..n])
                    }
                    Ok(_) => over = true,
                }
            }
            (kept, over)
        });
        let errors = std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut err).take(STDERR_KEPT as u64).read_to_end(&mut kept);
            let _ = std::io::copy(&mut err, &mut std::io::sink());
            String::from_utf8_lossy(&kept).into_owned()
        });
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(child.wait());
        });
        let status = loop {
            let now = Instant::now();
            if now >= deadline || cancel.load(Ordering::SeqCst) {
                kill();
                let why =
                    if cancel.load(Ordering::SeqCst) { ErrorCode::Cancelled } else { ErrorCode::Timeout };
                return Err(Error::new(why, format!("git {what} was stopped before it finished")));
            }
            match waited.recv_timeout(deadline.saturating_duration_since(now).min(Duration::from_millis(100)))
            {
                Ok(status) => break status,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    kill();
                    return Err(Error::new(ErrorCode::ServiceError, format!("git {what}: lost its status")));
                }
            }
        };
        // Whatever git left running in its group goes with it.
        kill();
        let status = status.map_err(|e| Error::new(ErrorCode::ServiceError, format!("git {what}: {e}")))?;
        let (stdout, over) = reader.join().unwrap_or_default();
        if over {
            return Err(Error::new(
                ErrorCode::ServiceError,
                format!(
                    "git {what} printed more than {} bytes, more than this host reads",
                    limit.unwrap_or_default()
                ),
            ));
        }
        let stderr = errors.join().unwrap_or_default();
        Ok(Output { code: status.code(), stdout, stderr })
    }
}
