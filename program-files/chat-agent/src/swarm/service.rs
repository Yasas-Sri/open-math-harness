//! The swarm service: this program started with `--swarm-service`, a
//! resident service of the VM's composition providing `swarm`. It keeps every
//! swarm's board, decides registrations with the duplicate check, takes
//! findings and members' reports, starts and stops the members, writes each
//! swarm's progress to a records file of its own, wakes the session whose
//! agent started a swarm with each member's report and when the swarm runs
//! out unsolved or a restart interrupted it, and appends an `ultramath_ended`
//! reference to the session's conversation when a swarm ends.
//!
//! It never waits for a session's agent, and nothing it does waits for one
//! swarm on behalf of another: one thread serves every call as it arrives,
//! and its own slow calls (the duplicate checks, the directories of a new
//! swarm, the wake-ups) are submitted without waiting (SDK `submit`,
//! `next_answer`), as the suggestions and discovery services do.
//! Registrations of one swarm are decided one at a time, in arrival order,
//! each against the board as the previous one left it; another swarm's are
//! not held up.
//!
//! **Records.** A swarm's progress (its start, its members' runs,
//! directions, refusals, findings, reports, verdicts, model use and end) is
//! written to `<state>/records/<project>/<session>/<swarm>.jsonl`, one
//! record a line with its `seq` and `at_ms`, and never into the
//! conversation. The session's agent reads it with `records`, by page; the
//! path holds the session, so an agent reaches its own session's swarms
//! only. The UI service holds `status` and `records` on every session for a
//! team's window, which only reads them. The files are kept: a swarm's records outlive it and the service.
//! A record that cannot be written is said on stderr and counted in the
//! swarm's `status`.
//!
//! **Forgetting.** `forget` on `[project]`, the UI service's while it
//! deletes the project, ends the members of the project's swarms, answers
//! the starts, registrations and reports that waited, and removes their
//! markers and `<state>/records/<project>/`.
//!
//! Swarms live in this process. A marker of each running one is kept in the
//! state directory only so that, after a restart of the VM's Nucleus, the
//! next service records it `interrupted` and wakes its session: nothing
//! resumes a swarm, and its members died with the process that started
//! them.
//!
//! **Privileges.** The service holds `spawn_as` and the chat program, to
//! start members; `fs` and `shell` on every project, `model complete` and
//! `web`, which it hands them narrowed; and `wake` and `ultramath_ended` on
//! every session.
//! Compromised, it can start agents as any project, spend on any model,
//! write any project's files, wake any session with text of its choosing
//! (shown to the agent as another process's, untrusted), write anything into
//! its records, decide any registration and append completion references;
//! holding neither `post` nor `say`, it cannot write agent text into a
//! conversation or speak as a person. A member's text is untrusted for the
//! other members, the duplicate check and the judge: a member can steer the
//! others by what it posts.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nucleus_conversation_interface as conversation;
use nucleus_model_interface as model;
use nucleus_protocol::{
    ACTION_PROVIDE, ACTION_SPAWN_AS, ACTION_SPAWN_EXECUTABLE, DispatchId, EndpointName, Error, ErrorCode,
    HandleId, HandleInfo, KERNEL_ENDPOINT, Payload, ProcessId, RequestId, ResourcePath,
};
use nucleus_sdk::{ChannelError, Connection, Dispatch, Event};
use nucleus_swarm_interface::{
    self as wire, MAX_VIEW_BYTES, MEMBER_ACTIONS, MemberState, Outcome, RecordsResponse, SubmitRequest,
    SwarmRecord,
};
use serde_json::{Value, json};

use super::board::Board;
use super::members;

mod claims;
mod start;
mod verdicts;

type Kernel = Connection<std::io::Stdin, std::io::Stdout>;

