//! Background jobs: a command that keeps running after its
//! call has answered, in an instance started with `--jobs on` (a project's
//! shell host). Only the projects service names jobs and drives them.
//!
//! A command becomes a job when its call asks for it (`background`) or when
//! the projects service moves it there (`detach`). Its call then answers at
//! once and the job takes over the command's processes (its
//! [`Tree`](nucleus_preexec::reaper::Tree)) and its output: the output goes
//! to `<root>/.nucleus/jobs/<id>.log`, at most [`wire::MAX_OUTPUT_BYTES`],
//! and its last lines stay in memory for `wait`. The job ends when the
//! command does, when it is stopped, or at the time limit its command named,
//! if any; then its whole tree is killed, as a foreground command's is when
//! it ends. Output files are kept: the agent or a person removes them. A job does not take
//! one of the `--max-inflight` slots: the projects service bounds how many
//! run.
//!
//! The project's own commands can write its folder, the jobs directory
//! included: every file here is opened beneath the root with no link
//! followed, so a link planted there names nothing outside it.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use nucleus_preexec::reaper::Tree;
use nucleus_protocol::{Error, ErrorCode};
use nucleus_shell_interface::jobs::{self as wire, State};
use nucleus_shell_interface::{JobStatus, OutputChunk};

use crate::{Flow, Wake};

/// The last output kept in memory, so the tail can start at a line.
const TAIL_KEEP: usize = wire::TAIL_BYTES * 2;
/// Ended jobs remembered for a late `wait` or `stop`.
const KEPT_ENDED: usize = 64;
/// How long a job's end waits for its output to reach the file.
const FLUSH_WAIT: Duration = Duration::from_secs(2);
/// Longest one `wait` holds its call.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(60);
/// How long a `stop` waits for the job to end before it answers.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(15);

