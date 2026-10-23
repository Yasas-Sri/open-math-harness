//! Harness for end-to-end tests: the real programs (`nucleus-fs`, the fixture
//! agent and the services), started by the real supervisor under the real
//! confinement backend. Nothing here mocks authorization or isolation.
//!
//! Synchronization is always by polling an observable (a transcript line, an
//! access-log line, kernel liveness, `/proc`) with a timeout, never by sleeping
//! for a fixed time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nucleus_kernel::{CapSpec, Config, SecurityEvent};
use nucleus_platform::{Bind, ProgramSpec, StartError, Supervisor, SupervisorConfig};
use nucleus_protocol::{ActionName, EndpointName, ProcessId, ProgramName, ResourcePath};
use serde_json::Value;

pub mod http;

/// Default time any single wait may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// `O_NONBLOCK`; lets the test open a FIFO for writing without hanging when
/// no reader is there yet.
const O_NONBLOCK: i32 = 0o4000;

/// Locations of the programs under test.
pub struct Binaries {
    pub fs: PathBuf,
    pub agent: PathBuf,
    pub openrouter: PathBuf,
    pub conversation: PathBuf,
    pub chat: PathBuf,
    pub memory: PathBuf,
    pub ui: PathBuf,
    pub approvals: PathBuf,
    pub lean: PathBuf,
    pub prover: PathBuf,
    pub discovery: PathBuf,
    pub discovery_client: PathBuf,
    pub shell: PathBuf,
    pub suggestions: PathBuf,
    pub diffs: PathBuf,
    pub projects: PathBuf,
    pub project: PathBuf,
    pub pdf: PathBuf,
    pub publish: PathBuf,
    pub skills: PathBuf,
}

/// Builds the service and agent binaries once per test process, beside the
/// running test binary, and returns where they are. Cargo does not build other
/// packages' binaries for an integration test, so the harness asks for them
/// explicitly; a test in another workspace has its own build directory.
pub fn binaries() -> &'static Binaries {
    static BINARIES: OnceLock<Binaries> = OnceLock::new();
    BINARIES.get_or_init(|| {
        // .../target/debug/deps/<test binary> -> .../target/debug
        let exe = std::env::current_exe().expect("current exe");
        let profile_dir = exe.ancestors().nth(2).expect("target profile directory").to_owned();
        {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
            let workspace =
                Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("workspace root").to_owned();
            let status = Command::new(cargo)
                .args([
                    "build",
                    "-p",
                    "nucleus-cli",
                    "-p",
                    "nucleus-fs",
                    "-p",
                    "nucleus-fixture-agent",
                    "-p",
                    "nucleus-openrouter",
                    "-p",
                    "nucleus-conversation",
                    "-p",
                    "nucleus-chat-agent",
                    "-p",
                    "nucleus-mcp",
                    "-p",
                    "nucleus-memory",
                    "-p",
                    "nucleus-ui",
                    "-p",
                    "nucleus-approvals",
                    "-p",
                    "nucleus-plugin-development",
                    "-p",
                    "nucleus-lean",
                    "-p",
                    "nucleus-prover",
                    "-p",
                    "nucleus-discovery",
                    "-p",
                    "nucleus-discovery-client",
                    "-p",
                    "nucleus-shell",
                    "-p",
                    "nucleus-suggestions",
                    "-p",
                    "nucleus-diffs",
                    "-p",
                    "nucleus-projects",
                    "-p",
                    "nucleus-project",
                    "-p",
                    "nucleus-pdf",
                    "-p",
                    "nucleus-publish",
                    "-p",
                    "nucleus-skills",
                    "--bins",
                ])
                // The programs are built as the test is: a release test runs release programs.
                .args(cfg!(not(debug_assertions)).then_some("--release"))
                .arg("--target-dir")
                .arg(profile_dir.parent().expect("target directory"))
                .current_dir(&workspace)
                .status()
                .expect("run cargo build");
            assert!(status.success(), "building the programs under test failed");
        }
        let found = Binaries {
            fs: profile_dir.join("nucleus-fs"),
            agent: profile_dir.join("nucleus-fixture-agent"),
            openrouter: profile_dir.join("nucleus-openrouter"),
            conversation: profile_dir.join("nucleus-conversation"),
            chat: profile_dir.join("nucleus-chat-agent"),
            memory: profile_dir.join("nucleus-memory"),
            ui: profile_dir.join("nucleus-ui"),
            approvals: profile_dir.join("nucleus-approvals"),
            lean: profile_dir.join("nucleus-lean"),
            prover: profile_dir.join("nucleus-prover"),
            discovery: profile_dir.join("nucleus-discovery"),
            discovery_client: profile_dir.join("nucleus-discovery-client"),
            shell: profile_dir.join("nucleus-shell"),
            suggestions: profile_dir.join("nucleus-suggestions"),
            diffs: profile_dir.join("nucleus-diffs"),
            projects: profile_dir.join("nucleus-projects"),
            project: profile_dir.join("nucleus-project"),
            pdf: profile_dir.join("nucleus-pdf"),
            publish: profile_dir.join("nucleus-publish"),
            skills: profile_dir.join("nucleus-skills"),
        };
        assert!(found.openrouter.is_file(), "missing {}", found.openrouter.display());
        assert!(found.fs.is_file(), "missing {}", found.fs.display());
        assert!(found.agent.is_file(), "missing {}", found.agent.display());
        found
    })
}

/// Where the sessions service sees the agent it starts for each session.
pub const SESSION_AGENT: &str = "/session-agent";
/// The project a window's session is in when it names none.
pub const DEFAULT_PROJECT: &str = "default";

/// The sessions service's authority to start its agents.
pub fn session_spawn() -> CapSpec {
    let mut spawn = cap("kernel", &["spawn_executable"], &["executable", "session-agent"]);
    spawn.delegable = false;
    spawn
}

pub fn program(name: &str) -> ProgramName {
    ProgramName::new(name).unwrap()
}

pub fn endpoint(name: &str) -> EndpointName {
    EndpointName::new(name).unwrap()
}

pub fn path(segments: &[&str]) -> ResourcePath {
    ResourcePath::new(segments.iter().copied()).unwrap()
}

fn actions(list: &[&str]) -> BTreeSet<ActionName> {
    list.iter().map(|a| ActionName::new(*a).unwrap()).collect()
}

/// A delegable root capability.
pub fn cap(endpoint_name: &str, acts: &[&str], resource: &[&str]) -> CapSpec {
    CapSpec {
        endpoint: endpoint(endpoint_name),
        actions: actions(acts),
        resource: path(resource),
        ttl_ms: None,
        delegable: true,
        max_calls: None,
    }
}