/// How often deadlines are looked at.
const SWEEP: Duration = Duration::from_secs(5);
/// Deadlines of the service's own calls.
const PREPARE_DEADLINE_MS: u64 = 120_000;
const WAKE_DEADLINE_MS: u64 = 10_000;
const CHECK_DEADLINE_MS: u64 = 120_000;
/// Time kept back from a waiting registration's deadline for the reply.
const REPLY_MARGIN: Duration = Duration::from_secs(2);
/// Registrations waiting in one swarm: one per member at most.
const MAX_QUEUED: usize = wire::DEFAULT_MEMBERS as usize * 4;
/// Most swarms that ended kept for `status`, one per session at most; their
/// records stay on disk for `records`.
const MAX_ENDED: usize = 32;
/// How often a running swarm's model calls are recorded, at most.
const USAGE_EVERY: Duration = Duration::from_secs(60);

fn invalid(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, why.into())
}

fn failed(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::ServiceError, why.into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Startup options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where each swarm's records and each running swarm's marker are kept
    /// (writable, private).
    pub state: PathBuf,
    /// The chat program in this process's namespace: what a member runs.
    pub member_executable: String,
    /// The operator's arguments every member starts with.
    pub member_args: Vec<String>,
    /// The models members may run, the default first.
    pub models: Vec<String>,
    /// Where the projects' folders are in this namespace: members run as
    /// their project when this is given and the service
    /// holds `kernel spawn_as`.
    pub projects: Option<String>,
    /// Swarms running in the VM at once.
    pub max_swarms: usize,
}

impl Options {
    /// Parses `--state <dir> --member-executable <path> --model <id>...
    /// [--member-arg <arg>]... [--projects <dir>] [--max-swarms <n>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            state: PathBuf::new(),
            member_executable: String::new(),
            member_args: Vec::new(),
            models: Vec::new(),
            projects: None,
            max_swarms: 2,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--state" => options.state = PathBuf::from(value),
                "--member-executable" => options.member_executable = value,
                "--member-arg" => options.member_args.push(value),
                "--model" => {
                    if options.models.contains(&value) || model::resource_for(&value).is_err() {
                        return Err(format!("--model {value}: a duplicate, or not a model id"));
                    }
                    options.models.push(value);
                }
                "--projects" => options.projects = Some(value),
                "--max-swarms" => {
                    options.max_swarms = value
                        .parse()
                        .ok()
                        .filter(|n| (1..=8).contains(n))
                        .ok_or("--max-swarms must be between 1 and 8")?;
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if !options.state.is_absolute() || options.member_executable.is_empty() || options.models.is_empty() {
            return Err(
                "--state (absolute), --member-executable and at least one --model are required".into()
            );
        }
        // A member's reasoning effort is its swarm's, whose `start` names it.
        if options.member_args.iter().any(|arg| arg == crate::sessions::EFFORT_FLAG) {
            return Err(
                "--member-arg --reasoning-effort is refused: `swarm start` names a swarm's reasoning effort"
                    .into(),
            );
        }
        // What a member is started with must be what a member accepts: a
        // misconfiguration fails here, not at a swarm's first member.
        let probe = ["--model", "a/b", "--task", "t", super::member::FLAG, super::member::FREE];
        let mut args = options.member_args.clone();
        args.extend(probe.map(str::to_owned));
        crate::Options::parse(args).map_err(|e| format!("--member-arg: {e}"))?;
        Ok(options)
    }
}

/// What the service holds, found at startup; it fails then if anything is
/// missing, never when a swarm needs it.
struct Authority {
    provide: HandleId,
    spawn: HandleId,
    spawn_as: Option<HandleId>,
    /// `conversation wake`: a member's report, and a swarm's end, reach the
    /// session's agent as a reminder.
    wake: HandleId,
    ended: HandleId,
    held: members::Held,
    /// `(id, resource, delegable complete handle)` of each offered model.
    models: Vec<(String, ResourcePath, HandleId)>,
}

