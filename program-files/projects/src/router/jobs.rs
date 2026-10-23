//! Background shell jobs as the router keeps them.
//!
//! A session's agent says it is one (`jobs register` on `[project,
//! session]`, which the kernel let only that session's agent and the
//! runtime's own services make). From then on each of its `shell exec`
//! calls is given a job id before it is forwarded, and keeps the agent's
//! name for it (`call`), so it can become a job: started so (`background`),
//! or moved there by a person (`jobs background`), which the router asks
//! of the host the command runs in (`detach`). The host answers the call
//! at once and the job runs there; the router keeps a `wait` out on it,
//! answered when the job ends or after [`WAIT_MS`] with its last lines
//! (the in-flight wait also keeps the host from being stopped as idle).
//!
//! `list` and `output` reach every job of the session's project; a job is
//! stopped (`stop`) through its own session only, which the kernel holds a
//! session's agent to.
//!
//! When a job ends the router records how and wakes the session
//! (`conversation wake`) with its exit status, its last lines
//! and its output's path, unless the agent stopped it itself. Each job is a
//! record in the state
//! directory, which also says whether its session has been told of its end:
//! a restart of the runtime, which ends every job, marks the ones that ran
//! `interrupted`, and the next start wakes the session of every job whose
//! end has not reached it (a wake-up the ending runtime refused included).
//!
//! Every record is kept and read at the next start. In memory an ended job
//! is kept without its last lines, which a listing reads back from its
//! record: what grows with the jobs a project ever ran is their list fields.
//!
//! A shell host can lie about its jobs and their output as it can about its
//! commands. A session's agent can start up to
//! [`wire::MAX_RUNNING_PER_PROJECT`] jobs, fewer when the project's other
//! sessions run some, which run as its project for as long as they last, and
//! read every job's output in its project, which its shell could read
//! already.
//!
//! Not defended: a wake-up the conversation refuses while the runtime runs is
//! logged and not sent again before the service's next start, though the
//! job's record and output stay and the agent lists it. A job
//! runs in its project's shell host, beside the project's other sessions'
//! commands, which can find its processes and signal them: a session's agent
//! is told not to stop another session's job
//! (`prompts/templates/agents/background_jobs.md`), not prevented from it.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nucleus_conversation_interface as conversation;
use nucleus_projects_interface as projects;
use nucleus_protocol::{DispatchId, Error, ErrorCode, Payload, ProcessId, RequestId, ResourcePath};
use nucleus_sdk::{ChannelError, Dispatch};
use nucleus_shell_interface::jobs::{
    self as wire, BackgroundRequest, By, Job, JobResponse, ListRequest, ListResponse, OutputResponse, Scope,
    State, StopRequest,
};
use nucleus_shell_interface::{
    ACTION_DETACH, ACTION_OUTPUT, ACTION_STOP, ACTION_WAIT, Detached, ExecResponse, JobRef, JobStatus,
    OutputChunk, OutputRequest, WaitRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Call, HostKey, Kernel, Router, Then, invalid};
use crate::hosts::Kind;

/// How long one `wait` on a job's host holds: how fresh a job's tail is.
const WAIT_MS: u64 = 5_000;
/// Agents remembered by their process; the longest registered goes first.
const MAX_AGENTS: usize = 256;
/// The deadline of the router's own calls about jobs, beyond their waits.
const CALL_MS: u64 = 30_000;
/// Why a job the runtime's restart ended is `interrupted`.
const RESTARTED: &str = "the VM's Open Math OS runtime restarted while it ran";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn busy(message: String) -> Error {
    Error::new(ErrorCode::ServiceError, message)
}

/// A session, as a `jobs` call's resource names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct Session {
    pub project: String,
    pub name: String,
}

impl Session {
    fn resource(&self) -> Result<ResourcePath, Error> {
        ResourcePath::new([self.project.as_str(), self.name.as_str()])
    }
}

/// A registered agent's command in flight, which may become a job.
pub(super) struct Exec {
    session: Session,
    /// The agent's name for it.
    call: Option<String>,
    command: String,
    /// The agent's call.
    dispatch: DispatchId,
    background: bool,
    /// A person's `background` waiting for the command's answer, and their name.
    moved: Option<(DispatchId, Option<String>)>,
}

/// One job.
struct Entry {
    session: Session,
    job: Job,
    /// Where it runs, while it does.
    host: Option<HostKey>,
    /// Who asked it to stop.
    stopping: Option<(By, Option<String>)>,
    waiting: bool,
    /// Its session knows how it ended (its wake-up was taken, or its own
    /// agent stopped it), or it still runs. A job that ended untold is told
    /// at the service's next start.
    told: bool,
    /// Its last lines are in its record only (`Book::shed`).
    shed: bool,
}

/// Everything the router keeps of jobs.
#[derive(Default)]
pub(super) struct Book {
    state: Option<PathBuf>,
    /// The registered agents, each with when it last registered (a count).
    agents: BTreeMap<ProcessId, (Session, u64)>,
    registrations: u64,
    execs: HashMap<String, Exec>,
    entries: BTreeMap<String, Entry>,
    /// The waits out, each with its job and the host it was sent to: the
    /// job's end may be recorded before its last wait is answered.
    waits: HashMap<RequestId, (String, HostKey)>,
    wakes: HashMap<RequestId, String>,
    last_id: u64,
}