/// Root grants for a service that may provide `endpoint_name`.
pub fn provider_grants(endpoint_name: &str) -> Vec<CapSpec> {
    vec![cap("kernel", &["provide"], &["endpoint", endpoint_name])]
}

/// Root grants for an agent: fs actions on a path, plus spawning `worker`.
pub fn agent_grants(fs_endpoint: &str, acts: &[&str], resource: &[&str]) -> Vec<CapSpec> {
    vec![cap(fs_endpoint, acts, resource), cap("kernel", &["spawn"], &["program", "worker"])]
}

/// Root grants for the resident discovery service (`nucleus-discovery`): the
/// compositions' list with a budgeted model. The service itself uses `provide`
/// for its `discovery` and `explore` endpoints (non-delegable) and
/// `create_endpoint` (delegable, as the kernel requires of creation authority,
/// though it delegates none of it). The rest it narrows to each run's process,
/// which narrows them again to the run's workers: `spawn_executable` over guest
/// `/programs`, where the built binaries are bound (see
/// [`Scenario::add_discovery`]); `model complete` with `model_calls` as the
/// hard cap (the kernel charges every worker's calls to this one capability, so
/// the budget is shared, never multiplied; a test that streams adds `start` to
/// it and an unbudgeted `{next, cancel}` beside it); `lean {statement, check,
/// verify, analyze, complete}` on every problem (the last two are the analysis
/// calls of task-scoring protocol v3; workers receive `check` or `verify` on
/// their run's registration only); `lean register`, which a run's process holds
/// on `[key]` (the run's key) and cannot delegate; and `explore {assignment, submit,
/// publish_plan, read_plans}` on the root path, because `provide` grants no
/// handle on the provided endpoint and a delegation must derive from a handle
/// the delegator holds. A test whose workers get a shell adds `shell exec`.
pub fn discovery_grants(model_calls: u64) -> Vec<CapSpec> {
    let mut own = vec![
        cap("kernel", &["provide"], &["endpoint", "discovery"]),
        cap("kernel", &["provide"], &["endpoint", "explore"]),
    ];
    for grant in &mut own {
        grant.delegable = false;
    }
    let mut model = cap("model", &["complete"], &["deepseek"]);
    model.max_calls = Some(model_calls);
    own.extend([
        cap("kernel", &["create_endpoint"], &["endpoint"]),
        cap("kernel", &["spawn_executable"], &["executable", "programs"]),
        model,
        cap("lean", &["statement", "check", "verify", "analyze", "complete"], &[]),
        cap("lean", &["register"], &[]),
        cap("explore", &["assignment", "submit", "publish_plan", "read_plans"], &[]),
    ]);
    own
}

/// The actions of the `discovery` interface, as a client of the service
/// (a fixture agent playing the operator or the chat agent) receives them:
/// one non-delegable capability on the root path, which is all the service's
/// interface needs (`status`, `stop`, `problems` and `tree` name the run —
/// and the problem — in the resource path).
pub fn discovery_caller_grants() -> Vec<CapSpec> {
    let mut grant = cap("discovery", &["start", "status", "stop", "list", "problems", "tree"], &[]);
    grant.delegable = false;
    vec![grant]
}

/// A fixture-agent tool call on the `discovery` endpoint through its generic
/// `call` verb, about the runs of the default project: `start` and `list`
/// on `[project]` (`run` is `None`), `status`, `stop` and `problems` on
/// `[project, run]`. `payload` is the task document for `start` and `{}`
/// otherwise. The deadline covers a `start`, which resolves every problem
/// through the Lean service before answering.
pub fn discovery_call(action: &str, run: Option<&str>, payload: Value) -> Value {
    let path: Vec<&str> = run.into_iter().collect();
    discovery_call_on(action, &path, payload)
}

/// [`discovery_call`] on an explicit path in the default project: `[run,
/// problem]` for `tree`.
pub fn discovery_call_on(action: &str, path: &[&str], payload: Value) -> Value {
    let path: Vec<&str> = std::iter::once(DEFAULT_PROJECT).chain(path.iter().copied()).collect();
    serde_json::json!({"tool": "call", "endpoint": "discovery", "action": action, "path": path,
                       "payload": payload, "deadline_ms": 240_000})
}

/// The prover's default system-prompt template directory in this repository
/// (`program-files/prover/prompts`), whose templates the discovery tests copy
/// into a task's prompts directory.
pub fn prompts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../program-files/prover/prompts")
        .canonicalize()
        .expect("the prover's prompts directory exists")
}

/// How [`Scenario::add_discovery`] configures the resident discovery service:
/// its deployment facts, never a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryService {
    /// Host directory bound read-only at guest `/tasks`: the curricula and
    /// prompt templates a task's `problems_file` / `prompt_template_file` name.
    pub tasks: PathBuf,
    /// Host directory bound read-only at guest `/policies`: the exploration-
    /// policy programs a task's `policy` names. A run's process runs a `.py`
    /// under `/usr/bin/python3`, which the backend's read-only `/usr`
    /// provides, and anything else directly, as its own host child inside
    /// its sandbox.
    pub policies: PathBuf,
    /// Arguments every worker gets (one `--prover-arg` each), before the
    /// service's own `--out`, `--lean-endpoint` and `--explore-endpoint`.
    pub prover_args: Vec<String>,
    /// Further service options: `--max-concurrency`,
    /// `--max-usd-per-problem`, … A scenario that starts runs needs kernel
    /// limits that fit these options (as `check_run_limits` in
    /// `compositions.rs` checks them); the kernel's defaults hold one run of
    /// at most 4 workers with `--max-queued-runs` below 4.
    pub extra: Vec<String>,
}

