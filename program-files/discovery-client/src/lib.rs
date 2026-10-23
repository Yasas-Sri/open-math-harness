//! `nucleus-discovery-client`: the headless operator path. It
//! starts one Dream-RSI run of a project (`--project`, the default project
//! unless given) from a task file through the `discovery` interface, polls
//! `status` and exits with the run's outcome — the same interface and
//! authorization path as a chat agent, no operator shortcut.
//!
//! The client is an ordinary kernel process: its stdin and stdout are the
//! channel, so every line it prints goes to stderr, which the supervisor
//! leaves attached to the launcher's. The order of work makes a bad task cost
//! nothing: connect, find the `discovery start` handle (and the `status` one
//! unless `--no-wait`), read and validate the task, and only then call
//! `start`. The client applies the rules the document alone decides
//! ([`Task::validate`] with [`NO_CEILINGS`]); the service's ceilings, its
//! tasks directory and `lean statement` are the service's to apply, and its
//! refusal is printed as it came. `start` is irreversible: a call whose
//! outcome the kernel could not observe is reported as such and never retried.
//! It is also slow to answer: the service replies only once the run's
//! process has resolved every problem of the task through `lean statement`
//! and registered every statement the task supplies, which takes minutes for
//! a large curriculum; the call asks for [`START_DEADLINE_MS`] (the kernel
//! clamps it to its configured maximum) rather than the kernel's 30 s
//! default, after which the outcome would be unknown while the run started
//! anyway.
//!
//! While waiting, one line is printed per `status` that changed anything, at
//! the kernel's default deadline per call. A `status` that times out or finds
//! the endpoint's queue full is retried at the next poll — an observation is safe
//! to repeat — for at most [`MAX_TRANSIENT_POLL_SILENCE_MS`]. A
//! terminate request is noticed after the next `status` call, so within one
//! poll interval; the supervisor kills the process when its grace period ends
//! regardless.
//!
//! Exit status ([`Exit::code`]): 0 when the run reached `done` (or, with
//! `--no-wait`, was started); 1 for bad options, an unreadable or invalid
//! task, or a task the service refused; 2 when the run ended `stopped` or
//! `failed` (its error is printed); 3 when the channel broke, the process
//! lacks the authority, or the service could not be reached; 130 when the
//! kernel asked this process to exit.

use nucleus_discovery_interface::{
    ACTION_RESUME, ACTION_START, ACTION_STATUS, Ceilings, ResumeResult, RunState, RunStatus, StartResult,
    Task,
};
use nucleus_protocol::{EndpointName, Error, ErrorCode, HandleId, ResourcePath};
use nucleus_sdk::{ChannelError, Connection};
use std::fmt::{self, Write as _};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `--poll-ms` unless given.
pub const DEFAULT_POLL_MS: u64 = 10_000;
/// `--project` unless given: the project a VM is made with.
pub const DEFAULT_PROJECT: &str = "default";
/// Deadline asked for on `start`, which the service answers only after
/// resolving every problem of the task (module doc).
pub const START_DEADLINE_MS: u64 = 300_000;
/// How long `status` may go on timing out or finding the queue full before
/// the client gives up on the service. The bound is on the clock, not on a
/// count of polls: where this client is the only `run_to_completion` boot
/// entry, its exit shuts the launcher down and so ends the run it observes.
pub const MAX_TRANSIENT_POLL_SILENCE_MS: u64 = 1_800_000;

/// Milliseconds since the first of the current run of failing `status`
/// calls, or 0 when the last one answered.
fn silent_for(since: Option<Instant>) -> u64 {
    since.map_or(0, |t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
}
/// Longest error text repeated on one line.
const MAX_ERROR_CHARS: usize = 240;
/// What the client validates a task against: the service's ceilings are not
/// known here, so none apply, and [`Task::validate`] decides every other
/// rule of the document.
pub const NO_CEILINGS: Ceilings = Ceilings {
    max_concurrency: usize::MAX,
    max_usd_micro_per_problem: 0,
    max_problems_per_task: usize::MAX,
    max_width: usize::MAX,
};

/// Startup options, from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The task document (JSON, version 1).
    pub task: PathBuf,
    pub resume: Option<String>,
    /// The project the run is of.
    pub project: String,
    /// Poll `status` until the run ends; `--no-wait` returns after `start`.
    pub wait: bool,
    /// Interval between `status` calls while waiting.
    pub poll_ms: u64,
    /// Endpoint name the `discovery` capability was granted under.
    pub discovery_endpoint: String,
}