impl Authority {
    fn find(handles: &[HandleInfo], options: &Options) -> Result<Self, String> {
        let has = |h: &HandleInfo, endpoint: &str, actions: &[&str]| {
            h.endpoint.as_str() == endpoint
                && actions.iter().all(|a| h.actions.iter().any(|b| b.as_str() == *a))
        };
        let find = |endpoint: &str, actions: &[&str], delegable: bool, within: Option<&ResourcePath>| {
            handles
                .iter()
                .find(|h| {
                    has(h, endpoint, actions)
                        && (h.delegable || !delegable)
                        && within.is_none_or(|resource| resource.is_within(&h.resource))
                })
                .map(|h| h.handle)
                .ok_or_else(|| {
                    let kind = if delegable { "a delegable" } else { "a" };
                    format!("started without {kind} `{endpoint}` capability for {actions:?}")
                })
        };
        let resource = |segments: Vec<String>| ResourcePath::new(segments).map_err(|e| e.to_string());
        let endpoint = resource(vec!["endpoint".into(), wire::ENDPOINT.into()])?;
        let executable = resource(
            std::iter::once("executable".to_owned())
                .chain(options.member_executable.split('/').filter(|s| !s.is_empty()).map(str::to_owned))
                .collect(),
        )?;
        let identity = resource(vec!["identity".into()])?;
        let spawn_as = find(KERNEL_ENDPOINT, &[ACTION_SPAWN_AS], false, Some(&identity)).ok();
        if spawn_as.is_some() != options.projects.is_some() {
            return Err("kernel spawn_as and --projects go together: members run as their project".into());
        }
        let mut models = Vec::new();
        for id in &options.models {
            let resource = model::resource_for(id).map_err(|e| e.to_string())?;
            let handle =
                find(model::ENDPOINT, &[model::ACTION_COMPLETE], true, Some(&resource)).map_err(|_| {
                    format!("offered model `{id}` is not covered by a delegable `model complete`")
                })?;
            models.push((id.clone(), resource, handle));
        }
        Ok(Self {
            provide: find(KERNEL_ENDPOINT, &[ACTION_PROVIDE], false, Some(&endpoint))?,
            spawn: find(KERNEL_ENDPOINT, &[ACTION_SPAWN_EXECUTABLE], false, Some(&executable))?,
            spawn_as,
            wake: find(conversation::ENDPOINT, &[conversation::ACTION_WAKE], false, None)?,
            ended: find(conversation::ENDPOINT, &[conversation::ACTION_ULTRAMATH_ENDED], false, None)?,
            held: members::Held::find(handles)?,
            models,
        })
    }
}

/// A member of a running swarm.
struct Member {
    name: String,
    process: Option<ProcessId>,
    state: MemberState,
    runs: u32,
    /// Model calls of its current or last run, as its loop reported them,
    /// and of its runs before that.
    calls: u64,
    spent: u64,
}

/// A registration waiting for its duplicate check.
struct Registration {
    dispatch: DispatchId,
    member: String,
    text: String,
    /// When its caller stops waiting.
    until: Instant,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    /// Its time ran out with submissions waiting: they may still be judged,
    /// nobody runs again.
    Closing,
    Ended(Outcome),
}

struct Swarm {
    id: String,
    project: String,
    session: String,
    /// The session's working directory, as the `fs` and `shell` resource.
    root: ResourcePath,
    task: String,
    model: String,
    model_resource: ResourcePath,
    model_handle: HandleId,
    /// What every run of its members is started with; `None`, the provider's own.
    effort: Option<wire::ReasoningEffort>,
    members: Vec<Member>,
    board: Board,
    member_steps: u32,
    budget: u64,
    reserved: u64,
    checks: u64,
    deadline: Instant,
    deadline_at_ms: u64,
    phase: Phase,
    queue: VecDeque<Registration>,
    checking: Option<(RequestId, Registration)>,
    /// Members whose report is on its way to the session's agent.
    reporting: Vec<String>,
    /// The model calls the last usage record said, and when it was written.
    usage: (u64, Option<Instant>),
    /// The `seq` of the next record, and the records that could not be written.
    next_seq: u64,
    unwritten: u64,
}

impl Swarm {
    fn session_resource(&self) -> Result<ResourcePath, Error> {
        ResourcePath::new([&self.project, &self.session])
    }

    /// Appends `record` to the swarm's records file, said on stderr and
    /// counted when it cannot be.
    fn write(&mut self, state: &Path, record: &SwarmRecord) {
        let path = records_path(state, &self.project, &self.session, &self.id);
        match append(&path, self.next_seq, record) {
            Ok(()) => self.next_seq += 1,
            Err(e) => {
                self.unwritten += 1;
                eprintln!("nucleus-swarm: a record of swarm {} could not be written: {e}", self.id);
            }
        }
    }