/// One test's private world on the host filesystem plus its program table.
pub struct Scenario {
    dir: tempfile::TempDir,
    programs: BTreeMap<ProgramName, ProgramSpec>,
    child_outputs: BTreeMap<String, PathBuf>,
    pub events: Arc<Mutex<Vec<SecurityEvent>>>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

impl Scenario {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("fixture")).unwrap();
        Self {
            dir,
            programs: BTreeMap::new(),
            child_outputs: BTreeMap::new(),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Host path of `relative` inside the scenario directory.
    pub fn host(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// Writes a host file, creating parent directories.
    pub fn write(&self, relative: &str, content: &str) {
        let target = self.host(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }

    /// Creates a named pipe. A service that opens it blocks until the test writes.
    pub fn mkfifo(&self, relative: &str) {
        let target = self.host(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let status = Command::new("mkfifo").arg(&target).status().unwrap();
        assert!(status.success(), "mkfifo {}", target.display());
    }

    /// Registers an fs service program rooted at host directory `data`.
    pub fn add_fs(&mut self, name: &str, data: &str, endpoint_name: &str) {
        let log = format!("{data}-log");
        std::fs::create_dir_all(self.host(data)).unwrap();
        std::fs::create_dir_all(self.host(&log)).unwrap();
        let mut args: Vec<String> = ["--root", "/data", "--access-log", "/log/access.jsonl"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        if endpoint_name != "fs" {
            args.extend(["--endpoint".to_owned(), endpoint_name.to_owned()]);
        }
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().fs.clone(),
                args,
                binds: vec![
                    Bind { host: self.host(data), guest: "/data".into(), writable: true },
                    Bind { host: self.host(&log), guest: "/log".into(), writable: true },
                ],
                network: false,
            },
        );
    }

    /// The Lean toolchain's guest paths for a sandbox that binds `pb_workspace`
    /// at `/pb` and `~/.elan` at `/elan`: `(lean executable, LEAN_PATH)`,
    /// computed on the host with `lake` and rewritten onto those mounts.
    fn lean_guest_paths(pb_workspace: &Path) -> (String, String) {
        let elan = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".elan");
        let lake_env = |var: &str| -> String {
            let out = Command::new("lake")
                .args(["env", "printenv", var])
                .current_dir(pb_workspace)
                .output()
                .expect("run lake env");
            assert!(out.status.success(), "lake env printenv {var} failed");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let remap = |host_path: &str| -> String {
            host_path
                .replace(&pb_workspace.to_string_lossy().into_owned(), "/pb")
                .replace(&elan.to_string_lossy().into_owned(), "/elan")
        };
        let lean_path = lake_env("LEAN_PATH").split(':').map(remap).collect::<Vec<_>>().join(":");
        let lean_host = {
            let out = Command::new("lake")
                .args(["env", "which", "lean"])
                .current_dir(pb_workspace)
                .output()
                .unwrap();
            let p = String::from_utf8(out.stdout).unwrap().trim().to_owned();
            std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)).to_string_lossy().into_owned()
        };
        (remap(&lean_host), lean_path)
    }

    /// Registers the shell service as program `name`: its workspaces under host
    /// `<name>-work/` bound writable at `/work`, the PutnamBench workspace
    /// read-only at `/pb` and the elan toolchains at `/elan`, so a command can
    /// run the toolchain `lean` with `LEAN_PATH` set (the service's `--env`).
    /// No network.
    pub fn add_shell(&mut self, name: &str, pb_workspace: &Path) {
        let work = format!("{name}-work");
        std::fs::create_dir_all(self.host(&work)).unwrap();
        let elan = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".elan");
        let (lean_guest, lean_path) = Self::lean_guest_paths(pb_workspace);
        let bin = Path::new(&lean_guest)
            .parent()
            .expect("lean lives in a bin directory")
            .to_string_lossy()
            .into_owned();
        let args: Vec<String> = [
            "--root",
            "/work",
            "--env",
            &format!("PATH={bin}:/usr/bin:/bin"),
            "--env",
            &format!("LEAN_PATH={lean_path}"),
            "--env",
            "HOME=/tmp",
            "--default-timeout-ms",
            "600000",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().shell.clone(),
                args,
                binds: vec![
                    Bind { host: self.host(&work), guest: "/work".into(), writable: true },
                    Bind { host: pb_workspace.to_path_buf(), guest: "/pb".into(), writable: false },
                    Bind { host: elan, guest: "/elan".into(), writable: false },
                ],
                network: false,
            },
        );
    }

    /// Registers the PDF text service as program `name`, reading host
    /// directory `data` through a read-only bind at `/data`. No network.
    pub fn add_pdf(&mut self, name: &str, data: &str) {
        std::fs::create_dir_all(self.host(data)).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().pdf.clone(),
                args: ["--root", "/data"].iter().map(|s| (*s).to_owned()).collect(),
                binds: vec![Bind { host: self.host(data), guest: "/data".into(), writable: false }],
                network: false,
            },
        );
    }

    /// Registers a skills service as program `name` whose root is the host
    /// directory `data`, a repository with its `.skill/` (`nucleus-skills`).
    /// No network and no catalog: it has only the repository's skills.
    pub fn add_skills(&mut self, name: &str, data: &str) {
        std::fs::create_dir_all(self.host(data)).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().skills.clone(),
                args: ["--root", "/data"].iter().map(|s| (*s).to_owned()).collect(),
                binds: vec![Bind { host: self.host(data), guest: "/data".into(), writable: true }],
                network: false,
            },
        );
    }

    /// Registers a shell service as program `name` over the same host directory
    /// an fs service was given as `data`, so that one resource names one
    /// directory for both, as a project's hosts do. Its commands write only
    /// their own workspace. No network, no toolchain.
    pub fn add_shell_over(&mut self, name: &str, data: &str) {
        std::fs::create_dir_all(self.host(data)).unwrap();
        let args = ["--root", "/data", "--env", "PATH=/usr/bin:/bin", "--env", "HOME=/tmp"];
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().shell.clone(),
                args: args.iter().map(|s| (*s).to_owned()).collect(),
                binds: vec![Bind { host: self.host(data), guest: "/data".into(), writable: true }],
                network: false,
            },
        );
    }

    /// Binds host directory `relative` of the scenario at `guest` in the
    /// sandbox of the program registered as `role`.
    pub fn bind(&mut self, role: &str, relative: &str, guest: &str, writable: bool) {
        std::fs::create_dir_all(self.host(relative)).unwrap();
        let host = self.host(relative);
        let spec = self.programs.get_mut(&program(role)).expect("the role is registered first");
        spec.binds.push(Bind { host, guest: guest.into(), writable });
    }

    /// Registers the swarm service as program `name`: the chat program with
    /// `--swarm-service`, its members the same program bound at
    /// `/programs/nucleus-chat-agent`, running `model`, its state in
    /// `<name>-state/` ([`Self::swarm_state`]). Its grants are the caller's.
    pub fn add_swarm(&mut self, name: &str, model: &str, member_args: &[&str]) {
        let state = self.swarm_state(name);
        std::fs::create_dir_all(&state).unwrap();
        let mut args: Vec<String> = [
            "--swarm-service",
            "--state",
            "/state",
            "--member-executable",
            "/programs/nucleus-chat-agent",
            "--model",
            model,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for arg in member_args {
            args.extend(["--member-arg".to_owned(), (*arg).to_owned()]);
        }
        let chat = binaries().chat.clone();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: chat.clone(),
                args,
                binds: vec![
                    Bind { host: chat, guest: "/programs/nucleus-chat-agent".into(), writable: false },
                    Bind { host: state, guest: "/state".into(), writable: true },
                ],
                network: false,
            },
        );
    }

    /// Where the swarm service registered as `name` keeps its state.
    pub fn swarm_state(&self, name: &str) -> PathBuf {
        self.host(&format!("{name}-state"))
    }

    /// Appends `args` to what the program registered as `name` starts with.
    pub fn add_args(&mut self, name: &str, args: &[&str]) {
        let spec = self.programs.get_mut(&program(name)).expect("the program is registered");
        spec.args.extend(args.iter().map(|s| (*s).to_owned()));
    }

    /// Registers the git diff service as program `name`, reading host
    /// directory `work` through a read-only bind at `/work` and showing its
    /// repository `repository`. No network.
    pub fn add_diffs(&mut self, name: &str, work: &str, repository: &str) {
        std::fs::create_dir_all(self.host(work)).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().diffs.clone(),
                args: ["--root", "/work", "--repository", repository]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                binds: vec![Bind { host: self.host(work), guest: "/work".into(), writable: false }],
                network: false,
            },
        );
    }

    /// Registers the projects service as program `name`: the host directory
    /// `projects` writable at `/projects`, the projects' shared home `home` at
    /// `/home`, the project host executable, and `extra` read-only binds (host,
    /// guest), with projects' processes (`--identities on`).
    pub fn add_projects(&mut self, name: &str, projects: &str, home: &str, extra: &[(&Path, &str)]) {
        self.add_projects_service(name, projects, home, extra, true);
    }

    /// The projects service of a composition without projects' processes:
    /// it is booted without `spawn_as`, and each project's host is its own
    /// sandboxed child, which sees the folder and the shared home where it
    /// does.
    pub fn add_projects_without_identities(&mut self, name: &str, projects: &str, home: &str) {
        self.add_projects_service(name, projects, home, &[], false);
    }

    fn add_projects_service(
        &mut self,
        name: &str,
        projects: &str,
        home: &str,
        extra: &[(&Path, &str)],
        identities: bool,
    ) {
        std::fs::create_dir_all(self.host(projects)).unwrap();
        std::fs::create_dir_all(self.host(home)).unwrap();
        let mut binds = vec![
            Bind { host: self.host(projects), guest: "/projects".into(), writable: true },
            Bind { host: self.host(home), guest: "/home".into(), writable: true },
            Bind {
                host: binaries().project.clone(),
                guest: "/programs/nucleus-project".into(),
                writable: false,
            },
        ];
        binds.extend(extra.iter().map(|(host, guest)| Bind {
            host: host.to_path_buf(),
            guest: (*guest).into(),
            writable: false,
        }));
        let mut args = vec!["--root", "/projects", "--home", "/home", "--programs", "/programs"];
        if identities {
            args.extend(["--identities", "on"]);
        }
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().projects.clone(),
                args: args.iter().map(|s| (*s).to_owned()).collect(),
                binds,
                // Its hosts clone, and can only inherit the network from it.
                network: true,
            },
        );
    }

    /// Registers the Lean verifier as program `name`: the PutnamBench `lean4`
    /// workspace `pb_workspace` (a host path, e.g. `$PB_LEAN`) bound **read-only**
    /// at `/pb`, the scenario's `problems` directory of frozen statements bound
    /// read-only at `/problems`, the elan toolchain directory bound read-only at
    /// `/elan`, and a scratch directory writable at `/scratch`. No network.
    ///
    /// The service runs the toolchain `lean` directly with `LEAN_PATH` (direct
    /// mode) rather than `lake env lean`: lake re-resolves the dependency
    /// manifest through git on every run, which fails inside the sandbox and
    /// makes it try to re-clone Mathlib onto the read-only workspace. The
    /// search path and the toolchain binary are computed here on the host with
    /// `lake` and rewritten onto the sandbox's mount points.
    pub fn add_lean(&mut self, name: &str, pb_workspace: &Path, problems: &str) {
        let scratch = format!("{name}-scratch");
        std::fs::create_dir_all(self.host(problems)).unwrap();
        std::fs::create_dir_all(self.host(&scratch)).unwrap();
        let elan = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".elan");
        let (lean_guest, lean_path) = Self::lean_guest_paths(pb_workspace);
        let mut args: Vec<String> = [
            "--workspace",
            "/pb",
            "--problems",
            "/problems",
            "--scratch",
            "/scratch",
            "--timeout-secs",
            "240",
            "--lean",
            &lean_guest,
            "--lean-path",
            &lean_path,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let mut binds = vec![
            Bind { host: pb_workspace.to_path_buf(), guest: "/pb".into(), writable: false },
            Bind { host: self.host(problems), guest: "/problems".into(), writable: false },
            Bind { host: self.host(&scratch), guest: "/scratch".into(), writable: true },
            Bind { host: elan, guest: "/elan".into(), writable: false },
        ];
        // The resident Lean REPL (for `check`), when the host has one: bound
        // read-only at /repl. Without it the service still serves statement/verify.
        if let Some(repl) = std::env::var_os("NUCLEUS_REPL").map(PathBuf::from).filter(|p| p.is_file()) {
            binds.push(Bind { host: repl, guest: "/repl".into(), writable: false });
            args.extend(["--repl", "/repl"].iter().map(|s| (*s).to_owned()));
        }
        self.programs.insert(
            program(name),
            ProgramSpec { executable: binaries().lean.clone(), args, binds, network: false },
        );
    }

    /// Guest path of the prover executable inside a process that has
    /// [`Self::bind_programs`]'s bind: what the discovery service starts its
    /// workers from.
    pub fn prover_executable_guest() -> String {
        format!("/programs/{}", binaries().prover.file_name().expect("executable name").to_string_lossy())
    }

    /// Registers the resident discovery service as program `role`. Its
    /// namespace: the directory of built programs read-only at `/programs`
    /// (with `spawn_executable` it starts each run's process from its own
    /// executable there, `--run-executable`, and that process starts each
    /// worker from the prover executable beside it), `service.tasks` read-only
    /// at `/tasks`, `service.policies` read-only at `/policies`, and host
    /// `out/<role>/` writable at `/out`, where every run gets
    /// `<project>/<run>/` (`task.json`, `discovery.jsonl`,
    /// `<problem>/{nodes,rounds}.jsonl`) and, beside it,
    /// `<run>.workers/<problem>/<task>/` for the workers' own files. No
    /// network. What runs is decided by `discovery start`, never here.
    pub fn add_discovery(&mut self, role: &str, service: &DiscoveryService) {
        std::fs::create_dir_all(self.out_dir(role)).unwrap();
        std::fs::create_dir_all(&service.tasks).unwrap();
        std::fs::create_dir_all(&service.policies).unwrap();
        // Each run's process is this same program, beside the prover in the
        // bound profile directory.
        let itself = binaries().discovery.file_name().expect("executable name");
        let run_executable =
            Path::new(&Self::prover_executable_guest()).with_file_name(itself).to_string_lossy().into_owned();
        let mut args = vec![
            "--out".to_owned(),
            "/out".to_owned(),
            "--prover-executable".to_owned(),
            Self::prover_executable_guest(),
            "--run-executable".to_owned(),
            run_executable,
        ];
        for arg in &service.prover_args {
            args.extend(["--prover-arg".to_owned(), arg.clone()]);
        }
        args.extend(service.extra.iter().cloned());
        self.programs.insert(
            program(role),
            ProgramSpec {
                executable: binaries().discovery.clone(),
                args,
                binds: vec![Bind { host: self.out_dir(role), guest: "/out".into(), writable: true }],
                network: false,
            },
        );
        self.bind_programs(role);
        let spec = self.programs.get_mut(&program(role)).expect("just registered");
        spec.binds.push(Bind { host: service.tasks.clone(), guest: "/tasks".into(), writable: false });
        spec.binds.push(Bind { host: service.policies.clone(), guest: "/policies".into(), writable: false });
    }

    /// Registers the headless operator client (`nucleus-discovery-client`) as
    /// program `role`, starting `task` (a task document, written to host
    /// `<role>-task/run.json` and bound read-only at `/task`) and polling the
    /// run every `poll_ms` until it ends. `extra` are further client options.
    pub fn add_discovery_client(&mut self, role: &str, task: &Value, poll_ms: u64, extra: &[&str]) {
        let dir = format!("{role}-task");
        self.write(&format!("{dir}/run.json"), &format!("{task:#}\n"));
        let mut args = vec![
            "--task".to_owned(),
            "/task/run.json".to_owned(),
            "--poll-ms".to_owned(),
            poll_ms.to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        self.programs.insert(
            program(role),
            ProgramSpec {
                executable: binaries().discovery_client.clone(),
                args,
                binds: vec![Bind { host: self.host(&dir), guest: "/task".into(), writable: false }],
                network: false,
            },
        );
    }

    /// The journal directory of run `run` of the default project under
    /// discovery service `role`: host `out/<role>/default/<run>/`.
    pub fn run_dir(&self, role: &str, run: &str) -> PathBuf {
        self.out_dir(role).join(DEFAULT_PROJECT).join(run)
    }

    /// The workers' directory of run `run`, beside its journal directory:
    /// host `out/<role>/default/<run>.workers/`.
    pub fn workers_dir(&self, role: &str, run: &str) -> PathBuf {
        self.out_dir(role).join(DEFAULT_PROJECT).join(format!("{run}.workers"))
    }

    /// Registers an agent program for `role` replaying `fixture`.
    pub fn add_agent(&mut self, role: &str, fixture: &Value, fs_endpoint: Option<&str>) {
        self.write(&format!("fixture/{role}.json"), &fixture.to_string());
        std::fs::create_dir_all(self.out_dir(role)).unwrap();
        let mut args = vec![
            "--fixture".to_owned(),
            format!("/fixture/{role}.json"),
            "--transcript".to_owned(),
            "/out/transcript.jsonl".to_owned(),
            "--signals".to_owned(),
            "/out".to_owned(),
        ];
        if let Some(name) = fs_endpoint {
            args.extend(["--fs-endpoint".to_owned(), name.to_owned()]);
        }
        self.programs.insert(
            program(role),
            ProgramSpec {
                executable: binaries().agent.clone(),
                args,
                binds: vec![
                    Bind { host: self.host("fixture"), guest: "/fixture".into(), writable: false },
                    Bind { host: self.out_dir(role), guest: "/out".into(), writable: true },
                ],
                network: false,
            },
        );
    }

    /// Guest path the fixture-agent executable has once [`Self::bind_programs`]
    /// has run: known in advance so a fixture can name it.
    pub fn fixture_executable_guest() -> String {
        format!("/programs/{}", binaries().agent.file_name().expect("executable name").to_string_lossy())
    }

    /// Makes the directory of built programs readable to `role` at guest
    /// `/programs`, so `role` can start one of them with `spawn_executable`.
    /// Returns the guest path of the fixture-agent executable. Guest `/programs`
    /// is used rather than `/bin`, which the sandbox reserves.
    pub fn bind_programs(&mut self, role: &str) -> String {
        let exe = binaries().agent.clone();
        let dir = exe.parent().expect("built programs live in a directory").to_path_buf();
        let spec = self.programs.get_mut(&program(role)).expect("role is registered before bind_programs");
        spec.binds.push(Bind { host: dir, guest: "/programs".into(), writable: false });
        format!("/programs/{}", exe.file_name().expect("executable name").to_string_lossy())
    }

    /// Registers the OpenRouter adapter as program `name`, with `key` in a
    /// read-only key file.
    /// `network` is the operator's sandbox grant; the adapter needs it.
    pub fn add_openrouter(&mut self, name: &str, base_url: &str, key: &str, extra: &[&str], network: bool) {
        let secrets = format!("{name}-secrets");
        self.write(&format!("{secrets}/key"), &format!("{key}\n"));
        let mut args: Vec<String> =
            ["--key-file", "/secrets/key", "--base-url", base_url].iter().map(|s| (*s).to_owned()).collect();
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().openrouter.clone(),
                args,
                binds: vec![Bind { host: self.host(&secrets), guest: "/secrets".into(), writable: false }],
                network,
            },
        );
    }

    /// Registers the conversation service as program `name`, with its sessions
    /// under host directory `sessions/`.
    pub fn add_conversation(&mut self, name: &str) {
        std::fs::create_dir_all(self.host("sessions")).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().conversation.clone(),
                args: vec!["--root".to_owned(), "/sessions".to_owned()],
                binds: vec![Bind { host: self.host("sessions"), guest: "/sessions".into(), writable: true }],
                network: false,
            },
        );
    }

    /// Registers the publish service as program `name`, as the VM composes
    /// it: no binds, no state of its own, no network.
    pub fn add_publish(&mut self, name: &str) {
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().publish.clone(),
                args: Vec::new(),
                binds: Vec::new(),
                network: false,
            },
        );
    }

    /// Registers the suggestions service as program `name`: no binds, no network.
    pub fn add_suggestions(&mut self, name: &str, model: &str, extra: &[&str]) {
        let mut args = vec!["--model".to_owned(), model.to_owned()];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().suggestions.clone(),
                args,
                binds: Vec::new(),
                network: false,
            },
        );
    }

    /// Registers the chat agent as program `name`. It gets no binds at all.
    pub fn add_chat_agent(&mut self, name: &str, model: &str, extra: &[&str]) {
        let mut args = vec!["--model".to_owned(), model.to_owned()];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        self.programs.insert(
            program(name),
            ProgramSpec { executable: binaries().chat.clone(), args, binds: Vec::new(), network: false },
        );
    }

    /// Registers the memory service as program `name`, over the same host
    /// directory an fs service was given as `data`: its workspace is
    /// `<data>/memories`, which is the `fs` resource `["memories"]`, and the
    /// service checks that those are one directory before it consolidates.
    /// Only the consolidator executable and the service's private workspace
    /// and database directory are bound into the memory service.
    pub fn add_memory(&mut self, name: &str, data: &str, model: &str, extra: &[&str]) {
        std::fs::create_dir_all(self.host(data)).unwrap();
        let chat = binaries().chat.clone();
        let agent = format!("/programs/{}", chat.file_name().expect("executable name").to_string_lossy());
        let mut args: Vec<String> =
            ["--root", "/data/memories", "--model", model, "--agent-executable", &agent]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        let workspace = self.host(&format!("{data}/memories"));
        let state = self.host("memory-state");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        args.extend(["--db".to_owned(), "/state".to_owned()]);
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().memory.clone(),
                args,
                binds: vec![
                    Bind { host: workspace, guest: "/data/memories".into(), writable: true },
                    Bind { host: state, guest: "/state".into(), writable: true },
                    Bind { host: chat, guest: agent.into(), writable: false },
                ],
                network: false,
            },
        );
    }

    /// Registers the sessions service, serving the sessions endpoint: it starts
    /// the chat agent registered as `agent` for each session, with that
    /// program's arguments, from its executable bound at [`SESSION_AGENT`]. Its
    /// grants need `kernel spawn_executable` on it.
    pub fn add_sessions(&mut self, name: &str, agent: &str) {
        let child = self.programs.get(&program(agent)).expect("child registered first").clone();
        let mut args = vec!["--child-executable".to_owned(), SESSION_AGENT.to_owned()];
        for arg in &child.args {
            args.extend(["--child-arg".to_owned(), arg.clone()]);
        }
        // What the agent program sees, each agent sees: from the service's
        // own namespace, which holds the same binds.
        for bind in &child.binds {
            let flag = if bind.writable { "--child-bind-writable" } else { "--child-bind" };
            args.extend([flag.to_owned(), bind.guest.to_string_lossy().into_owned()]);
        }
        let mut binds = child.binds.clone();
        binds.push(Bind { host: child.executable.clone(), guest: SESSION_AGENT.into(), writable: false });
        self.programs.insert(
            program(name),
            ProgramSpec { executable: binaries().chat.clone(), args, binds, network: child.network },
        );
    }

    /// Registers the approvals guard as program `name` with the given policy. Its
    /// policy is bound read-only; its log goes to `<name>-log/approvals.jsonl`.
    pub fn add_approvals(&mut self, name: &str, policy: &Value) {
        let (policy_dir, log_dir) = (format!("{name}-policy"), format!("{name}-log"));
        self.write(&format!("{policy_dir}/policy.json"), &policy.to_string());
        std::fs::create_dir_all(self.host(&log_dir)).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().approvals.clone(),
                args: ["--policy", "/policy/policy.json", "--log", "/log/approvals.jsonl"]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                binds: vec![
                    Bind { host: self.host(&policy_dir), guest: "/policy".into(), writable: false },
                    Bind { host: self.host(&log_dir), guest: "/log".into(), writable: true },
                ],
                network: false,
            },
        );
    }

    /// Events recorded by the approvals guard, in order.
    pub fn approvals_log(&self, name: &str) -> Vec<Value> {
        let text =
            std::fs::read_to_string(self.host(&format!("{name}-log/approvals.jsonl"))).unwrap_or_default();
        text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// Registers the UI service as program `name`, listening in host directory `<name>-socket/`.
    pub fn add_ui(&mut self, name: &str) {
        let dir = format!("{name}-socket");
        std::fs::create_dir_all(self.host(&dir)).unwrap();
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().ui.clone(),
                args: vec!["--socket".to_owned(), "/ui/ui.sock".to_owned()],
                binds: vec![Bind { host: self.host(&dir), guest: "/ui".into(), writable: true }],
                network: false,
            },
        );
    }

    /// A UI service whose every connection's first line is its label, as each
    /// stream the platform opens begins with one ([`Self::connect_ui_as`]).
    pub fn add_labelled_ui(&mut self, name: &str) {
        self.add_ui(name);
        if let Some(spec) = self.programs.get_mut(&program(name)) {
            spec.args.push("--labelled".to_owned());
        }
    }

    /// The UI service as the VM runs it: it dials the platform's link with the
    /// configuration `config`, bound read-only at `/link`, with the network it
    /// needs for that.
    pub fn add_linked_ui(&mut self, name: &str, config: &Value) {
        let dir = format!("{name}-link");
        self.write(&format!("{dir}/platform.json"), &config.to_string());
        self.programs.insert(
            program(name),
            ProgramSpec {
                executable: binaries().ui.clone(),
                args: vec!["--link".to_owned(), "/link/platform.json".to_owned()],
                binds: vec![Bind { host: self.host(&dir), guest: "/link".into(), writable: false }],
                network: true,
            },
        );
    }

    /// The trusted GitHub broker uses the same link configuration as the UI.
    pub fn add_github_broker(&mut self, name: &str, config: &Value) {
        self.add_linked_ui(name, config);
        self.programs.get_mut(&program(name)).unwrap().args[0] = "github".into();
    }

    /// Connects to the UI service the way a window does.
    pub fn connect_ui(&self, name: &str) -> UiClient {
        self.connect_ui_within(name, TIMEOUT)
    }

    /// [`Self::connect_ui`], waiting up to `patience` for each answer: for a
    /// call that is meant to take long.
    pub fn connect_ui_within(&self, name: &str, patience: Duration) -> UiClient {
        let path = self.host(&format!("{name}-socket/ui.sock"));
        assert!(wait_for(TIMEOUT, || path.exists()), "{name} never opened its socket");
        let stream = open_front_end(&path, patience);
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        UiClient { reader, writer: stream, next_id: 1 }
    }

    /// Connects to a labelled UI service as the platform hands it a team
    /// member's window: the label line first, with the nickname `member` and an
    /// account made from it.
    pub fn connect_ui_as(&self, name: &str, member: Option<&str>) -> UiClient {
        self.connect_ui_labelled(name, &window_label(member.unwrap_or("member"), "member"))
    }

    /// Connects to a labelled UI service with the label the platform writes
    /// for the connection, e.g. the platform's mirror or service link.
    pub fn connect_ui_labelled(&self, name: &str, label: &Value) -> UiClient {
        self.connect_ui_labelled_within(name, label, TIMEOUT)
    }

    /// [`Self::connect_ui_labelled`], waiting up to `patience` for each answer.
    pub fn connect_ui_labelled_within(&self, name: &str, label: &Value, patience: Duration) -> UiClient {
        let mut client = self.connect_ui_within(name, patience);
        writeln!(client.writer, "{label}").expect("write the label");
        client
    }

    /// The log of the default project's `session` as the conversation
    /// service wrote it, markers included.
    pub fn conversation_log(&self, session: &str) -> Vec<Value> {
        let path = format!("sessions/{DEFAULT_PROJECT}/{session}/log.jsonl");
        let text = std::fs::read_to_string(self.host(&path)).unwrap_or_default();
        text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    pub fn out_dir(&self, role: &str) -> PathBuf {
        self.child_outputs.get(role).cloned().unwrap_or_else(|| self.host(&format!("out/{role}")))
    }

    /// Gives the child an output directory inside its parent's existing writable
    /// directory. Does not add any filesystem authority to the parent.
    pub fn child_workspace(&mut self, parent: &str, child: &str) {
        let path = self.out_dir(parent).join(child);
        std::fs::create_dir_all(&path).unwrap();
        let spec = self.programs.get_mut(&program(child)).unwrap();
        spec.binds.iter_mut().find(|b| b.guest == std::path::Path::new("/out")).unwrap().host = path.clone();
        self.child_outputs.insert(child.to_owned(), path);
    }

    /// Creates the file an `await_operator` tool call is waiting for.
    pub fn signal(&self, role: &str, name: &str) {
        std::fs::write(self.out_dir(role).join(name), "").unwrap();
    }

    /// The supervisor configuration for this scenario, for tests that need to
    /// break it on purpose.
    pub fn config(&self, adjust: impl FnOnce(&mut Config), outbound_queue: usize) -> SupervisorConfig {
        let mut limits = Config::with_programs(BTreeSet::new());
        adjust(&mut limits);
        SupervisorConfig {
            limits,
            programs: self.programs.clone(),
            sandbox_tool: None,
            grace: Duration::from_millis(300),
            outbound_queue,
            tick: Duration::from_millis(20),
            identity_helper: None,
            cgroups: false,
            max_active_projects: 96,
        }
    }

    pub fn start_with(&self, config: SupervisorConfig) -> Result<Supervisor, StartError> {
        let events = Arc::clone(&self.events);
        Supervisor::start(
            config,
            Arc::new(move |event: &SecurityEvent| {
                events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event.clone());
            }),
        )
    }

    pub fn start(&self) -> Supervisor {
        self.start_with(self.config(|_| {}, 64)).expect("supervisor starts")
    }

    /// Boots a service and waits until it provides `endpoint_name`.
    pub fn boot_service(&self, supervisor: &Supervisor, name: &str, endpoint_name: &str) -> ProcessId {
        self.boot_service_within(supervisor, name, endpoint_name, TIMEOUT)
    }

    /// As [`Self::boot_service`], with an explicit bound for a service whose
    /// start is legitimately slow: the `lean` service imports Mathlib into its
    /// resident REPL *before* it provides (seconds on a warm page cache, much
    /// longer on a cold one), so a test that binds the REPL waits longer here.
    pub fn boot_service_within(
        &self,
        supervisor: &Supervisor,
        name: &str,
        endpoint_name: &str,
        timeout: Duration,
    ) -> ProcessId {
        let process = supervisor.boot(program(name), provider_grants(endpoint_name)).expect("boot service");
        let target = endpoint(endpoint_name);
        assert!(
            supervisor.wait_until(timeout, |s| s.provider_of(&target).is_some_and(|(p, _)| p == process)),
            "{name} never provided {endpoint_name}"
        );
        process
    }

    pub fn events(&self) -> Vec<SecurityEvent> {
        self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Children of `parent`, in start order.
    pub fn children_of(&self, parent: ProcessId) -> Vec<ProcessId> {
        self.events()
            .iter()
            .filter_map(|e| match e {
                SecurityEvent::ProcessStarted { process, parent: Some(p), .. } if *p == parent => {
                    Some(*process)
                }
                _ => None,
            })
            .collect()
    }

    fn lines(file: &Path) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
        // A line still being written has no newline yet and is ignored.
        let complete = text.rfind('\n').map_or("", |end| &text[..=end]);
        complete.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// Transcript lines written so far by `role`.
    pub fn transcript(&self, role: &str) -> Vec<Value> {
        Self::lines(&self.out_dir(role).join("transcript.jsonl"))
    }

    /// Waits until `role` has written at least `count` lines.
    pub fn wait_transcript(&self, role: &str, count: usize) -> Vec<Value> {
        let ok = wait_for(TIMEOUT, || self.transcript(role).len() >= count);
        let got = self.transcript(role);
        assert!(ok, "{role} wrote {} of {count} transcript lines: {got:?}", got.len());
        got
    }

    /// Waits until `role` finished its fixture; returns the tool lines only.
    pub fn wait_done(&self, role: &str) -> Vec<Value> {
        self.wait_done_within(role, TIMEOUT)
    }

    /// As [`Self::wait_done`], with an explicit bound for agents whose calls
    /// are legitimately slow (e.g. each Lean verification is a real
    /// elaboration of `import Mathlib`, seconds apiece).
    pub fn wait_done_within(&self, role: &str, timeout: Duration) -> Vec<Value> {
        let done = |lines: &[Value]| lines.last().is_some_and(|l| l["done"] == true);
        let ok = wait_for(timeout, || done(&self.transcript(role)));
        let mut got = self.transcript(role);
        assert!(ok, "{role} never finished: {got:?}");
        got.pop();
        got
    }

    /// Dispatches the fs service rooted at `data` has received.
    pub fn access_log(&self, data: &str) -> Vec<Value> {
        Self::lines(&self.host(&format!("{data}-log/access.jsonl")))
    }

    /// Waits until the service at `data` has received a dispatch for `file_name`.
    pub fn wait_access(&self, data: &str, file_name: &str) {
        let seen = || self.access_log(data).iter().any(|l| mentions(l, file_name));
        assert!(wait_for(TIMEOUT, seen), "{data} never saw a request for {file_name}");
    }

    /// Feeds `content` to whoever is blocked reading the FIFO, then closes it.
    /// Waits for a reader to be present instead of blocking in `open`.
    pub fn feed_fifo(&self, relative: &str, content: &str) {
        let target = self.host(relative);
        use std::os::unix::fs::OpenOptionsExt;
        let mut pipe = None;
        let opened = wait_for(TIMEOUT, || {
            pipe = std::fs::OpenOptions::new().write(true).custom_flags(O_NONBLOCK).open(&target).ok();
            pipe.is_some()
        });
        assert!(opened, "nobody is reading {}", target.display());
        pipe.expect("opened").write_all(content.as_bytes()).unwrap();
    }
}