impl Options {
    /// Parses `--task <file> | --resume <run>` `[--project <name>] [--wait |
    /// --no-wait] [--poll-ms <n>] [--discovery-endpoint <name>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut task = None;
        let mut options = Options {
            task: PathBuf::new(),
            resume: None,
            project: DEFAULT_PROJECT.to_owned(),
            wait: true,
            poll_ms: DEFAULT_POLL_MS,
            discovery_endpoint: nucleus_discovery_interface::ENDPOINT.to_owned(),
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--wait" => {
                    options.wait = true;
                    continue;
                }
                "--no-wait" => {
                    options.wait = false;
                    continue;
                }
                _ => {}
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--task" => task = Some(PathBuf::from(value)),
                "--resume" => {
                    ResourcePath::new([value.as_str()]).map_err(|e| e.to_string())?;
                    options.resume = Some(value);
                }
                "--project" => {
                    if !nucleus_discovery_interface::valid_name(&value) {
                        return Err(format!("{flag} {value:?} is not a project name"));
                    }
                    options.project = value;
                }
                "--poll-ms" => {
                    options.poll_ms = value.parse().map_err(|_| format!("{flag} must be a number"))?
                }
                "--discovery-endpoint" => {
                    EndpointName::new(value.as_str()).map_err(|e| format!("{flag}: {e}"))?;
                    options.discovery_endpoint = value;
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        options.task = match task {
            Some(task) => task,
            None if options.resume.is_some() => PathBuf::new(),
            None => return Err("--task or --resume is required".into()),
        };
        if options.poll_ms == 0 {
            return Err("--poll-ms must be at least 1".to_owned());
        }
        Ok(options)
    }
}

/// How the client ended; [`Exit::code`] is the exit status documented on the crate.
#[derive(Clone, Debug, PartialEq)]
pub enum Exit {
    /// The run reached `done`, or, with `--no-wait`, was started.
    Done,
    /// Bad options.
    Usage(String),
    /// The task could not be read, failed validation, or was refused by the service.
    TaskRefused(String),
    /// The run ended `stopped` or `failed`.
    RunEnded(Box<RunStatus>),
    /// The channel broke, the process lacks the authority, or the service could not be reached.
    Failed(String),
    /// The kernel asked this process to exit.
    Terminated,
}

impl Exit {
    /// The process exit status (crate doc).
    pub fn code(&self) -> u8 {
        match self {
            Self::Done => 0,
            Self::Usage(_) | Self::TaskRefused(_) => 1,
            Self::RunEnded(_) => 2,
            Self::Failed(_) => 3,
            Self::Terminated => 130,
        }
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Done => f.write_str("done"),
            Self::Usage(why) | Self::TaskRefused(why) | Self::Failed(why) => f.write_str(why),
            Self::RunEnded(status) => {
                write!(f, "run {} {}", one_line(&status.run), state_name(status.state))?;
                match &status.error {
                    Some(error) => write!(f, ": {}", bounded(error)),
                    None => Ok(()),
                }
            }
            Self::Terminated => f.write_str("terminated by the kernel"),
        }
    }
}

/// Reads and parses the task document, then applies the rules the document
/// alone decides ([`NO_CEILINGS`]). The document is returned as written: the
/// service, not the client, fills in the defaults.
pub fn read_task(path: &Path) -> Result<Task, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let task: Task = serde_json::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
    task.validate(&NO_CEILINGS).map_err(|e| format!("{shown}: {e}"))?;
    Ok(task)
}

/// The line printed for a `start` result.
pub fn start_line(started: &StartResult) -> String {
    one_line(&format!(
        "started run {}: {} problems, {} queued ahead",
        started.run, started.problems, started.queued_ahead
    ))
}

/// The line printed for a `status` that changed anything: run, state,
/// progress, the active problems, tokens, the USD the providers charged and the error when
/// the service reports them. Text from the service is kept to one line and
/// the error is bounded.
pub fn status_line(status: &RunStatus) -> String {
    let mut line = format!(
        "run {} {}: {}/{} finished, {} solved, active [{}], tokens {} in {} out",
        status.run,
        state_name(status.state),
        status.finished,
        status.problems,
        status.solved,
        status.active.join(" "),
        status.tokens_in,
        status.tokens_out
    );
    if let Some(usd) = status.usd {
        // Writing into a String cannot fail.
        let _ = write!(line, ", usd {usd:.4}");
    }
    if status.failed_continuations > 0 {
        let _ = write!(line, ", failed {}", status.failed_continuations);
        if let Some(last) = &status.last_failure {
            let _ = write!(line, " (last: {})", bounded(last));
        }
    }
    if let Some(error) = &status.error {
        let _ = write!(line, ", error: {}", bounded(error));
    }
    one_line(&line)
}