fn refuse(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What a command's call hands a job: everything the command is.
pub(crate) struct Running {
    pub tree: Tree,
    /// Its output, as its readers send it; nothing has been taken yet.
    pub collect: Receiver<Vec<u8>>,
    /// How much of it waits to be taken, which bounds what the readers send.
    pub flow: Arc<Flow>,
    /// Its exit, and the job's stop, arrive here.
    pub woken: Receiver<Wake>,
    /// Sends to `woken`.
    pub wake: Sender<Wake>,
    pub started: Instant,
    /// Its time limit, from `started`; `None`, none.
    pub limit: Option<Duration>,
}

/// The next wake-up on `woken`, or a timeout at `deadline` if there is one.
pub(crate) fn recv_until(
    woken: &Receiver<Wake>,
    deadline: Option<Instant>,
) -> Result<Wake, RecvTimeoutError> {
    match deadline {
        Some(deadline) => woken.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        None => woken.recv().map_err(|_| RecvTimeoutError::Disconnected),
    }
}

/// One job.
struct Job {
    stop: Sender<Wake>,
    live: Mutex<Live>,
    changed: Condvar,
}

/// How a job is doing.
struct Live {
    started: Instant,
    state: State,
    exit_code: Option<i32>,
    /// What its command left running when it exited.
    leftovers: Option<u64>,
    /// When it ended, since `started`.
    ended_after: Option<Duration>,
    /// Bytes in its file.
    size: u64,
    /// Its last output.
    tail: VecDeque<u8>,
}

impl Live {
    fn status(&self) -> JobStatus {
        JobStatus {
            state: self.state,
            exit_code: self.exit_code,
            stopped_leftovers: self.leftovers,
            elapsed_ms: millis(self.ended_after.unwrap_or_else(|| self.started.elapsed())),
            size: self.size,
            tail: tail_text(&self.tail),
        }
    }
}

/// The last lines of `kept`: at most [`wire::TAIL_BYTES`], from the start of
/// a line when there is more.
pub(crate) fn tail_text(kept: &VecDeque<u8>) -> String {
    let bytes: Vec<u8> = kept.iter().copied().collect();
    let mut from = bytes.len().saturating_sub(wire::TAIL_BYTES);
    if from > 0 {
        // The line break that ends the output starts no line: a last line
        // longer than the tail is shown from its last bytes.
        let lines = &bytes[from..bytes.len() - 1];
        from = lines.iter().position(|b| *b == b'\n').map_or(from, |at| from + at + 1);
    }
    String::from_utf8_lossy(&bytes[from..]).into_owned()
}

/// The jobs of one shell instance.
pub(crate) struct Jobs {
    root: PathBuf,
    table: Mutex<Table>,
}

#[derive(Default)]
struct Table {
    live: HashMap<String, Arc<Job>>,
    ended: VecDeque<(String, JobStatus)>,
}

impl Jobs {
    pub(crate) fn new(root: &Path) -> Self {
        Self { root: root.to_owned(), table: Mutex::new(Table::default()) }
    }

    /// The file a job's output goes to, as the instance's commands name it.
    pub(crate) fn output_path(&self, id: &str) -> String {
        format!("{}/{}/{id}.log", self.root.display(), wire::OUTPUT_DIR)
    }

    /// Makes `running` the job `id`: its output goes to the job's file from
    /// the command's first byte, and the job ends it as the module says.
    pub(crate) fn adopt(self: &Arc<Self>, id: &str, running: Running) -> Result<(), Error> {
        if !wire::valid_id(id) {
            return Err(refuse(format!("{id:?} is no job id")));
        }
        {
            let table = lock(&self.table);
            if table.live.contains_key(id) || table.ended.iter().any(|(ended, _)| ended == id) {
                return Err(refuse(format!("job {id} exists already")));
            }
        }
        // Without its file the job still runs; its tail is what is kept.
        let file =
            self.create_output(id).map_err(|e| eprintln!("nucleus-shell: job {id}'s output file: {e}")).ok();
        let Running { tree, collect, flow, woken, wake, started, limit } = running;
        let job = Arc::new(Job {
            stop: wake,
            live: Mutex::new(Live {
                started,
                state: State::Running,
                exit_code: None,
                leftovers: None,
                ended_after: None,
                size: 0,
                tail: VecDeque::new(),
            }),
            changed: Condvar::new(),
        });
        lock(&self.table).live.insert(id.to_owned(), Arc::clone(&job));
        let (flushed, written) = mpsc::channel();
        let draining = Arc::clone(&job);
        std::thread::spawn(move || drain(collect, &flow, file, &draining, &flushed));
        let (jobs, id) = (Arc::clone(self), id.to_owned());
        let deadline = limit.map(|limit| started + limit);
        std::thread::spawn(move || {
            let state = loop {
                match recv_until(&woken, deadline) {
                    Ok(Wake::Exited) | Err(RecvTimeoutError::Disconnected) => break State::Exited,
                    Ok(Wake::Stop) => break State::Stopped,
                    Ok(Wake::Withdrawn | Wake::Detach) => {}
                    Err(RecvTimeoutError::Timeout) => break State::TimedOut,
                }
            };
            // Whatever still runs ends now, the command's process first.
            let (exit_code, leftovers) = match tree.end() {
                Ok(ended) if state == State::Exited => {
                    (ended.status.code(), (ended.leftovers > 0).then_some(ended.leftovers))
                }
                Ok(_) => (None, None),
                Err(e) => {
                    eprintln!("nucleus-shell: job {id}'s processes may still run: {e}");
                    (None, None)
                }
            };
            let _ = written.recv_timeout(FLUSH_WAIT);
            {
                let mut live = lock(&job.live);
                live.state = state;
                live.exit_code = exit_code;
                live.leftovers = leftovers;
                live.ended_after = Some(started.elapsed());
            }
            job.changed.notify_all();
            jobs.retire(&id, &job);
        });
        Ok(())
    }

    /// Moves an ended job from the running ones to the remembered ones.
    fn retire(&self, id: &str, job: &Job) {
        let status = lock(&job.live).status();
        let mut table = lock(&self.table);
        table.live.remove(id);
        table.ended.push_back((id.to_owned(), status));
        while table.ended.len() > KEPT_ENDED {
            table.ended.pop_front();
        }
    }

    /// The job's status once it has ended, or after `wait`.
    pub(crate) fn wait(&self, id: &str, wait: Duration) -> Result<JobStatus, Error> {
        let job = {
            let table = lock(&self.table);
            match table.live.get(id) {
                Some(job) => Arc::clone(job),
                None => return Self::ended(&table, id),
            }
        };
        let live = lock(&job.live);
        let (live, _) = job
            .changed
            .wait_timeout_while(live, wait.min(MAX_WAIT), |live| live.state == State::Running)
            .unwrap_or_else(PoisonError::into_inner);
        Ok(live.status())
    }

    fn ended(table: &Table, id: &str) -> Result<JobStatus, Error> {
        table
            .ended
            .iter()
            .find(|(ended, _)| ended == id)
            .map(|(_, status)| status.clone())
            .ok_or_else(|| refuse(format!("this shell has no job {id}")))
    }

    /// Ends a running job: its whole tree is killed. Its status once it has
    /// ended, or as it is after [`STOP_WAIT`].
    pub(crate) fn stop(&self, id: &str) -> Result<JobStatus, Error> {
        let job = {
            let table = lock(&self.table);
            match table.live.get(id) {
                Some(job) => Arc::clone(job),
                None => return Self::ended(&table, id),
            }
        };
        let _ = job.stop.send(Wake::Stop);
        self.wait(id, STOP_WAIT)
    }

    /// A piece of a job's output file: `after` onwards, or its end.
    pub(crate) fn output(&self, id: &str, after: Option<u64>) -> Result<OutputChunk, Error> {
        if !wire::valid_id(id) {
            return Err(refuse(format!("{id:?} is no job id")));
        }
        let opened = self
            .dir(false)
            .and_then(|dir| open_beneath(&dir, &format!("{id}.log"), rustix::fs::OFlags::RDONLY, false));
        let mut file = match opened {
            Ok(fd) => File::from(fd),
            // Nothing removes an output file but the project's own commands:
            // that is said, not refused.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OutputChunk { text: String::new(), start: 0, next: 0, size: 0, removed: true });
            }
            Err(e) => return Err(Error::new(ErrorCode::ServiceError, format!("job {id}'s output: {e}"))),
        };
        let failed =
            |e: std::io::Error| Error::new(ErrorCode::ServiceError, format!("job {id}'s output: {e}"));
        let size = file.metadata().map_err(failed)?.len();
        let (mut start, most) = match after {
            None => (size.saturating_sub(wire::OUTPUT_TAIL_BYTES), wire::OUTPUT_TAIL_BYTES),
            Some(after) => (after.min(size), wire::OUTPUT_PAGE_BYTES),
        };
        file.seek(SeekFrom::Start(start)).map_err(failed)?;
        let mut bytes = Vec::new();
        file.take(most).read_to_end(&mut bytes).map_err(failed)?;
        // A page that more follows, or may follow while the job runs, ends
        // between characters: the next one starts with the character this
        // one would have cut. The end of the output starts at a character.
        if start + (bytes.len() as u64) < size || lock(&self.table).live.contains_key(id) {
            bytes.truncate(whole_chars(&bytes));
        }
        if after.is_none() && start > 0 {
            let cut = bytes.iter().take(3).take_while(|byte| **byte & 0xC0 == 0x80).count();
            bytes.drain(..cut);
            start += cut as u64;
        }
        let next = start + bytes.len() as u64;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(OutputChunk { text, start, next, size, removed: false })
    }

    /// The jobs directory, opened (readable) beneath the root with no link
    /// followed; made when `create`.
    fn dir(&self, create: bool) -> std::io::Result<OwnedFd> {
        use rustix::fs::{Mode, OFlags, ResolveFlags};
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        let mut fd =
            rustix::fs::open(&self.root, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        for segment in wire::OUTPUT_DIR.split('/') {
            if create {
                match rustix::fs::mkdirat(&fd, segment, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            fd = rustix::fs::openat2(&fd, segment, flags, Mode::empty(), resolve)?;
        }
        Ok(fd)
    }

    /// A new, empty output file for `id`, locked for as long as its job
    /// writes it.
    fn create_output(&self, id: &str) -> std::io::Result<File> {
        use rustix::fs::{FlockOperation, OFlags};
        let dir = self.dir(true)?;
        let file =
            open_beneath(&dir, &format!("{id}.log"), OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, true)?;
        rustix::fs::flock(&file, FlockOperation::LockExclusive)?;
        Ok(File::from(file))
    }
}

/// The length of `bytes` without a UTF-8 character that its end cuts short.
fn whole_chars(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let needs = match bytes[n - back] {
            0x80..=0xBF => continue,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if needs > back { n - back } else { n };
    }
    n
}

/// Opens the regular file `name` in `dir` with no link followed (a new file
/// 0600). The project's commands can write the directory: a pipe or a
/// device planted under a job's name is refused, and opening never waits
/// for one's other end.
fn open_beneath(
    dir: &OwnedFd,
    name: &str,
    flags: rustix::fs::OFlags,
    create: bool,
) -> std::io::Result<OwnedFd> {
    use rustix::fs::{FileType, Mode, OFlags};
    let mode = if create { Mode::from_raw_mode(0o600) } else { Mode::empty() };
    let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let file = rustix::fs::openat(dir, name, flags, mode)?;
    if FileType::from_raw_mode(rustix::fs::fstat(&file)?.st_mode) != FileType::RegularFile {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name} is no regular file"),
        ));
    }
    Ok(file)
}