/// True when an access-log record's resource names `file_name`.
pub fn mentions(record: &Value, file_name: &str) -> bool {
    record["resource"].as_array().is_some_and(|segments| segments.iter().any(|s| s == file_name))
}

/// Error code of a transcript line, if the call failed.
pub fn error_code(line: &Value) -> Option<&str> {
    line["result"]["error"]["code"].as_str()
}

/// Success value of a transcript line, if the call succeeded.
pub fn ok_value(line: &Value) -> Option<&Value> {
    line["result"].get("ok")
}

/// Polls `condition` until it holds or `timeout` passes.
pub fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// True when no host process with this pid exists (it was reaped).
pub fn pid_gone(pid: u32) -> bool {
    // Signal 0 probes for existence without delivering anything.
    !Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Simulates a crash of the host process.
pub fn sigkill(pid: u32) {
    let status = Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
    assert!(status.success(), "kill -9 {pid}");
}

/// Opens the UI service's endpoint, a Unix socket.
fn open_front_end(endpoint: &Path, patience: Duration) -> std::os::unix::net::UnixStream {
    // bind() publishes the pathname before listen() makes it connectable.
    // Waiting for the file alone races service startup.
    let mut stream = None;
    let connected = wait_for(TIMEOUT, || match std::os::unix::net::UnixStream::connect(endpoint) {
        Ok(opened) => {
            stream = Some(opened);
            true
        }
        Err(e)
            if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) =>
        {
            false
        }
        Err(e) => panic!("connect to the UI service {}: {e}", endpoint.display()),
    });
    assert!(connected, "the UI service never listened on {}", endpoint.display());
    let stream = stream.expect("connected");
    stream.set_read_timeout(Some(patience)).unwrap();
    stream
}