/// The wire name of a state, as `status` reports it.
fn state_name(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Done => "done",
        RunState::Stopped => "stopped",
        RunState::Failed => "failed",
    }
}

/// Untrusted text made safe for a terminal line: control characters become spaces.
fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// [`one_line`], cut at [`MAX_ERROR_CHARS`].
fn bounded(text: &str) -> String {
    let line = one_line(text);
    if line.chars().count() <= MAX_ERROR_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_ERROR_CHARS).collect();
    cut.push_str("...");
    cut
}

/// Prints one progress line. Progress nobody can read is no reason to stop
/// watching the run, so a failed write is ignored.
fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// The first initial handle for `endpoint` that allows `action`, with the
/// scope it was granted on.
fn capability<R: Read, W: Write>(
    kernel: &Connection<R, W>,
    endpoint: &str,
    action: &str,
) -> Option<(HandleId, ResourcePath)> {
    let handle = kernel.find_handle(endpoint, action)?;
    let info = kernel.handles().iter().find(|h| h.handle == handle)?;
    Some((handle, info.resource.clone()))
}

fn describe(action: &str, error: &Error) -> String {
    format!("{action}: {:?}: {}", error.code, error.message)
}

/// Whether a refused `status` may simply be asked again at the next poll.
fn transient(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::Timeout | ErrorCode::QueueFull)
}

/// Starts the task and, unless `--no-wait`, watches the run to its end.
/// Progress lines go to `out`.
pub fn run<R: Read, W: Write>(options: &Options, mut kernel: Connection<R, W>, out: &mut dyn Write) -> Exit {
    let endpoint = options.discovery_endpoint.as_str();
    let action = if options.resume.is_some() { ACTION_RESUME } else { ACTION_START };
    let Some((start, _)) = capability(&kernel, endpoint, action) else {
        return Exit::Failed(format!("this process holds no `{endpoint} {action}` capability"));
    };
    let status = if options.wait {
        match capability(&kernel, endpoint, ACTION_STATUS) {
            Some((handle, _)) => Some(handle),
            None => {
                return Exit::Failed(format!(
                    "this process holds no `{endpoint} {ACTION_STATUS}` capability, which waiting needs \
                     (--no-wait only starts the run)"
                ));
            }
        }
    } else {
        None
    };
    let project = options.project.as_str();
    let (name, request, resource) = match &options.resume {
        Some(session) => {
            let resource = match ResourcePath::new([project, session.as_str()]) {
                Ok(resource) => resource,
                Err(e) => return Exit::TaskRefused(e.to_string()),
            };
            (session.clone(), serde_json::json!({}), resource)
        }
        None => match (read_task(&options.task), ResourcePath::new([project])) {
            (Ok(task), Ok(resource)) => (task.name.clone(), serde_json::json!(task), resource),
            (Err(why), _) => return Exit::TaskRefused(why),
            (_, Err(e)) => return Exit::TaskRefused(e.to_string()),
        },
    };
    let started = match kernel.call(start, action, &resource, &request, Some(START_DEADLINE_MS)) {
        Err(ChannelError::Terminated) => return Exit::Terminated,
        Err(e) => return Exit::Failed(format!("{action}: {e}")),
        Ok(Err(e)) if e.code == ErrorCode::InvalidInput => {
            return Exit::TaskRefused(describe(action, &e));
        }
        Ok(Err(e)) if e.code == ErrorCode::UnknownOutcome => {
            return Exit::Failed(format!(
                "{}; the run may have started: check `{endpoint} list` before starting it again",
                describe(action, &e)
            ));
        }
        Ok(Err(e)) => return Exit::Failed(describe(action, &e)),
        Ok(Ok(payload)) if options.resume.is_some() => match payload.decode::<ResumeResult>() {
            Ok(resumed) => {
                say(
                    out,
                    &format!(
                        "resumed session {}; {} prior model outcome(s) have unknown cost",
                        one_line(&name),
                        resumed.uncertain_model_calls
                    ),
                );
                resumed.start
            }
            Err(e) => return Exit::Failed(format!("resume: malformed reply: {}", e.message)),
        },
        Ok(Ok(payload)) => match payload.decode::<StartResult>() {
            Ok(started) => started,
            Err(e) => return Exit::Failed(format!("{action}: malformed reply: {}", e.message)),
        },
    };
    say(out, &start_line(&started));
    if started.run != name {
        return Exit::Failed(format!(
            "{action}: the service named the run {:?} for task {:?}",
            one_line(&started.run),
            name
        ));
    }
    if kernel.terminate_requested() {
        return Exit::Terminated;
    }
    let Some(status) = status else {
        return Exit::Done;
    };
    // The name passed validation, so it is a resource segment.
    let run = match ResourcePath::new([project, name.as_str()]) {
        Ok(run) => run,
        Err(e) => return Exit::Failed(format!("run name {:?}: {}", name, e.message)),
    };
    watch(&mut kernel, status, &run, options.poll_ms, out)
}