    fn slot(&self, member: &str) -> members::Slot {
        members::Slot {
            project: self.project.clone(),
            session: self.session.clone(),
            swarm: self.id.clone(),
            member: member.to_owned(),
            root: self.root.clone(),
        }
    }

    fn running(&self) -> bool {
        matches!(self.phase, Phase::Running | Phase::Closing)
    }

    /// The model calls the swarm has made: its members' runs and its checks.
    fn calls(&self) -> u64 {
        self.members.iter().map(|m| m.spent + m.calls).sum::<u64>() + self.checks
    }

    fn minutes_left(&self) -> u64 {
        self.deadline.saturating_duration_since(Instant::now()).as_secs() / 60
    }

    fn state_name(&self) -> String {
        match self.phase {
            Phase::Running => "running".into(),
            Phase::Closing => "closing".into(),
            Phase::Ended(outcome) => serde_json::to_value(outcome)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        }
    }
}

/// A start waiting for the members' directories.
struct Starting {
    dispatch: DispatchId,
    project: String,
    session: String,
    start: wire::Start,
    model: (String, ResourcePath, HandleId),
    id: String,
    /// The project's repository, as the `fs` and `shell` resource.
    root: ResourcePath,
    cancelled: bool,
}

/// A member's report on its way to the session's agent: its submission is
/// taken once the wake-up carrying it is set, and refused if it is not.
struct Report {
    swarm: String,
    member: String,
    direction: String,
    dispatch: DispatchId,
    request: SubmitRequest,
    /// The member's call was withdrawn: it is answered no more.
    cancelled: bool,
}

/// What a submitted call of the service's own is for.
enum Call {
    Prepare(u64),
    Check(String),
    /// The wake-up carrying a member's report.
    Report(Box<Report>),
    /// A wake-up of the service's own, about the swarm named.
    Wake(String),
    Ended(String),
}

pub struct Service {
    options: Options,
    authority: Authority,
    swarms: BTreeMap<String, Swarm>,
    /// Each session's latest swarm.
    latest: BTreeMap<(String, String), String>,
    starting: BTreeMap<u64, Starting>,
    next_start: u64,
    calls: BTreeMap<RequestId, Call>,
    last_id_ms: u64,
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

impl Service {
    fn new(options: Options, authority: Authority) -> Self {
        Self {
            options,
            authority,
            swarms: BTreeMap::new(),
            latest: BTreeMap::new(),
            starting: BTreeMap::new(),
            next_start: 0,
            calls: BTreeMap::new(),
            last_id_ms: 0,
        }
    }

    // ---- what is recorded, and what wakes a session ---------------------------------------------

    /// Appends `record` to the records of swarm `swarm_id`.
    fn record(&mut self, swarm_id: &str, record: SwarmRecord) {
        let state = self.options.state.clone();
        if let Some(swarm) = self.swarms.get_mut(swarm_id) {
            swarm.write(&state, &record);
        }
    }

    /// Wakes the session of swarm `swarm_id` now with `text`, for `call`.
    fn wake(
        &mut self,
        kernel: &mut Kernel,
        session: &ResourcePath,
        text: String,
        call: Call,
    ) -> Result<Result<(), Error>, ChannelError> {
        let request = conversation::RemindRequest { after_ms: 0, text };
        let submitted = kernel.submit(
            self.authority.wake,
            conversation::ACTION_WAKE,
            session,
            &request,
            Some(WAKE_DEADLINE_MS),
        )?;
        Ok(submitted.map(|id| drop(self.calls.insert(id, call))))
    }

    // ---- the event loop's entry points ----------------------------------------------------------