/// The label line the platform writes for a window: `role` (`owner` or
/// `member`), the nickname `nickname` and an account made from it.
pub fn window_label(nickname: &str, role: &str) -> Value {
    serde_json::json!({"v": 1, "kind": "window", "account": account_of(nickname), "nickname": nickname, "role": role})
}

/// The account id [`window_label`] gives the person called `nickname`: what
/// the records name as the author of what that window says.
pub fn account_of(nickname: &str) -> String {
    let hex: String = nickname.bytes().take(31).map(|b| format!("{b:02x}")).collect();
    format!("a-{hex}")
}

/// A window's connection to the UI service: JSON lines over its local endpoint.
pub struct UiClient {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_id: u64,
}

impl UiClient {
    /// Sends one operation. `Ok` holds the result, `Err` the error object.
    pub fn request(&mut self, operation: Value) -> Result<Value, Value> {
        let id = self.send(operation);
        let (answered, result) = self.receive();
        // An unparsable request is answered without an id.
        assert!(answered.is_none_or(|answered| answered == id), "answered out of order");
        result
    }

    /// Sends one operation without reading its answer; returns its id.
    pub fn send(&mut self, mut operation: Value) -> u64 {
        let id = self.next_id;
        operation["id"] = serde_json::json!(id);
        self.next_id += 1;
        writeln!(self.writer, "{operation}").expect("write to the UI service");
        id
    }