/// Polls `status` on `[project, run]` until the run ends, a terminate request arrives,
/// or a call fails for good.
fn watch<R: Read, W: Write>(
    kernel: &mut Connection<R, W>,
    status: HandleId,
    run: &ResourcePath,
    poll_ms: u64,
    out: &mut dyn Write,
) -> Exit {
    let mut last: Option<RunStatus> = None;
    let mut failures = 0u32;
    let mut failing_since: Option<Instant> = None;
    loop {
        let reply = match kernel.call(status, ACTION_STATUS, run, &serde_json::json!({}), None) {
            Ok(reply) => reply,
            Err(ChannelError::Terminated) => return Exit::Terminated,
            Err(e) => return Exit::Failed(format!("{ACTION_STATUS}: {e}")),
        };
        if kernel.terminate_requested() {
            return Exit::Terminated;
        }
        match reply {
            Err(e) if transient(e.code) && silent_for(failing_since) < MAX_TRANSIENT_POLL_SILENCE_MS => {
                failures += 1;
                if failing_since.is_none() {
                    failing_since = Some(Instant::now());
                }
                let waited = silent_for(failing_since) / 1000;
                let retry = format!("(the service is busy; {failures} in a row, {waited}s)");
                say(out, &one_line(&format!("{} {retry}", describe(ACTION_STATUS, &e))));
            }
            Err(e) => return Exit::Failed(describe(ACTION_STATUS, &e)),
            Ok(payload) => {
                failures = 0;
                failing_since = None;
                let current = match payload.decode::<RunStatus>() {
                    Ok(current) => current,
                    Err(e) => {
                        return Exit::Failed(format!("{ACTION_STATUS}: malformed reply: {}", e.message));
                    }
                };
                if last.as_ref() != Some(&current) {
                    say(out, &status_line(&current));
                }
                match current.state {
                    RunState::Done => return Exit::Done,
                    RunState::Stopped | RunState::Failed => return Exit::RunEnded(Box::new(current)),
                    RunState::Queued | RunState::Running => last = Some(current),
                }
            }
        }
        std::thread::sleep(Duration::from_millis(poll_ms));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use nucleus_discovery_interface::{RunState, TaskLimits, TaskShape};
    use nucleus_protocol::{
        ActionName, FromKernel, HandleInfo, Payload, ProcessId, RequestId, ResponseBody, ToKernel,
        read_frame, write_frame,
    };
    use serde::Serialize;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::io::Cursor;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn a_task_file_is_required_and_the_rest_has_defaults() {
        let options = parse(&["--task", "tasks/gen2.json"]).unwrap();
        assert_eq!(
            options,
            Options {
                task: PathBuf::from("tasks/gen2.json"),
                resume: None,
                project: "default".to_owned(),
                wait: true,
                poll_ms: 10_000,
                discovery_endpoint: "discovery".to_owned(),
            }
        );
        // Extra arguments, and what they must yield.
        type Accepted = (&'static [&'static str], fn(&Options) -> bool);
        let accepted: &[Accepted] = &[
            (&["--no-wait"], |o| !o.wait),
            (&["--no-wait", "--wait"], |o| o.wait),
            (&["--poll-ms", "250"], |o| o.poll_ms == 250),
            (&["--discovery-endpoint", "disc"], |o| o.discovery_endpoint == "disc"),
            (&["--project", "alpha"], |o| o.project == "alpha"),
        ];
        for (extra, holds) in accepted {
            let args = [&["--task", "t.json"][..], extra].concat();
            assert!(holds(&parse(&args).unwrap()), "{extra:?}");
        }
        for refused in [
            &[][..],
            &["--task"],
            &["--task", "t.json", "--poll-ms", "0"],
            &["--task", "t.json", "--poll-ms", "soon"],
            &["--task", "t.json", "--discovery-endpoint", "not a name"],
            &["--task", "t.json", "--project", "../alpha"],
            &["--task", "t.json", "--verbose", "1"],
        ] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
    }

    // --- Formatting -----------------------------------------------------------

    fn status(state: RunState) -> RunStatus {
        RunStatus {
            run: "gen2-heldout".into(),
            state,
            problems: 20,
            finished: 3,
            solved: 1,
            active: vec!["putnam_1962_b2".into(), "putnam_1985_a1".into()],
            tokens_in: 1000,
            tokens_out: 200,
            usd: None,
            error: None,
            task: shape(),
            failed_continuations: 0,
            last_failure: None,
            started_at_ms: Some(1_758_240_060_000),
            failure_reasons: vec![],
            other_failures: 0,
        }
    }

    /// A task's shape as `status` reports it (not shown on the status line).
    fn shape() -> TaskShape {
        TaskShape { policy: None, concurrency: 2, limits: TaskLimits::default(), worker_network: None }
    }

    #[test]
    fn a_status_line_is_one_compact_line() {
        let line = status_line(&RunStatus { usd: Some(0.25), ..status(RunState::Running) });
        for part in ["gen2-heldout", "running", "3/20", "putnam_1985_a1", "1000", "0.25"] {
            assert!(line.contains(part), "{part} in {line}");
        }
        // What a run, a problem or an error says cannot break the line or
        // drive the terminal, and an error is bounded.
        let odd = RunStatus {
            run: "a\nb".into(),
            active: vec!["p\rq".into()],
            error: Some(format!("worker\nexploded\t\u{1b}[31m{}", "x".repeat(1000))),
            ..status(RunState::Failed)
        };
        let line = status_line(&odd);
        assert!(!line.contains(['\n', '\r', '\u{1b}']), "{line}");
        assert!(line.contains("exploded") && line.len() < 400, "{line}");
    }

    #[test]
    fn state_names_are_the_wire_names() {
        for state in
            [RunState::Queued, RunState::Running, RunState::Done, RunState::Stopped, RunState::Failed]
        {
            assert_eq!(json!(state_name(state)), serde_json::to_value(state).unwrap());
        }
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(Exit::Done.code(), 0);
        assert_eq!(Exit::Usage("x".into()).code(), 1);
        assert_eq!(Exit::TaskRefused("x".into()).code(), 1);
        assert_eq!(Exit::RunEnded(Box::new(status(RunState::Stopped))).code(), 2);
        assert_eq!(Exit::RunEnded(Box::new(status(RunState::Failed))).code(), 2);
        assert_eq!(Exit::Failed("x".into()).code(), 3);
        assert_eq!(Exit::Terminated.code(), 130);
    }

    // --- The task file --------------------------------------------------------

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    /// A task file of its own for one test.
    fn task_file(text: &str) -> PathBuf {
        let n = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nucleus-discovery-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("task-{n}.json"));
        std::fs::write(&path, text).unwrap();
        path
    }

    const VALID: &str = r#"{"v":1,"name":"t","problems":["putnam_1962_b2"],"prompt_template":"{{ statement }}","concurrency":1}"#;

    #[test]
    fn a_task_is_read_and_validated_locally_without_ceilings() {
        let task = read_task(&task_file(VALID)).unwrap();
        assert_eq!((task.name.as_str(), task.concurrency), ("t", 1));
        assert_eq!(task.limits, TaskLimits::default(), "returned as written, defaults left to the service");

        let unknown = task_file(r#"{"v":1,"name":"t","bogus":true,"concurrency":1}"#);
        assert!(read_task(&unknown).unwrap_err().contains("bogus"), "an unknown field is refused at parse");
        let bad_name = task_file(&VALID.replace(r#""name":"t""#, r#""name":"Bad Name""#));
        let why = read_task(&bad_name).unwrap_err();
        assert!(why.contains("name must match") && why.contains("task-"), "the validator's message: {why}");
        let both = task_file(&VALID.replace(r#""problems""#, r#""problems_file":"c.txt","problems""#));
        assert!(
            read_task(&both).unwrap_err().contains("exactly one of problems, problems_file and statements")
        );
        let missing = std::env::temp_dir().join("nucleus-discovery-client-absent.json");
        assert!(read_task(&missing).unwrap_err().contains("absent.json"));

        // The ceilings are the service's: a wide concurrency and a large or absent cap pass here.
        let wide = task_file(&VALID.replace(r#""concurrency":1"#, r#""concurrency":64"#));
        assert!(read_task(&wide).is_ok());
        let capped = task_file(
            &VALID
                .replace(r#""concurrency":1"#, r#""concurrency":1,"limits":{"max_usd_per_problem":"1000"}"#),
        );
        assert!(read_task(&capped).is_ok());
        let zero = task_file(&VALID.replace(r#""concurrency":1"#, r#""concurrency":0"#));
        assert!(read_task(&zero).unwrap_err().contains("concurrency must be between 1 and"));
    }

    // --- A scripted kernel channel -------------------------------------------

    /// What the client writes, readable after `run` consumed the connection.
    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn calls(&self) -> Vec<ToKernel> {
            let bytes = self.0.borrow().clone();
            let mut cursor = Cursor::new(bytes);
            let mut calls = Vec::new();
            while let Ok(msg) = read_frame::<_, ToKernel>(&mut cursor) {
                calls.push(msg);
            }
            calls
        }
    }

    fn handle(actions: &[&str], resource: &[&str]) -> HandleInfo {
        HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new("discovery").unwrap(),
            actions: actions.iter().map(|a| ActionName::new(*a).unwrap()).collect::<BTreeSet<_>>(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            delegable: false,
            expires_in_ms: None,
            max_calls: None,
        }
    }

    fn hello(handles: Vec<HandleInfo>) -> FromKernel {
        FromKernel::Hello { process: ProcessId(7), handles }
    }

    fn answer(request: u64, value: &impl Serialize) -> FromKernel {
        let result = Ok(ResponseBody::Payload(Payload::encode(value).unwrap()));
        FromKernel::Response { request: RequestId(request), result }
    }

    fn refusal(request: u64, code: ErrorCode, message: &str) -> FromKernel {
        FromKernel::Response { request: RequestId(request), result: Err(Error::new(code, message)) }
    }

    fn started() -> StartResult {
        StartResult { run: "t".into(), problems: 1, queued_ahead: 0 }
    }

    fn run_status(state: RunState, finished: u64, solved: u64, active: &[&str]) -> RunStatus {
        RunStatus {
            run: "t".into(),
            state,
            problems: 1,
            finished,
            solved,
            active: active.iter().map(|s| (*s).to_owned()).collect(),
            tokens_in: 100 * finished,
            tokens_out: 10 * finished,
            usd: None,
            error: None,
            task: TaskShape {
                policy: None,
                concurrency: 1,
                limits: TaskLimits::default(),
                worker_network: None,
            },
            failed_continuations: 0,
            last_failure: None,
            started_at_ms: None,
            failure_reasons: vec![],
            other_failures: 0,
        }
    }

    /// Runs the client against a channel that plays `script` in order and
    /// returns its exit, the lines it printed and the calls it made.
    fn drive(args: &[&str], script: &[FromKernel]) -> (Exit, Vec<String>, Vec<ToKernel>) {
        let mut bytes = Vec::new();
        for msg in script {
            write_frame(&mut bytes, msg).unwrap();
        }
        let sink = Sink::default();
        let kernel = Connection::open(Cursor::new(bytes), sink.clone()).unwrap();
        let options = parse(args).unwrap();
        let mut out = Vec::new();
        let exit = run(&options, kernel, &mut out);
        let lines = String::from_utf8(out).unwrap().lines().map(str::to_owned).collect();
        (exit, lines, sink.calls())
    }

    fn action_of(call: &ToKernel) -> (&str, &[String]) {
        match call {
            ToKernel::Call { action, resource, .. } => (action.as_str(), resource.segments()),
            other => panic!("not a call: {other:?}"),
        }
    }

    #[test]
    fn resume_uses_the_session_resource_without_reading_a_task_or_replaying_tools() {
        let result = ResumeResult { start: started(), uncertain_model_calls: 2 };
        let script = [hello(vec![handle(&[ACTION_RESUME], &["t"])]), answer(1, &result)];
        let (exit, lines, calls) =
            drive(&["--resume", "t", "--task", "/does-not-exist", "--no-wait"], &script);
        assert_eq!(exit, Exit::Done);
        assert_eq!(calls.len(), 1);
        assert_eq!(action_of(&calls[0]), (ACTION_RESUME, &["default".to_owned(), "t".to_owned()][..]));
        assert!(lines.iter().any(|s| s.contains("2 prior model")));
        let (exit, _, calls) =
            drive(&["--resume", "t", "--no-wait"], &[hello(vec![handle(&[ACTION_START], &[])])]);
        assert!(matches!(exit, Exit::Failed(_)));
        assert!(calls.is_empty());
    }

    #[test]
    fn starts_then_prints_one_line_per_change_until_done() {
        let path = task_file(VALID);
        let script = [
            hello(vec![handle(&["start", "status", "stop", "list"], &[])]),
            answer(1, &started()),
            answer(2, &run_status(RunState::Running, 0, 0, &["putnam_1962_b2"])),
            answer(3, &run_status(RunState::Running, 0, 0, &["putnam_1962_b2"])),
            answer(4, &run_status(RunState::Running, 1, 1, &[])),
            answer(5, &run_status(RunState::Done, 1, 1, &[])),
        ];
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!(exit, Exit::Done);
        assert_eq!(lines.len(), 4, "the start, then a line per change of the status: {lines:?}");
        let ToKernel::Call { action, payload, .. } = &calls[0] else { panic!("{:?}", calls[0]) };
        assert_eq!(action.as_str(), "start");
        let sent: Task = payload.decode().unwrap();
        assert_eq!(sent, read_task(&path).unwrap(), "the task document is the payload, as written");
        assert_eq!(calls.len(), 5, "polled until done");
        for call in &calls[1..] {
            assert_eq!(action_of(call), ("status", &["default".to_owned(), "t".to_owned()][..]));
        }
    }

    #[test]
    fn start_is_called_on_the_project() {
        let path = task_file(VALID);
        let script = [hello(vec![handle(&["start"], &["ops"])]), answer(1, &started())];
        let (exit, lines, calls) =
            drive(&["--task", path.to_str().unwrap(), "--project", "ops", "--no-wait"], &script);
        assert_eq!(exit, Exit::Done);
        assert_eq!((lines.len(), calls.len()), (1, 1), "--no-wait returns after start");
        assert_eq!(action_of(&calls[0]), ("start", &["ops".to_owned()][..]));
    }

    #[test]
    fn a_stopped_or_failed_run_is_exit_two_with_its_error() {
        let path = task_file(VALID);
        let mut failed = run_status(RunState::Failed, 0, 0, &[]);
        failed.error = Some("worker spawn refused".into());
        let script = [
            hello(vec![handle(&["start", "status"], &[])]),
            answer(1, &started()),
            answer(2, &run_status(RunState::Queued, 0, 0, &[])),
            answer(3, &failed),
        ];
        let (exit, lines, _) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!(exit, Exit::RunEnded(Box::new(failed)));
        assert_eq!(exit.code(), 2);
        assert!(exit.to_string().contains("worker spawn refused"), "{exit}");
        assert!(lines.last().unwrap().contains("worker spawn refused"), "{lines:?}");

        let script = [
            hello(vec![handle(&["start", "status"], &[])]),
            answer(1, &started()),
            answer(2, &run_status(RunState::Stopped, 0, 0, &[])),
        ];
        let (exit, _, _) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!(exit, Exit::RunEnded(Box::new(run_status(RunState::Stopped, 0, 0, &[]))));
    }

    #[test]
    fn a_terminate_request_stops_polling() {
        let path = task_file(VALID);
        let script = [
            hello(vec![handle(&["start", "status"], &[])]),
            answer(1, &started()),
            FromKernel::Terminate,
            answer(2, &run_status(RunState::Running, 0, 0, &[])),
            answer(3, &run_status(RunState::Done, 1, 1, &[])),
        ];
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!((exit.clone(), exit.code()), (Exit::Terminated, 130));
        assert_eq!(calls.len(), 2, "no poll after the request");
        assert_eq!(lines.len(), 1, "the start line only: the reply that carried the request is not reported");

        let script =
            [hello(vec![handle(&["start", "status"], &[])]), FromKernel::Terminate, answer(1, &started())];
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!(exit, Exit::Terminated);
        assert_eq!(
            (calls.len(), lines.len()),
            (1, 0),
            "interrupted start has no confirmed outcome to report"
        );

        let script = [hello(vec![handle(&["resume", "status"], &[])]), FromKernel::Terminate];
        let (exit, lines, calls) = drive(&["--resume", "t"], &script);
        assert_eq!(exit, Exit::Terminated);
        assert_eq!((calls.len(), lines.len()), (1, 0), "interrupted resume is neither reported nor retried");
    }

    #[test]
    fn a_missing_capability_fails_before_any_call() {
        let path = task_file(VALID);
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap()], &[hello(vec![])]);
        assert_eq!(exit, Exit::Failed("this process holds no `discovery start` capability".into()));
        assert!(lines.is_empty() && calls.is_empty());

        let only_start = [hello(vec![handle(&["start"], &[])])];
        let (exit, _, calls) = drive(&["--task", path.to_str().unwrap()], &only_start);
        assert!(
            matches!(&exit, Exit::Failed(why) if why.contains("`discovery status` capability")),
            "{exit:?}"
        );
        assert_eq!(exit.code(), 3);
        assert!(calls.is_empty(), "waiting needs status, so start is not called");

        let elsewhere = [hello(vec![handle(&["start", "status"], &[])])];
        let (exit, _, calls) =
            drive(&["--task", path.to_str().unwrap(), "--discovery-endpoint", "disc"], &elsewhere);
        assert!(matches!(&exit, Exit::Failed(why) if why.contains("`disc start`")), "{exit:?}");
        assert!(calls.is_empty());
    }

    #[test]
    fn a_bad_task_is_refused_before_any_call() {
        let bad = task_file(r#"{"v":2,"name":"t","problems":["p"],"prompt_template":"x","concurrency":1}"#);
        let script = [hello(vec![handle(&["start", "status"], &[])])];
        let (exit, lines, calls) = drive(&["--task", bad.to_str().unwrap()], &script);
        assert!(matches!(&exit, Exit::TaskRefused(why) if why.contains("task version 2")), "{exit:?}");
        assert_eq!(exit.code(), 1);
        assert!(lines.is_empty() && calls.is_empty());
    }

    #[test]
    fn the_services_refusal_of_a_task_is_printed_as_it_came() {
        let path = task_file(VALID);
        let script = [
            hello(vec![handle(&["start", "status"], &[])]),
            refusal(1, ErrorCode::InvalidInput, "task: unknown problem \"putnam_1962_b2\""),
        ];
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap()], &script);
        assert_eq!(
            exit,
            Exit::TaskRefused("start: InvalidInput: task: unknown problem \"putnam_1962_b2\"".into())
        );
        assert!(lines.is_empty() && calls.len() == 1);

        for (code, exit_code) in [
            (ErrorCode::PermissionDenied, 3),
            (ErrorCode::Revoked, 3),
            (ErrorCode::Unavailable, 3),
            (ErrorCode::BudgetExhausted, 3),
        ] {
            let script = [hello(vec![handle(&["start", "status"], &[])]), refusal(1, code, "no")];
            let (exit, _, _) = drive(&["--task", path.to_str().unwrap()], &script);
            assert!(matches!(&exit, Exit::Failed(why) if why.starts_with("start: ")), "{code:?}: {exit:?}");
            assert_eq!(exit.code(), exit_code, "{code:?}");
        }
        let script =
            [hello(vec![handle(&["start", "status"], &[])]), refusal(1, ErrorCode::UnknownOutcome, "lost")];
        let (exit, _, _) = drive(&["--task", path.to_str().unwrap()], &script);
        let Exit::Failed(why) = exit else { panic!("{exit:?}") };
        assert!(why.contains("the run may have started"), "{why}");
    }

    #[test]
    fn a_run_named_differently_by_the_service_is_a_failure() {
        let path = task_file(VALID);
        let other = StartResult { run: "u".into(), ..started() };
        let script = [hello(vec![handle(&["start", "status"], &[])]), answer(1, &other)];
        let (exit, lines, calls) = drive(&["--task", path.to_str().unwrap()], &script);
        assert!(matches!(&exit, Exit::Failed(why) if why.contains("named the run \"u\"")), "{exit:?}");
        assert_eq!((lines.len(), calls.len()), (1, 1), "reported, not polled");
    }

    #[test]
    fn transient_status_failures_are_ridden_out_while_the_service_is_busy() {
        // A status failure that says the service is busy is ridden out for
        // as long as the clock allows, however many there are in a row.
        let mut script = vec![hello(vec![handle(&["start", "status"], &[])]), answer(1, &started())];
        for request in 2..=21 {
            let code = if request % 2 == 0 { ErrorCode::Timeout } else { ErrorCode::QueueFull };
            script.push(refusal(request, code, "busy"));
        }
        script.push(answer(22, &run_status(RunState::Done, 1, 1, &[])));
        let path = task_file(VALID);
        let (exit, _, _) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert_eq!(exit, Exit::Done, "the run outlives its observer's blind spell");

        // Authority errors and a vanished service are never retried.
        for code in [ErrorCode::PermissionDenied, ErrorCode::StaleHandle, ErrorCode::Unavailable] {
            let script = [
                hello(vec![handle(&["start", "status"], &[])]),
                answer(1, &started()),
                refusal(2, code, "gone"),
            ];
            let (exit, _, calls) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
            assert!(matches!(exit, Exit::Failed(_)), "{code:?}: {exit:?}");
            assert_eq!(calls.len(), 2, "{code:?} is not retried");
        }
    }

    #[test]
    fn a_broken_channel_is_exit_three() {
        let path = task_file(VALID);
        let script = [hello(vec![handle(&["start", "status"], &[])]), answer(1, &started())];
        let (exit, _, _) = drive(&["--task", path.to_str().unwrap(), "--poll-ms", "1"], &script);
        assert!(matches!(&exit, Exit::Failed(why) if why.starts_with("status: ")), "{exit:?}");
        assert_eq!(exit.code(), 3);
        let script = [hello(vec![handle(&["start", "status"], &[])]), answer(1, &json!({"nope": 1}))];
        let (exit, _, _) = drive(&["--task", path.to_str().unwrap()], &script);
        assert!(matches!(&exit, Exit::Failed(why) if why.starts_with("start: malformed reply")), "{exit:?}");
    }
}