    fn dispatch(&mut self, kernel: &mut Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        let segments = d.resource.segments();
        let action = d.action.as_str();
        let outcome = match (segments.len(), action) {
            (1, wire::ACTION_STOP) => self.stop_project(kernel, &segments[0], &d.payload)?,
            (1, wire::ACTION_FORGET) => self.forget_project(kernel, &segments[0], &d.payload)?,
            (2, wire::ACTION_START) => {
                return match self.start(kernel, d)? {
                    Ok(()) => Ok(()),
                    Err(e) => kernel.reply(d.id, Err(e)),
                };
            }
            (2, wire::ACTION_STATUS) => self.status(&segments[0], &segments[1], &d.payload),
            (2, wire::ACTION_RECORDS) => self.records(&segments[0], &segments[1], &d.payload),
            (2, wire::ACTION_VERDICT) => self.verdict(kernel, &segments[0], &segments[1], &d.payload)?,
            (2, wire::ACTION_STOP) => self.stop(kernel, &segments[0], &segments[1], &d.payload)?,
            (4, _) if MEMBER_ACTIONS.contains(&action) => match self.member_call(kernel, d)? {
                Some(outcome) => outcome,
                // A registration waits for its duplicate check, a report for
                // the wake-up that carries it.
                None => return Ok(()),
            },
            _ => Err(invalid(format!("{action} is not an action on {}", d.resource))),
        };
        kernel.reply(d.id, outcome)
    }

    /// The caller of `dispatch` gave up: a start or a registration it waited
    /// for is not carried out, and a report on its way is answered no more.
    fn cancel(&mut self, dispatch: DispatchId) {
        for start in self.starting.values_mut().filter(|s| s.dispatch == dispatch) {
            start.cancelled = true;
        }
        for call in self.calls.values_mut() {
            if let Call::Report(report) = call
                && report.dispatch == dispatch
            {
                report.cancelled = true;
            }
        }
        for swarm in self.swarms.values_mut() {
            swarm.queue.retain(|r| r.dispatch != dispatch);
            if let Some((_, registration)) = &mut swarm.checking
                && registration.dispatch == dispatch
            {
                registration.cancelled = true;
            }
        }
    }

    fn answer(
        &mut self,
        kernel: &mut Kernel,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        match self.calls.remove(&request) {
            Some(Call::Prepare(key)) => self.prepared(kernel, key, result)?,
            Some(Call::Check(swarm)) => self.checked(kernel, &swarm, request, result)?,
            Some(Call::Report(report)) => self.reported(kernel, *report, result)?,
            Some(Call::Ended(swarm)) => {
                if let Err(e) = result {
                    eprintln!(
                        "nucleus-swarm: completion of {swarm} could not be added to the conversation: {e}"
                    );
                }
            }
            Some(Call::Wake(swarm)) => {
                if let Err(e) = result {
                    eprintln!("nucleus-swarm: the session of swarm {swarm} could not be woken: {e}");
                }
            }
            None => {}
        }
        Ok(())
    }

    fn child_exited(&mut self, kernel: &mut Kernel, child: ProcessId) -> Result<(), ChannelError> {
        let found = self.swarms.iter_mut().find_map(|(id, swarm)| {
            swarm.members.iter_mut().find(|m| m.process == Some(child)).map(|m| (id.clone(), m))
        });
        let Some((id, member)) = found else { return Ok(()) };
        member.process = None;
        if member.state != MemberState::Running {
            return Ok(());
        }
        member.state = MemberState::Ended;
        let name = member.name.clone();
        self.member_ended(
            &id,
            &name,
            "its run ended without a solution (its step or time budget, or its model stopped)",
        );
        self.settle(kernel, &id, true)
    }

    /// Ends what ran out of time.
    fn sweep(&mut self, kernel: &mut Kernel) -> Result<(), ChannelError> {
        let now = Instant::now();
        let due: Vec<String> = self
            .swarms
            .values()
            .filter(|s| s.phase == Phase::Running && now >= s.deadline)
            .map(|s| s.id.clone())
            .collect();
        for id in due {
            self.stop_members(kernel, &id, "the swarm's time ran out")?;
            if let Some(swarm) = self.swarms.get_mut(&id) {
                swarm.phase = Phase::Closing;
            }
            self.settle(kernel, &id, true)?;
        }
        self.report_usage();
        Ok(())
    }