    /// The next answer and the id it answers.
    pub fn receive(&mut self) -> (Option<u64>, Result<Value, Value>) {
        use std::io::BufRead;
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read from the UI service");
        let response: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"));
        let id = response["id"].as_u64();
        match response.get("ok") {
            Some(ok) => (id, Ok(ok.clone())),
            None => (id, Err(response["error"].clone())),
        }
    }

    pub fn say(&mut self, session: &str, id: &str, text: &str) -> Result<Value, Value> {
        self.request(
            serde_json::json!({"op": "conversation.say", "session": session, "message_id": id, "text": text}),
        )
    }

    pub fn history(&mut self, session: &str) -> Vec<Value> {
        self.history_before(session, Instant::now() + TIMEOUT)
    }

    fn history_before(&mut self, session: &str, deadline: Instant) -> Vec<Value> {
        let mut records = Vec::new();
        let mut after = None;
        loop {
            // One budget for the entire pagination, not a fresh wait for each
            // page. An in-flight request still uses the existing UI transport's
            // timeout; the adapter has no per-request deadline field.
            assert!(Instant::now() < deadline, "conversation history did not finish within its budget");
            let got = self.request(serde_json::json!({
                "op": "conversation.history", "session": session, "after_seq": after,
            }));
            let page: nucleus_conversation_interface::HistoryResponse =
                serde_json::from_value(got.expect("history")).expect("valid history response");
            let next = page.continuation(after).expect("history sequence must advance");
            assert!(Instant::now() < deadline, "conversation history did not finish within its budget");
            records.extend(page.records);
            match next {
                Some(seq) => after = Some(seq),
                None => return records,
            }
        }
    }

    /// Polls the visible history until `turns` turns have ended (completed or interrupted).
    pub fn wait_turns(&mut self, session: &str, turns: usize) -> Vec<Value> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let log = self.history_before(session, deadline);
            if log.iter().filter(|r| r["kind"] == "turn_complete" || r["kind"] == "interrupted").count()
                >= turns
            {
                return log;
            }
            assert!(Instant::now() < deadline, "turn {turns} never completed: {log:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