/// Writes a job's output to its file, up to the cap, and keeps its tail,
/// until every writer of its pipe is gone; then says so on `flushed`.
fn drain(collect: Receiver<Vec<u8>>, flow: &Flow, mut file: Option<File>, job: &Job, flushed: &Sender<()>) {
    const CUT: &[u8] = b"\n[nucleus: the job's output past 8 MiB was not kept]\n";
    let mut cut = false;
    for chunk in collect {
        flow.taken(chunk.len());
        let size = lock(&job.live).size;
        let room = usize::try_from(wire::MAX_OUTPUT_BYTES.saturating_sub(size)).unwrap_or(usize::MAX);
        let keep = chunk.len().min(room);
        let mut written = 0;
        if let Some(out) = file.as_mut() {
            if keep > 0 && out.write_all(&chunk[..keep]).is_ok() {
                written += keep;
            }
            if keep < chunk.len() && !std::mem::replace(&mut cut, true) && out.write_all(CUT).is_ok() {
                written += CUT.len();
            }
        }
        let mut live = lock(&job.live);
        live.size += written as u64;
        live.tail.extend(&chunk);
        let excess = live.tail.len().saturating_sub(TAIL_KEEP);
        live.tail.drain(..excess);
    }
    let _ = flushed.send(());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Pages of a job's output are joined by the window: none may end inside
    /// a character, or each page boundary would show as broken text.
    #[test]
    fn a_page_of_output_ends_between_characters() {
        let text = "ab\u{20ac}\u{1f600}".as_bytes();
        let cut: Vec<usize> = (0..=text.len()).map(|end| whole_chars(&text[..end])).collect();
        assert_eq!(cut, [0, 1, 2, 2, 2, 5, 5, 5, 5, 9]);
        assert_eq!(whole_chars(&[b'a', 0xFF]), 2, "bytes that are no text are kept");
    }

    /// The tail starts at a line when it can, and is never empty because the
    /// output's last line is longer than it.
    #[test]
    fn the_tail_of_one_long_line_is_its_end() {
        let tail = |text: String| tail_text(&text.into_bytes().into_iter().collect());
        assert_eq!(tail(format!("{}\nlast line\n", "x".repeat(4000))), "last line\n");
        let long = tail(format!("{}\n", "9".repeat(4000)));
        assert_eq!((long.len(), long.ends_with("9\n")), (wire::TAIL_BYTES, true));
    }

    /// The project's commands can write the jobs directory. A pipe planted
    /// under a job's name is not opened as its output: reading it is
    /// refused at once instead of waiting for a writer for ever. A file they
    /// removed is answered as removed.
    #[test]
    fn a_pipe_under_a_jobs_name_is_refused_without_waiting() {
        let root = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(root.path());
        let dir = jobs.dir(true).unwrap();
        rustix::fs::mknodat(
            &dir,
            "j1.log",
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        assert_eq!(jobs.output("j1", Some(0)).unwrap_err().code, ErrorCode::ServiceError);
        // A removed output file is said, not refused.
        let removed = jobs.output("j2", None).unwrap();
        assert!(removed.removed && removed.text.is_empty(), "{removed:?}");
    }
}