    /// Records each running swarm's model calls, at most once a minute and
    /// only when they changed.
    fn report_usage(&mut self) {
        let state = self.options.state.clone();
        for swarm in self.swarms.values_mut().filter(|s| s.running()) {
            let calls = swarm.calls();
            let (said, at) = swarm.usage;
            if calls == said || at.is_some_and(|at| at.elapsed() < USAGE_EVERY) {
                continue;
            }
            swarm.usage = (calls, Some(Instant::now()));
            let record = SwarmRecord::Usage { swarm: swarm.id.clone(), calls };
            swarm.write(&state, &record);
        }
    }

    // ---- the state directory --------------------------------------------------------------------

    fn state_file(&self, id: &str) -> PathBuf {
        self.options.state.join(format!("{id}.json"))
    }

    fn remember(&self, swarm: &Swarm) -> std::io::Result<()> {
        let record = json!({"v": 1, "swarm": swarm.id, "project": swarm.project, "session": swarm.session});
        let path = self.state_file(&swarm.id);
        let temporary = path.with_extension("json.next");
        std::fs::write(&temporary, record.to_string())?;
        std::fs::rename(&temporary, &path)
    }

    fn unmark(&self, id: &str) {
        if let Err(e) = std::fs::remove_file(self.state_file(id))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("nucleus-swarm: the state record of swarm {id} could not be removed: {e}");
        }
    }
}

impl Service {
    /// Stops a project's swarms without discarding their boards or records.
    /// The archive workflow confirms the project's cgroup is empty afterwards.
    fn stop_project(
        &mut self,
        kernel: &mut Kernel,
        project: &str,
        payload: &Payload,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        if let Err(e) = payload.decode::<nucleus_projects_interface::Empty>() {
            return Ok(Err(e));
        }
        let starts: Vec<_> =
            self.starting.iter().filter(|(_, s)| s.project == project).map(|(key, _)| *key).collect();
        for key in starts {
            if let Some(start) = self.starting.remove(&key)
                && !start.cancelled
            {
                kernel.reply(start.dispatch, Err(invalid("the project was archived")))?;
            }
        }
        let ids: Vec<_> = self
            .swarms
            .values()
            .filter(|s| s.project == project && s.running())
            .map(|s| s.id.clone())
            .collect();
        let checks: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(request, call)| match call {
                Call::Check(id) if ids.contains(id) => Some(*request),
                _ => None,
            })
            .collect();
        for request in checks {
            kernel.cancel(request)?;
        }
        for id in ids {
            self.stop_members(kernel, &id, "the project was archived")?;
            if let Some(swarm) = self.swarms.get_mut(&id) {
                swarm.board.drop_pending();
            }
            self.finish(
                kernel,
                &id,
                nucleus_swarm_interface::Outcome::Stopped,
                "the project was archived",
                false,
            )?;
        }
        Ok(Payload::encode(&json!({})))
    }

    /// Forgets a project's swarms after stopping their members (module doc).
    fn forget_project(
        &mut self,
        kernel: &mut Kernel,
        project: &str,
        payload: &Payload,
    ) -> Result<Result<Payload, Error>, ChannelError> {
        if let Err(e) = payload.decode::<nucleus_projects_interface::Empty>() {
            return Ok(Err(e));
        }
        if !nucleus_projects_interface::valid_name(project) {
            return Ok(Err(invalid("forget is a project's: its resource is [project]")));
        }
        let gone = || invalid(format!("the project {project} was deleted"));
        let starts: Vec<u64> =
            self.starting.iter().filter(|(_, s)| s.project == project).map(|(k, _)| *k).collect();
        for key in starts {
            if let Some(start) = self.starting.remove(&key)
                && !start.cancelled
            {
                kernel.reply(start.dispatch, Err(gone()))?;
            }
        }
        let ids: Vec<String> =
            self.swarms.values().filter(|s| s.project == project).map(|s| s.id.clone()).collect();
        for id in ids {
            let Some(mut swarm) = self.swarms.remove(&id) else { continue };
            for member in &mut swarm.members {
                if let Some(process) = member.process.take()
                    && let Err(e) = kernel.kill(process)?
                {
                    eprintln!("nucleus-swarm: member {} of {id} could not be stopped: {e}", member.name);
                }
            }
            let waiting = swarm.queue.drain(..).chain(swarm.checking.take().map(|(_, r)| r));
            for registration in waiting.filter(|r| !r.cancelled).collect::<Vec<_>>() {
                kernel.reply(registration.dispatch, Err(gone()))?;
            }
            self.unmark(&id);
        }
        self.latest.retain(|(of, _), _| of != project);
        let records = self.options.state.join("records").join(project);
        let removed = match std::fs::symlink_metadata(&records) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&records),
            Ok(_) => std::fs::remove_file(&records),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        Ok(match removed {
            Ok(()) => Payload::encode(&json!({})),
            Err(e) => Err(failed(format!("the swarm records of {project}: {e}"))),
        })
    }
}