/// A job's record in the state directory.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    v: u32,
    session: Session,
    job: Job,
    /// Whether its session has been told of its end (`Entry::told`).
    told: bool,
}

impl Book {
    /// The jobs the state directory records, those a restart interrupted
    /// marked so; with them, those whose sessions are still to be told.
    pub(super) fn open(state: Option<PathBuf>) -> Result<(Self, Vec<String>), String> {
        let mut book = Self { state, ..Self::default() };
        let mut untold = Vec::new();
        let Some(dir) = book.state.clone() else { return Ok((book, untold)) };
        let listed =
            std::fs::read_dir(&dir).map_err(|e| format!("the jobs' state {}: {e}", dir.display()))?;
        let now = now_ms();
        for entry in listed.flatten() {
            let path = entry.path();
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json"))
            else {
                continue;
            };
            let record =
                std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
            let Some(Record { session, mut job, mut told, .. }) =
                record.filter(|r| r.v == 1 && r.job.job == id)
            else {
                eprintln!("nucleus-projects: {} is no job record; removed", path.display());
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let ran = job.state == State::Running;
            if ran {
                job.state = State::Interrupted;
                job.ended_at_ms = Some(now);
                job.note = Some(RESTARTED.into());
                told = false;
            }
            if !told {
                untold.push(id.to_owned());
            }
            book.entries.insert(
                id.to_owned(),
                Entry { session, job, host: None, stopping: None, waiting: false, told, shed: false },
            );
            if ran {
                book.persist(id);
            }
            book.shed(id);
        }
        Ok((book, untold))
    }

    /// Keeps an ended job's last lines in its record only.
    fn shed(&mut self, id: &str) {
        if self.state.is_none() {
            return;
        }
        if let Some(entry) = self.entries.get_mut(id).filter(|e| e.job.state != State::Running) {
            entry.job.tail = String::new();
            entry.shed = true;
        }
    }

    /// Job `id` whole: its last lines read back from its record when they
    /// were shed.
    fn job(&self, id: &str) -> Option<Job> {
        let entry = self.entries.get(id)?;
        let mut job = entry.job.clone();
        if entry.shed
            && let Some(dir) = &self.state
        {
            let record = std::fs::read(dir.join(format!("{id}.json")))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
            match record {
                Some(record) => job.tail = record.job.tail,
                None => eprintln!("nucleus-projects: the record of job {id} could not be read back"),
            }
        }
        Some(job)
    }

    /// A new job id, after every one this book knows.
    fn new_id(&mut self) -> String {
        self.last_id = now_ms().max(self.last_id + 1);
        loop {
            let id = format!("j{}", base36(self.last_id));
            if !self.entries.contains_key(&id) && !self.execs.contains_key(&id) {
                return id;
            }
            self.last_id += 1;
        }
    }

    /// Remembers that `process` is `session`'s agent, forgetting the one
    /// that registered longest ago when there are too many: an agent
    /// registers before each command, so the one about to run is never it.
    fn register(&mut self, process: ProcessId, session: Session) {
        if !self.agents.contains_key(&process) && self.agents.len() >= MAX_AGENTS {
            let oldest = self.agents.iter().min_by_key(|(_, (_, at))| *at).map(|(process, _)| *process);
            if let Some(oldest) = oldest {
                self.agents.remove(&oldest);
            }
        }
        self.registrations += 1;
        self.agents.insert(process, (session, self.registrations));
    }

    /// The session `process` is the registered agent of.
    fn agent(&self, process: ProcessId) -> Option<&Session> {
        self.agents.get(&process).map(|(session, _)| session)
    }

    fn running(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().filter(|e| e.job.state == State::Running)
    }

    /// A page of the jobs of `session`'s project, or of `session` alone:
    /// running before ended, each newest first (the `jobs` interface's
    /// module doc), from the cursor `after`.
    fn listed(&self, session: &Session, request: &ListRequest, limit: usize) -> Result<ListResponse, Error> {
        let after = request.after.as_deref().map(parse_cursor).transpose()?;
        let only_session = request.scope == Some(Scope::Session);
        let in_scope = self.entries.iter().filter(|(_, e)| {
            e.session.project == session.project && (!only_session || e.session == *session)
        });
        let running = in_scope.clone().filter(|(_, e)| e.job.state == State::Running).count();
        let mut keys: Vec<Place> = in_scope
            .map(|(id, e)| place(e.job.state != State::Running, id))
            .filter(|key| after.as_ref().is_none_or(|after| key > after))
            .collect();
        keys.sort();
        let next = (keys.len() > limit).then(|| keys.get(limit - 1).map(cursor)).flatten();
        let jobs = keys.iter().take(limit).filter_map(|key| self.job(&key.1.0.1)).collect();
        Ok(ListResponse { jobs, running, next })
    }

    /// Forgets every job of `project` with its record, and what is out about
    /// them: the project is being deleted, its sessions too.
    fn forget(&mut self, project: &str) -> Result<(), String> {
        let ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.session.project == project)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.entries.remove(&id);
            if let Some(dir) = &self.state {
                match std::fs::remove_file(dir.join(format!("{id}.json"))) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(format!("the record of job {id}: {e}"));
                    }
                    _ => {}
                }
            }
        }
        self.execs.retain(|_, exec| exec.session.project != project);
        self.agents.retain(|_, (session, _)| session.project != project);
        let entries = &self.entries;
        self.waits.retain(|_, (id, _)| entries.contains_key(id));
        self.wakes.retain(|_, id| entries.contains_key(id));
        Ok(())
    }

    /// Writes a job's record (a new file renamed over the old one).
    fn persist(&self, id: &str) {
        let (Some(dir), Some(entry), Some(job)) = (&self.state, self.entries.get(id), self.job(id)) else {
            return;
        };
        let record = Record { v: 1, session: entry.session.clone(), job, told: entry.told };
        let (path, fresh) = (dir.join(format!("{id}.json")), dir.join(format!(".{id}.json.new")));
        let written = serde_json::to_vec(&record)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&fresh, bytes))
            .and_then(|()| std::fs::rename(&fresh, &path));
        if let Err(e) = written {
            eprintln!("nucleus-projects: the record of job {id}: {e}");
        }
    }
}