/// Where a swarm's records are: beneath its session, so an agent reaches its
/// own session's swarms only.
fn records_path(state: &Path, project: &str, session: &str, swarm: &str) -> PathBuf {
    state.join("records").join(project).join(session).join(format!("{swarm}.jsonl"))
}

/// The session's newest swarm that has records: ids grow with their start
/// time, so the longest and then greatest.
fn latest_recorded(state: &Path, project: &str, session: &str) -> Option<String> {
    let directory = records_path(state, project, session, "x");
    std::fs::read_dir(directory.parent()?)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_suffix(".jsonl").map(str::to_owned))
        .filter(|id| wire::valid_id(id))
        .max_by_key(|id| (id.len(), id.clone()))
}

/// Appends one record, with its `seq` and the time, to the file at `path`.
fn append(path: &Path, seq: u64, record: &SwarmRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_value(record).map_err(std::io::Error::other)?;
    if let Some(fields) = line.as_object_mut() {
        fields.insert("seq".into(), json!(seq));
        fields.insert("at_ms".into(), json!(now_ms()));
    }
    let mut text = line.to_string();
    text.push('\n');
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(text.as_bytes())
}

/// The `seq` the next record of the file at `path` takes: one past its last.
fn next_seq_of(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else { return 0 };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok()?["seq"].as_u64())
        .max()
        .map_or(0, |seq| seq + 1)
}

/// One page of the records file at `path`: from `after`, at most `limit`
/// records and [`MAX_VIEW_BYTES`] of them, ending at a whole record.
fn read_records(
    path: &Path,
    swarm: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<RecordsResponse, Error> {
    let file = std::fs::File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => invalid(format!("this session has no swarm {swarm}")),
        _ => failed(format!("the records of swarm {swarm}: {e}")),
    })?;
    let (mut records, mut bytes, mut next) = (Vec::new(), 0, None);
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| failed(format!("the records of swarm {swarm}: {e}")))?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(seq) = record["seq"].as_u64() else { continue };
        if after.is_some_and(|after| seq <= after) {
            continue;
        }
        if records.len() == limit || (!records.is_empty() && bytes + line.len() > MAX_VIEW_BYTES) {
            next = records.last().and_then(|r: &Value| r["seq"].as_u64());
            break;
        }
        bytes += line.len();
        records.push(record);
    }
    Ok(RecordsResponse { swarm: swarm.to_owned(), records, next })
}

/// Records every swarm a previous service left running as interrupted,
/// wakes its session to say so, and forgets its marker.
fn mark_interrupted(kernel: &mut Kernel, authority: &Authority, options: &Options) -> Result<(), String> {
    let entries = match std::fs::read_dir(&options.state) {
        Ok(entries) => entries,
        Err(e) => return Err(format!("--state {}: {e}", options.state.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let marker: Option<(String, String, String)> = std::fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() <= 4096)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| {
                let field = |name: &str| v[name].as_str().map(str::to_owned);
                Some((field("swarm")?, field("project")?, field("session")?))
            });
        if let Some((swarm, project, session)) = marker
            && wire::valid_id(&swarm)
            && let Ok(resource) = ResourcePath::new([project.as_str(), session.as_str()])
        {
            let file = records_path(&options.state, &project, &session, &swarm);
            let ended = SwarmRecord::Ended {
                swarm: swarm.clone(),
                outcome: Outcome::Interrupted,
                note: "the VM's Open Math OS runtime restarted; nothing resumes a swarm".into(),
                calls: 0,
                reserved_calls: 0,
                checks: 0,
            };
            if let Err(e) = append(&file, next_seq_of(&file), &ended) {
                eprintln!("nucleus-swarm: the end of swarm {swarm} could not be recorded: {e}");
            }
            let request = conversation::UltramathEndedRequest { swarm: swarm.clone() };
            match kernel.call(
                authority.ended,
                conversation::ACTION_ULTRAMATH_ENDED,
                &resource,
                &request,
                Some(WAKE_DEADLINE_MS),
            ) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!(
                    "nucleus-swarm: completion of {swarm} could not be added to the conversation: {e}"
                ),
                Err(e) => return Err(e.to_string()),
            }
            let text = format!(
                "Swarm {swarm} was interrupted: the VM's Open Math OS runtime restarted, and nothing resumes a swarm. Its \
                 records: ultramath_records with swarm {swarm}. Tell the person what they show."
            );
            let request = conversation::RemindRequest { after_ms: 0, text };
            let woken = kernel.call(
                authority.wake,
                conversation::ACTION_WAKE,
                &resource,
                &request,
                Some(WAKE_DEADLINE_MS),
            );
            match woken {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    eprintln!("nucleus-swarm: the session of swarm {swarm} could not be woken: {e}")
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("nucleus-swarm: {}: {e}", path.display());
        }
    }
    Ok(())
}

/// Serves `swarm` until told to terminate.
pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = Options::parse(args)?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = Authority::find(kernel.handles(), &options)?;
    mark_interrupted(&mut kernel, &authority, &options)?;
    let interface = wire::interface(EndpointName::new(wire::ENDPOINT).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let provide = authority.provide;
    let service = Rc::new(RefCell::new(Service::new(options, authority)));
    // Calls are served as they arrive, also while `next_answer` waits. While
    // the loop below holds the service for an answer (its kernel requests
    // can meet new events), an event waits in the backlog for the next turn.
    let serving = Rc::clone(&service);
    kernel.set_event_handler(move |kernel, event| {
        let Ok(mut service) = serving.try_borrow_mut() else { return Ok(false) };
        match event {
            Event::Dispatch(d) => service.dispatch(kernel, d)?,
            Event::CancelDispatch(dispatch) => service.cancel(*dispatch),
            Event::ChildExited(child) => service.child_exited(kernel, *child)?,
            Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(false),
        }
        Ok(true)
    });
    kernel.provide(provide, interface).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let waker = kernel.waker();
    std::thread::spawn(move || {
        while waker.wake() {
            std::thread::sleep(SWEEP);
        }
    });
    loop {
        let outcome = match kernel.next_answer() {
            Ok(Some((request, result))) => service.borrow_mut().answer(&mut kernel, request, result),
            Ok(None) => service.borrow_mut().sweep(&mut kernel),
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => Err(e),
        };
        outcome.map_err(|e| e.to_string())?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn options_name_the_state_the_member_program_and_its_models() {
        let parse = |args: &[&str]| Options::parse(args.iter().map(|s| (*s).to_owned()));
        let base =
            ["--state", "/state", "--member-executable", "/programs/nucleus-chat-agent", "--model", "a/b"];
        let parsed = parse(&base).unwrap();
        assert_eq!((parsed.models.as_slice(), parsed.max_swarms), (&["a/b".to_owned()][..], 2));
        assert!(
            parse(&[&base[..], &["--member-arg", "--personality", "--member-arg", "none"]].concat()).is_ok()
        );
        for bad in [
            &["--member-arg", "--no-such-flag", "--member-arg", "x"][..],
            // A swarm's effort is its start's.
            &["--member-arg", "--reasoning-effort", "--member-arg", "low"],
            &["--model", "a/b"],
            &["--max-swarms", "0"],
            &["--unknown", "x"],
        ] {
            assert!(parse(&[&base[..], bad].concat()).is_err(), "{bad:?}");
        }
        assert!(parse(&["--state", "relative", "--member-executable", "/p", "--model", "a/b"]).is_err());
        assert_eq!(base36(0), "0");
        assert_eq!(base36(36 * 36), "100");
    }
}