/// Where a job stands in a listing: ended after running, then newest first
/// (ids grow with their start, so the longest and then greatest).
type Place = (bool, Reverse<(usize, String)>);

fn place(ended: bool, id: &str) -> Place {
    (ended, Reverse((id.len(), id.to_owned())))
}

/// The cursor after `place`: `running.<id>` or `ended.<id>`.
fn cursor(place: &Place) -> String {
    let (ended, Reverse((_, id))) = place;
    format!("{}.{id}", if *ended { "ended" } else { "running" })
}

fn parse_cursor(cursor: &str) -> Result<Place, Error> {
    match cursor.split_once('.') {
        Some(("running", id)) if wire::valid_id(id) => Ok(place(false, id)),
        Some(("ended", id)) if wire::valid_id(id) => Ok(place(true, id)),
        _ => Err(invalid("after is not the next of a jobs list")),
    }
}

fn base36(mut n: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// Text for another party's model: no hidden character but tab and line
/// breaks, carriage returns dropped.
fn shown(text: &str) -> String {
    let mut kept = text.to_owned();
    // Taking a hidden character out can leave two that hide each other
    // side by side (a selector either side of a joiner): until none is left.
    loop {
        let next: String = nucleus_protocol::text::marked(&kept)
            .filter(|(c, hidden)| !hidden || matches!(c, '\t' | '\n'))
            .map(|(c, _)| c)
            .collect();
        if next.len() == kept.len() {
            return next;
        }
        kept = next;
    }
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

/// What wakes the session when `job` has ended: how, its command, its last
/// lines and where its whole output is, within the reminder's bound.
pub(super) fn wake_text(job: &Job) -> String {
    let id = &job.job;
    let took = duration(job.ended_at_ms.unwrap_or_else(now_ms).saturating_sub(job.started_at_ms));
    let head = match job.state {
        State::Exited => match job.exit_code {
            Some(code) => format!("Background job {id} ended: exit status {code}, after {took}."),
            None => format!("Background job {id} ended: a signal ended it, after {took}."),
        },
        State::Stopped => match (job.stopped_by, &job.stopped_author) {
            (Some(By::Person), Some(author)) => {
                format!("Background job {id} was stopped by a person (account {author}) after {took}.")
            }
            (Some(By::Person), None) => format!("Background job {id} was stopped by a person after {took}."),
            _ => format!("Background job {id} was stopped after {took}."),
        },
        State::TimedOut => {
            format!(
                "Background job {id} reached the time limit its command named and was stopped, after {took}."
            )
        }
        State::Interrupted | State::Running => format!(
            "Background job {id} was interrupted ({}), after {took}.",
            job.note.as_deref().unwrap_or("it ended unexpectedly")
        ),
    };
    let left = match job.stopped_leftovers {
        Some(1) => " The 1 process its command left running (started in the background, or with setsid or \
                    nohup) was stopped with it: a job ends when its command does."
            .to_owned(),
        Some(n) => format!(
            " The {n} processes its command left running (started in the background, or with setsid or \
             nohup) were stopped with it: a job ends when its command does."
        ),
        None => String::new(),
    };
    let limit = conversation::MAX_REMINDER_TEXT_BYTES;
    let tail_room = limit.saturating_sub(64);
    let output = shown(&format!("\nFull output: {} (shell_jobs output {id} reads it).", job.output));
    let mut text = shown(&format!("{head}{left}"));
    // As much of its command as leaves room for where its output is.
    let command = shown(&job.command);
    let mut room = limit.saturating_sub(text.len() + output.len() + "\nCommand: ".len()).min(command.len());
    while !command.is_char_boundary(room) {
        room -= 1;
    }
    if room > 0 {
        text.push_str(&format!("\nCommand: {}", &command[..room]));
    }
    text.push_str(&output);
    let tail = shown(job.tail.trim_end());
    if !tail.is_empty() && text.len() + 32 < tail_room {
        let room = tail_room - text.len() - 32;
        let mut from = tail.len().saturating_sub(room);
        while !tail.is_char_boundary(from) {
            from += 1;
        }
        let mut last = &tail[from..];
        if from > 0 {
            last = last.split_once('\n').map_or(last, |(_, rest)| rest);
        }
        if !last.is_empty() {
            text.push_str(&format!("\nIts last lines of output:\n{last}"));
        }
    }
    let mut end = text.len().min(conversation::MAX_REMINDER_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// What the agent's call says when its command went on as a job.
fn moved_note(job: &Job) -> String {
    let who = match (job.by, &job.author) {
        (By::Agent, _) => "Started in the background".to_owned(),
        (By::Person, Some(author)) => format!("Moved to the background by a person (account {author})"),
        (By::Person, None) => "Moved to the background by a person".to_owned(),
    };
    let until = match job.limit_ms {
        Some(ms) => format!("until its time limit, {} from its start", duration(ms)),
        None => "with no time limit".to_owned(),
    };
    format!(
        "{who} as job {}: it keeps running after this answer, {until}. You are woken with its exit status, its \
         last lines of output and its full output's path ({}) when it ends; meanwhile shell_jobs lists your \
         jobs, reads a job's output or stops it.",
        job.job, job.output
    )
}

impl Router {
    /// Opens the jobs' state directory; without one, a restart forgets them.
    pub fn with_jobs(mut self, state: Option<PathBuf>) -> Result<Self, String> {
        let (book, untold) = Book::open(state)?;
        self.book = book;
        self.untold = untold;
        Ok(self)
    }

    /// Wakes the sessions that are still to be told how their jobs ended:
    /// those a restart interrupted, and those whose wake-up did not reach
    /// the session before the runtime ended.
    pub fn wake_untold(&mut self, k: &mut impl Kernel) -> Result<(), ChannelError> {
        for id in std::mem::take(&mut self.untold) {
            self.wake(k, &id)?;
        }
        Ok(())
    }

    /// Room for one more running job in `session`'s project, counting the
    /// commands on their way to the background; refused saying which
    /// sessions run them.
    fn room_for_job(&self, session: &Session) -> Result<(), Error> {
        let coming =
            self.book.execs.values().filter(|e| e.background || e.moved.is_some()).map(|e| &e.session);
        let mut by_session: BTreeMap<&str, usize> = BTreeMap::new();
        for s in
            self.book.running().map(|e| &e.session).chain(coming).filter(|s| s.project == session.project)
        {
            *by_session.entry(s.name.as_str()).or_default() += 1;
        }
        let running: usize = by_session.values().sum();
        if running >= wire::MAX_RUNNING_PER_PROJECT {
            let whose: Vec<String> =
                by_session.iter().map(|(name, n)| format!("{n} of session {name}")).collect();
            return Err(Error::new(
                ErrorCode::LimitExceeded,
                format!(
                    "at most {} background jobs run in a project at once, and project {} runs {running} ({}): \
                     stop one that is no longer needed, or wait for one to end",
                    wire::MAX_RUNNING_PER_PROJECT,
                    session.project,
                    whose.join(", ")
                ),
            ));
        }
        Ok(())
    }

    /// A `shell exec` on its way to a host: a registered agent's gets a job
    /// id and keeps its call's name; nobody else's may start a job.
    pub(super) fn tag_exec(
        &mut self,
        d: &Dispatch,
        kind: Kind,
        project: &str,
        call: &mut Call,
    ) -> Result<(), Error> {
        let mut payload: Value = call.payload.decode()?;
        let Some(fields) = payload.as_object_mut() else { return Err(invalid("exec takes an object")) };
        // Only this service names jobs.
        fields.remove("job");
        let background = fields.get("background").and_then(Value::as_bool) == Some(true);
        let agent =
            self.book.agent(d.caller).filter(|s| kind == Kind::Shell && s.project == project).cloned();
        let Some(session) = agent else {
            if background {
                return Err(invalid("background jobs are for a session's own agent, in the VM"));
            }
            call.payload = Payload::encode(&payload)?;
            return Ok(());
        };
        if background {
            self.room_for_job(&session)?;
        }
        let label =
            fields.get("call").and_then(Value::as_str).filter(|c| wire::valid_call(c)).map(str::to_owned);
        if label.is_none() {
            fields.remove("call");
        }
        let command =
            fields.get("command").and_then(Value::as_str).map(wire::command_line).unwrap_or_default();
        let id = self.book.new_id();
        fields.insert("job".into(), Value::String(id.clone()));
        call.payload = Payload::encode(&payload)?;
        call.job = Some(id.clone());
        self.book
            .execs
            .insert(id, Exec { session, call: label, command, dispatch: d.id, background, moved: None });
        Ok(())
    }

    /// Forgets a command that will not become a job; a person who was
    /// moving it is told.
    pub(super) fn untag(&mut self, k: &mut impl Kernel, job: &str) -> Result<(), ChannelError> {
        if let Some(Exec { moved: Some((window, _)), .. }) = self.book.execs.remove(job) {
            k.reply(window, Err(invalid("that command ended before it could go to the background")))?;
        }
        Ok(())
    }

    /// The answer of a registered agent's command: `Ok(None)` when it did
    /// not become a job (the caller answers as for any command), or it
    /// did and is answered here.
    pub(super) fn exec_answered(
        &mut self,
        k: &mut impl Kernel,
        id: &str,
        host: &HostKey,
        result: &Result<Payload, Error>,
    ) -> Result<bool, ChannelError> {
        let response = result.as_ref().ok().and_then(|p| p.decode::<ExecResponse>().ok());
        let Some(mut response) = response.filter(|r| r.job.is_some()) else {
            // An answer that is not its host's (its caller withdrew the
            // call, or it ran out of time, as the host made the command a
            // job) leaves the host with a job this service has no word of:
            // it is stopped, so no command runs on that nobody can see.
            if result.is_err() {
                self.stop_unanswered(k, host, id)?;
            }
            self.untag(k, id)?;
            return Ok(false);
        };
        let Some(exec) = self.book.execs.remove(id) else { return Ok(false) };
        let Some(detached) = response.job.take() else { return Ok(false) };
        let author = exec.moved.as_ref().and_then(|(_, author)| author.clone());
        let job = Job {
            job: id.to_owned(),
            session: exec.session.name.clone(),
            command: exec.command,
            state: State::Running,
            started_at_ms: now_ms().saturating_sub(response.elapsed_ms),
            ended_at_ms: None,
            exit_code: None,
            stopped_leftovers: None,
            by: detached.by,
            author: if detached.by == By::Person { author.clone() } else { None },
            stopped_by: None,
            stopped_author: None,
            output: detached.output.clone(),
            size: 0,
            tail: String::new(),
            limit_ms: detached.limit_ms,
            note: None,
        };
        response.job = Some(Detached { author: job.author.clone(), note: moved_note(&job), ..detached });
        k.reply(exec.dispatch, Payload::encode(&response))?;
        if let Some((window, _)) = exec.moved {
            k.reply(window, Payload::encode(&JobResponse { job: job.clone() }))?;
        }
        self.book.entries.insert(
            id.to_owned(),
            Entry {
                session: exec.session,
                job,
                host: Some(host.clone()),
                stopping: None,
                waiting: false,
                told: true,
                shed: false,
            },
        );
        self.book.persist(id);
        self.wait_on(k, id)?;
        Ok(true)
    }

    /// Tells `host` to stop the job `id`, whose command's answer was lost:
    /// the host stops it or has none by that name. Its answer is not awaited.
    fn stop_unanswered(&mut self, k: &mut impl Kernel, host: &HostKey, id: &str) -> Result<(), ChannelError> {
        let (Some(host), Ok(payload)) =
            (self.hosts.get(host), Payload::encode(&JobRef { job: id.to_owned() }))
        else {
            return Ok(());
        };
        let _ = k.submit(host.invoke, ACTION_STOP, &ResourcePath::root(), &payload, Some(CALL_MS))?;
        Ok(())
    }

    /// A call of the `jobs` endpoint, on `[project, session]`.
    pub(super) fn jobs_call(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let session = match d.resource.segments() {
            [project, name] if projects::valid_name(project) && projects::valid_name(name) => {
                Session { project: project.clone(), name: name.clone() }
            }
            _ => return k.reply(d.id, Err(invalid("a jobs call names [project, session]"))),
        };
        if let Err(e) = self.registry.exists(&session.project) {
            return k.reply(d.id, Err(e));
        }
        let deadline = Instant::now() + Duration::from_millis(d.remaining_ms);
        let no_job = |id: &str| invalid(format!("this project has no job {id}"));
        match d.action.as_str() {
            wire::ACTION_REGISTER => {
                let registered = d.payload.decode::<projects::Empty>().map(|_| {
                    self.book.register(d.caller, session);
                });
                k.reply(d.id, registered.and_then(|()| Payload::encode(&serde_json::json!({}))))
            }
            wire::ACTION_LIST => {
                let listed = d.payload.decode::<ListRequest>().and_then(|request| {
                    let limit = request.validate()?;
                    Payload::encode(&self.book.listed(&session, &request, limit)?)
                });
                k.reply(d.id, listed)
            }
            wire::ACTION_OUTPUT => {
                let request = match d.payload.decode::<OutputRequest>() {
                    Ok(request) => request,
                    Err(e) => return k.reply(d.id, Err(e)),
                };
                let Some(entry) =
                    self.book.entries.get(&request.job).filter(|e| e.session.project == session.project)
                else {
                    return k.reply(d.id, Err(no_job(&request.job)));
                };
                // A running job's own host, or the session's shell host,
                // which reads the project's files as well.
                let key = entry.host.clone().unwrap_or((session.project.clone(), Kind::Shell));
                let call = Call {
                    dispatch: d.id,
                    action: ACTION_OUTPUT.to_owned(),
                    resource: ResourcePath::root(),
                    payload: match Payload::encode(&request) {
                        Ok(payload) => payload,
                        Err(e) => return k.reply(d.id, Err(e)),
                    },
                    deadline,
                    then: Then::JobOutput { job: request.job },
                    job: None,
                };
                self.send(k, key, call)
            }
            // `stop` is the session's agent's.
            wire::ACTION_STOP => {
                let request = d.payload.decode::<StopRequest>().map(|r| (r.job, By::Agent, None));
                let (id, by, author) = match request {
                    Ok(request) => request,
                    Err(e) => return k.reply(d.id, Err(e)),
                };
                let Some(entry) =
                    self.book.entries.get_mut(&id).filter(|e| e.session.project == session.project)
                else {
                    return k.reply(d.id, Err(no_job(&id)));
                };
                // Stopped through its own session only: a session's agent
                // reaches its own session and nothing else.
                if entry.session != session {
                    let whose = match (entry.job.by, &entry.job.author) {
                        (By::Person, Some(author)) => {
                            format!("a person (account {author}) moved it to the background there")
                        }
                        (By::Person, None) => "a person moved it to the background there".to_owned(),
                        (By::Agent, _) => "that session's agent started it".to_owned(),
                    };
                    let refused = format!(
                        "job {id} belongs to session {} ({whose}): it is someone else's work, not this \
                         session's to stop. Tell the person instead.",
                        entry.session.name
                    );
                    return k.reply(d.id, Err(invalid(refused)));
                }
                let Some(key) = entry.host.clone().filter(|_| entry.job.state == State::Running) else {
                    return k.reply(d.id, Err(invalid(format!("job {id} has ended"))));
                };
                // The first to stop it is who stopped it.
                entry.stopping.get_or_insert((by, author));
                let call = Call {
                    dispatch: d.id,
                    action: ACTION_STOP.to_owned(),
                    resource: ResourcePath::root(),
                    payload: match Payload::encode(&JobRef { job: id.clone() }) {
                        Ok(payload) => payload,
                        Err(e) => return k.reply(d.id, Err(e)),
                    },
                    deadline,
                    then: Then::JobStop { job: id },
                    job: None,
                };
                self.submit(k, key, call)
            }
            wire::ACTION_BACKGROUND => {
                let request = match d.payload.decode::<BackgroundRequest>() {
                    Ok(request) if wire::valid_call(&request.call) => request,
                    Ok(_) => return k.reply(d.id, Err(invalid("call names no tool call"))),
                    Err(e) => return k.reply(d.id, Err(e)),
                };
                let found = self.book.execs.iter().find(|(_, e)| {
                    e.session == session
                        && e.call.as_deref() == Some(request.call.as_str())
                        && e.moved.is_none()
                });
                let Some((id, exec)) = found.map(|(id, e)| (id.clone(), e.dispatch)) else {
                    let none =
                        format!("no running shell command of this session's agent is call {}", request.call);
                    return k.reply(d.id, Err(invalid(none)));
                };
                // The host its command runs in, once it runs there.
                let host = self.by_dispatch.get(&exec).into_iter().flatten().find_map(|request| {
                    self.forwards
                        .get(request)
                        .filter(|f| f.job.as_deref() == Some(id.as_str()))
                        .map(|f| f.host.clone())
                });
                // Not a refusal for good: the command still waits for its host.
                let Some(key) = host else {
                    return k.reply(
                        d.id,
                        Err(busy("that command has not started yet; try again in a moment".into())),
                    );
                };
                if let Err(e) = self.room_for_job(&session) {
                    return k.reply(d.id, Err(e));
                }
                if let Some(exec) = self.book.execs.get_mut(&id) {
                    exec.moved = Some((d.id, request.author));
                }
                let call = Call {
                    dispatch: d.id,
                    action: ACTION_DETACH.to_owned(),
                    resource: ResourcePath::root(),
                    payload: match Payload::encode(&JobRef { job: id.clone() }) {
                        Ok(payload) => payload,
                        Err(e) => return k.reply(d.id, Err(e)),
                    },
                    deadline,
                    then: Then::JobDetach { job: id },
                    job: None,
                };
                self.submit(k, key, call)
            }
            other => k.reply(d.id, Err(invalid(format!("jobs has no action {other}")))),
        }
    }

    /// The host's answer to a person's or the agent's call about a job:
    /// what the caller gets, or `None` when it is answered later.
    pub(super) fn job_reply(
        &mut self,
        k: &mut impl Kernel,
        then: Then,
        result: Result<Payload, Error>,
    ) -> Result<Option<Result<Payload, Error>>, ChannelError> {
        Ok(match then {
            Then::JobOutput { job } => {
                Some(result.and_then(|p| p.decode::<OutputChunk>()).and_then(|chunk| {
                    let whole = self.book.job(&job).ok_or_else(|| invalid(format!("no job {job}")))?;
                    Payload::encode(&OutputResponse {
                        job: whole,
                        text: chunk.text,
                        start: chunk.start,
                        next: chunk.next,
                        size: chunk.size,
                        removed: chunk.removed,
                    })
                }))
            }
            // A host that could not end it in time says it still runs: it
            // is recorded when it has ended, as stopped by whoever asked.
            Then::JobStop { job } => match result.and_then(|p| p.decode::<JobStatus>()) {
                Ok(status) => {
                    if status.state != State::Running {
                        self.job_ended(k, &job, Ok(status))?;
                    }
                    match self.book.job(&job) {
                        Some(whole) => Some(Payload::encode(&JobResponse { job: whole })),
                        None => Some(Err(invalid(format!("no job {job}")))),
                    }
                }
                Err(e) => {
                    if let Some(entry) = self.book.entries.get_mut(&job) {
                        entry.stopping = None;
                    }
                    Some(Err(e))
                }
            },
            // The command's own answer, which follows, completes the move.
            Then::JobDetach { job } => match result {
                Ok(_) => None,
                Err(e) => {
                    if let Some(exec) = self.book.execs.get_mut(&job) {
                        exec.moved = None;
                    }
                    Some(Err(e))
                }
            },
            _ => Some(result),
        })
    }

    /// Asks a running job's host how it is: answered when it ends, or after
    /// [`WAIT_MS`] with its last lines.
    fn wait_on(&mut self, k: &mut impl Kernel, id: &str) -> Result<(), ChannelError> {
        let Some(entry) =
            self.book.entries.get_mut(id).filter(|e| e.job.state == State::Running && !e.waiting)
        else {
            return Ok(());
        };
        let Some(key) = entry.host.clone() else { return Ok(()) };
        let Some(host) = self.hosts.get_mut(&key).filter(|h| h.ready) else { return Ok(()) };
        let Ok(payload) = Payload::encode(&WaitRequest { job: id.to_owned(), wait_ms: WAIT_MS }) else {
            return Ok(());
        };
        // A refusal (the router's calls all out) is tried again at the next sweep.
        if let Ok(request) =
            k.submit(host.invoke, ACTION_WAIT, &ResourcePath::root(), &payload, Some(WAIT_MS + CALL_MS))?
        {
            host.inflight += 1;
            entry.waiting = true;
            self.book.waits.insert(request, (id.to_owned(), key));
        }
        Ok(())
    }

    /// The answer of one of the router's own calls about jobs: a wait or a
    /// wake-up. False when `request` is none of them.
    pub(super) fn job_answer(
        &mut self,
        k: &mut impl Kernel,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<bool, ChannelError> {
        if let Some(id) = self.book.wakes.remove(&request) {
            let Some(entry) = self.book.entries.get_mut(&id) else { return Ok(true) };
            match result {
                Ok(_) => entry.told = true,
                // The runtime is ending, and the job with it: the session
                // is told at the next start, and told that.
                Err(e) if e.code == ErrorCode::ShuttingDown => {
                    if entry.job.state == State::Interrupted {
                        entry.job.note = Some(RESTARTED.into());
                    }
                }
                Err(e) => {
                    eprintln!("nucleus-projects: waking the session of job {id}: {e}");
                    // A session that is no more has nobody to tell.
                    entry.told = e.code == ErrorCode::InvalidInput;
                }
            }
            self.book.persist(&id);
            return Ok(true);
        }
        let Some((id, key)) = self.book.waits.remove(&request) else { return Ok(false) };
        // The host's call is back, whatever has become of the job since.
        if let Some(host) = self.hosts.get_mut(&key) {
            host.inflight = host.inflight.saturating_sub(1);
            host.last_used = Instant::now();
        }
        let Some(entry) = self.book.entries.get_mut(&id) else { return Ok(true) };
        entry.waiting = false;
        match result.and_then(|p| p.decode::<JobStatus>()) {
            Ok(status) if status.state == State::Running => {
                entry.job.size = status.size;
                entry.job.tail = status.tail;
                self.wait_on(k, &id)?;
            }
            Ok(status) => self.job_ended(k, &id, Ok(status))?,
            Err(e) if e.code == ErrorCode::Timeout => self.wait_on(k, &id)?,
            // Only its host's own word that it has no such job ends it
            // here. A host's end is the kernel's to tell (`jobs_host_ended`),
            // and a call the kernel refused or lost is made again at the
            // next sweep: neither says anything of the job.
            Err(e)
                if e.code == ErrorCode::InvalidInput
                    && entry.job.state == State::Running
                    && entry.host.is_some() =>
            {
                self.job_ended(k, &id, Err(format!("its shell lost it: {}", e.message)))?;
            }
            Err(_) => {}
        }
        Ok(true)
    }

    /// A job has ended as its host says (`Ok`), or was lost (`Err`, why): it
    /// is recorded, and its session woken.
    fn job_ended(
        &mut self,
        k: &mut impl Kernel,
        id: &str,
        how: Result<JobStatus, String>,
    ) -> Result<(), ChannelError> {
        let Some(entry) = self.book.entries.get_mut(id).filter(|e| e.job.state == State::Running) else {
            return Ok(());
        };
        let job = &mut entry.job;
        match how {
            Ok(status) => {
                job.state = status.state;
                job.exit_code = status.exit_code;
                job.stopped_leftovers = status.stopped_leftovers;
                job.size = status.size;
                job.tail = status.tail;
                job.ended_at_ms = Some(job.started_at_ms.saturating_add(status.elapsed_ms));
            }
            Err(note) => {
                job.state = State::Interrupted;
                job.note = Some(note);
                job.ended_at_ms = Some(now_ms());
            }
        }
        if let Some((by, author)) = entry.stopping.take()
            && job.state == State::Stopped
        {
            job.stopped_by = Some(by);
            job.stopped_author = author;
        }
        entry.host = None;
        // The agent that stopped it knows from its own call; any other
        // end is told to its session, now or at the next start.
        entry.told = entry.job.stopped_by == Some(By::Agent);
        let told = entry.told;
        self.book.persist(id);
        if !told {
            self.wake(k, id)?;
        }
        self.book.shed(id);
        Ok(())
    }

    /// Wakes the job's session now with how it ended.
    fn wake(&mut self, k: &mut impl Kernel, id: &str) -> Result<(), ChannelError> {
        let (Some(handle), Some(entry), Some(job)) =
            (self.authority.wake, self.book.entries.get(id), self.book.job(id))
        else {
            return Ok(());
        };
        let request = conversation::RemindRequest { after_ms: 0, text: wake_text(&job) };
        let (Ok(resource), Ok(payload)) = (entry.session.resource(), Payload::encode(&request)) else {
            return Ok(());
        };
        match k.submit(handle, conversation::ACTION_WAKE, &resource, &payload, Some(CALL_MS))? {
            Ok(request) => drop(self.book.wakes.insert(request, id.to_owned())),
            Err(e) => eprintln!("nucleus-projects: waking the session of job {id}: {e}"),
        }
        Ok(())
    }

    /// The host `key` ended: the jobs it ran are lost.
    pub(super) fn jobs_host_ended(&mut self, k: &mut impl Kernel, key: &HostKey) -> Result<(), ChannelError> {
        let lost: Vec<String> =
            self.book.running().filter(|e| e.host.as_ref() == Some(key)).map(|e| e.job.job.clone()).collect();
        for id in lost {
            self.job_ended(k, &id, Err("its shell host ended".to_owned()))?;
        }
        Ok(())
    }

    /// Asks again about running jobs no wait is out for.
    pub(super) fn jobs_sweep(&mut self, k: &mut impl Kernel) -> Result<(), ChannelError> {
        let quiet: Vec<String> =
            self.book.running().filter(|e| !e.waiting).map(|e| e.job.job.clone()).collect();
        for id in quiet {
            self.wait_on(k, &id)?;
        }
        Ok(())
    }

    /// Forgets `project`'s jobs (`Book::forget`), before its hosts are
    /// stopped: a job ended so wakes no session.
    pub(super) fn forget_jobs(&mut self, project: &str) -> Result<(), Error> {
        self.book.forget(project).map_err(busy)
    }

    /// Whether a host runs a job, which keeps it from being stopped as idle.
    pub(super) fn holds_jobs(&self, key: &HostKey) -> bool {
        self.book.running().any(|e| e.host.as_ref() == Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(state: State, exit_code: Option<i32>, tail: &str) -> Job {
        Job {
            job: "jk3".into(),
            session: "s1".into(),
            command: "gap -q scan.g".into(),
            state,
            started_at_ms: 1_000,
            ended_at_ms: Some(1_000 + 2_472_000),
            exit_code,
            stopped_leftovers: None,
            by: By::Person,
            author: Some("Ada".into()),
            stopped_by: None,
            stopped_author: None,
            output: "/homes/alpha/.nucleus/jobs/jk3.log".into(),
            size: 10,
            tail: tail.into(),
            limit_ms: None,
            note: None,
        }
    }

    /// The wake-up says how the job ended, its command, its last lines and
    /// where its whole output is, and is a reminder the conversation takes:
    /// within its bound, with nothing hidden, however long or odd the
    /// output.
    #[test]
    fn a_job_s_wake_up_carries_its_end_and_fits_a_reminder() {
        let long = format!("{}\n\u{1b}[31mlast line\r\n", "x".repeat(5_000));
        for (job, says) in [
            (ended(State::Exited, Some(0), "n=28 done\n"), "exit status 0"),
            (ended(State::Exited, Some(1), &long), "exit status 1"),
            (ended(State::TimedOut, None, ""), "time limit its command named"),
            (ended(State::Interrupted, None, "x"), "interrupted"),
            // A joiner between two selectors: taking it out leaves nothing hidden.
            (ended(State::Exited, Some(0), "a\u{fe0f}\u{200d}\u{fe0f}b\n"), "exit status 0"),
        ] {
            let text = wake_text(&job);
            let request = conversation::RemindRequest { after_ms: 0, text: text.clone() };
            assert!(request.validate(conversation::ReminderAction::Wake).is_ok(), "{text:?}");
            for part in [says, "jk3", "gap -q scan.g", "/homes/alpha/.nucleus/jobs/jk3.log", "41 min"] {
                assert!(text.contains(part), "{part:?} missing from {text:?}");
            }
        }
        assert!(wake_text(&ended(State::Exited, Some(0), "n=28 done\n")).contains("n=28 done"));
        assert!(wake_text(&ended(State::Exited, Some(1), &long)).contains("[31mlast line"));
        let mut stopped = ended(State::Stopped, None, "");
        (stopped.stopped_by, stopped.stopped_author) = (Some(By::Person), Some("a-1".into()));
        assert!(wake_text(&stopped).contains("stopped by a person (account a-1)"));
        let mut left = ended(State::Exited, Some(0), "");
        left.stopped_leftovers = Some(2);
        assert!(wake_text(&left).contains("The 2 processes its command left running"));
        // However much there is to say, where its output is still fits.
        let mut wordy = ended(State::Exited, None, &long);
        (wordy.command, wordy.stopped_leftovers) = ("\u{20ac}".repeat(200), Some(2));
        let text = wake_text(&wordy);
        let request = conversation::RemindRequest { after_ms: 0, text: text.clone() };
        assert!(request.validate(conversation::ReminderAction::Wake).is_ok(), "{text:?}");
        assert!(
            text.contains("/homes/alpha/.nucleus/jobs/jk3.log (shell_jobs output jk3 reads it)"),
            "{text:?}"
        );
        let mut lost = ended(State::Interrupted, None, "");
        lost.note = Some("its shell host ended".into());
        assert!(wake_text(&lost).contains("its shell host ended"));
    }
}
