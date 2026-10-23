//! The service's options: deployment facts only. A run's process is started
//! with the same arguments, so its options are the service's.

use super::*;

/// Deployment facts only: where things are, what a task may ask for.
/// Nothing here names a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The arguments as given. Every run's process is started with them
    /// ([`wire::RunCommand`]), so its options are the service's.
    pub args: Vec<String>,
    /// This program, an absolute path in the service's namespace: what each
    /// run's process is started from. The service does not start without it.
    pub run_executable: Option<PathBuf>,
    /// The most runs executing at once; the others admitted wait, first come
    /// first served.
    pub max_running_runs: usize,
    /// Bound on saved journal and worker receipt bytes read by one resume.
    pub resume_max_bytes: u64,
    /// Run directories are created here: `<out>/<project>/<run>`, and a
    /// run's process sees `<out>/<project>` as its own `--out`.
    pub out: PathBuf,
    /// The projects directory in the service's namespace: with it, a run is
    /// started only for a project that has a home there.
    pub projects: Option<PathBuf>,
    /// Where a task's `problems_file` and `prompt_template_file` resolve.
    pub tasks_dir: PathBuf,
    /// Where a task's `policy` resolves.
    pub policies_dir: PathBuf,
    /// The worker executable, an absolute path in this process's namespace.
    pub prover_executable: PathBuf,
    /// Arguments every worker gets, in order, before the harness's own.
    pub prover_args: Vec<String>,
    pub discovery_endpoint: String,
    pub explore_endpoint: String,
    pub lean_endpoint: String,
    /// The endpoint of the `shell` interface. When this process holds a
    /// delegable `shell exec` capability on it, every worker gets a workspace
    /// there and runs the tool loop; without one, workers run the fixed
    /// candidate/verdict loop. It is the shell of a run whose workers have no
    /// network.
    pub shell_endpoint: String,
    /// The endpoint of the workers' shell that has network
    /// (`--network-shell-endpoint`), when the deployment has one: a run whose
    /// workers have network works there instead.
    pub network_shell_endpoint: Option<String>,
    /// Whether the workers of a run whose task does not say have network
    /// (`--worker-network on|off`). Off unless given: a result must not come
    /// from looking the proof up.
    pub worker_network: bool,
    /// The most problems one run may explore at once: the ceiling on a task's
    /// `concurrency`.
    pub max_concurrency: usize,
    /// The most workers one round may run (`--max-width`): the ceiling on a
    /// task's `limits.w`, what the machine can run at once.
    pub max_width: usize,
    /// The most problems one task may name (after de-duplication and `range`).
    pub max_problems_per_task: usize,
    /// The most runs being admitted, queued or running at once; a `start` or
    /// `resume` beyond it is refused.
    pub max_queued_runs: usize,
    /// Micro-USD one problem's rollout may spend: a task's cap defaults to it
    /// and may not exceed it. Spent against what the providers charged, which
    /// the workers report with their submissions; this service prices nothing
    /// itself. 0 only with `--no-usd-ceiling`: the operator's explicit choice
    /// to run without one, and a task may then leave its cap off.
    pub max_usd_micro_per_problem: u64,
    /// Wall clock one worker gets per model call of its grant; a round is
    /// given `calls_per_continuation + collection_calls` times this (module
    /// doc, "The loop", 4).
    pub worker_timeout_ms: u64,
    /// A round in which every continuation failed for a provider reason
    /// (`model_error`: the worker could not get an answer from the model) is
    /// retried after a doubling backoff from `provider_backoff_ms`, this many
    /// times per problem, before the rollout stops as `provider_unavailable`
    /// — the provider's outage is not the tree's dead end.
    pub provider_retries: u32,
    pub provider_backoff_ms: u64,
    /// Runs a `.py` policy program (absolute path).
    pub policy_interpreter: PathBuf,
    /// Deadline for each of the policy process's replies.
    pub policy_timeout_ms: u64,
    /// A run's process only (`--project`, which the service appends like
    /// `--out`): the project of its run, which its workers' model calls name
    /// (`--project` to the prover). Metadata for the provider's records,
    /// never authority; the service refuses it.
    pub project: Option<String>,
}

/// `--tasks-dir` unless given.
pub const DEFAULT_TASKS_DIR: &str = "/tasks";
/// `--policies-dir` unless given.
pub const DEFAULT_POLICIES_DIR: &str = "/policies";
/// `--max-concurrency` unless given.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
/// `--max-width` unless given.
pub const DEFAULT_MAX_WIDTH: usize = 16;
/// `--max-problems-per-task` unless given: the whole curriculum file,
/// `program-files/discovery/curricula/curriculum.txt`, fits.
pub const DEFAULT_MAX_PROBLEMS_PER_TASK: usize = 700;
/// `--max-queued-runs` unless given.
pub const DEFAULT_MAX_QUEUED_RUNS: usize = 8;
/// `--max-running-runs` unless given.
pub const DEFAULT_MAX_RUNNING_RUNS: usize = 1;
/// `--worker-timeout-ms` unless given: the prover's default model deadline
/// plus its default verify deadline (300 s each), plus a wait at the harness.
pub const DEFAULT_WORKER_TIMEOUT_MS: u64 = 900_000;
/// Retries of a round lost to the provider, per problem (`--provider-retries`).
pub const DEFAULT_PROVIDER_RETRIES: u32 = 3;
/// First backoff before such a retry; doubles each time (`--provider-backoff-ms`).
pub const DEFAULT_PROVIDER_BACKOFF_MS: u64 = 60_000;
/// Default interpreter for a `.py` policy.
pub const DEFAULT_POLICY_INTERPRETER: &str = "/usr/bin/python3";
/// `--policy-timeout-ms` unless given.
pub const DEFAULT_POLICY_TIMEOUT_MS: u64 = 5000;

/// A USD option with at most six decimals (`"0.09576"`, `"20"`) as micro-USD
/// exactly — no floating point, so a ceiling is what the operator wrote.
fn micro_usd_option(flag: &str, value: &str) -> Result<u64, String> {
    discovery::micro_usd(value)
        .ok_or_else(|| format!("{flag} must be a USD amount with at most six decimals, got {value:?}"))
}

impl Options {
    /// Parses `--out <dir> --prover-executable <path> [--prover-arg <arg>]…
    /// [--run-executable <path>] [--projects <dir>] [--tasks-dir <dir>] [--policies-dir <dir>]
    /// [--discovery-endpoint <name>] [--explore-endpoint <name>] [--lean-endpoint <name>]
    /// [--shell-endpoint <name>] [--network-shell-endpoint <name>] [--worker-network on|off]
    /// [--max-concurrency <n>] [--max-width <n>] [--max-problems-per-task <n>]
    /// [--max-queued-runs <n>] [--max-running-runs <n>] [--resume-max-bytes <n>]
    /// (--max-usd-per-problem <usd> | --no-usd-ceiling)
    /// [--worker-timeout-ms <n>] [--provider-retries <n>]
    /// [--provider-backoff-ms <n>] [--policy-interpreter <path>]
    /// [--policy-timeout-ms <n>] [--project <name>]`. The USD ceiling is required, or its absence
    /// declared: a service that would silently run tasks without one is a
    /// misconfiguration. A task's problems, template, policy and limits are
    /// task fields, not options.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let given: Vec<String> = args.into_iter().collect();
        let (mut out, mut executable, mut no_usd_ceiling) = (None, None, false);
        let mut options = Options {
            args: given.clone(),
            run_executable: None,
            max_running_runs: DEFAULT_MAX_RUNNING_RUNS,
            resume_max_bytes: 64 * 1024 * 1024,
            out: PathBuf::new(),
            projects: None,
            tasks_dir: PathBuf::from(DEFAULT_TASKS_DIR),
            policies_dir: PathBuf::from(DEFAULT_POLICIES_DIR),
            prover_executable: PathBuf::new(),
            prover_args: Vec::new(),
            discovery_endpoint: discovery::ENDPOINT.to_owned(),
            explore_endpoint: explore::ENDPOINT.to_owned(),
            lean_endpoint: lean::ENDPOINT.to_owned(),
            shell_endpoint: shell::ENDPOINT.to_owned(),
            network_shell_endpoint: None,
            worker_network: false,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_width: DEFAULT_MAX_WIDTH,
            max_problems_per_task: DEFAULT_MAX_PROBLEMS_PER_TASK,
            max_queued_runs: DEFAULT_MAX_QUEUED_RUNS,
            max_usd_micro_per_problem: 0,
            worker_timeout_ms: DEFAULT_WORKER_TIMEOUT_MS,
            provider_retries: DEFAULT_PROVIDER_RETRIES,
            provider_backoff_ms: DEFAULT_PROVIDER_BACKOFF_MS,
            policy_interpreter: PathBuf::from(DEFAULT_POLICY_INTERPRETER),
            policy_timeout_ms: DEFAULT_POLICY_TIMEOUT_MS,
            project: None,
        };
        let number = |flag: &str, value: &str| -> Result<u64, String> {
            value.parse().map_err(|_| format!("{flag} must be a number"))
        };
        let endpoint = |flag: &str, value: String| -> Result<String, String> {
            EndpointName::new(value.as_str()).map_err(|e| format!("{flag}: {}", e.message))?;
            Ok(value)
        };
        let count = |flag: &str, value: &str| -> Result<usize, String> {
            usize::try_from(number(flag, value)?).map_err(|_| format!("{flag} is too large"))
        };
        let mut args = given.into_iter();
        while let Some(flag) = args.next() {
            if flag == "--no-usd-ceiling" {
                no_usd_ceiling = true;
                continue;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--out" => out = Some(PathBuf::from(value)),
                "--projects" => options.projects = Some(PathBuf::from(value)),
                "--run-executable" => options.run_executable = Some(PathBuf::from(value)),
                "--tasks-dir" => options.tasks_dir = PathBuf::from(value),
                "--policies-dir" => options.policies_dir = PathBuf::from(value),
                "--prover-executable" => executable = Some(PathBuf::from(value)),
                "--prover-arg" => options.prover_args.push(value),
                "--discovery-endpoint" => options.discovery_endpoint = endpoint(&flag, value)?,
                "--explore-endpoint" => options.explore_endpoint = endpoint(&flag, value)?,
                "--lean-endpoint" => options.lean_endpoint = endpoint(&flag, value)?,
                "--shell-endpoint" => options.shell_endpoint = endpoint(&flag, value)?,
                "--network-shell-endpoint" => {
                    options.network_shell_endpoint = Some(endpoint(&flag, value)?);
                }
                "--worker-network" => {
                    options.worker_network = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        other => return Err(format!("--worker-network takes on or off, not {other:?}")),
                    };
                }
                "--resume-max-bytes" => {
                    options.resume_max_bytes = value.parse().map_err(|_| "invalid --resume-max-bytes")?;
                    if options.resume_max_bytes == 0 {
                        return Err("--resume-max-bytes must be positive".into());
                    }
                }
                "--max-concurrency" => options.max_concurrency = count(&flag, &value)?,
                "--max-width" => options.max_width = count(&flag, &value)?,
                "--max-problems-per-task" => options.max_problems_per_task = count(&flag, &value)?,
                "--max-queued-runs" => options.max_queued_runs = count(&flag, &value)?,
                "--max-running-runs" => options.max_running_runs = count(&flag, &value)?,
                "--max-usd-per-problem" => {
                    options.max_usd_micro_per_problem = micro_usd_option(&flag, &value)?
                }
                "--worker-timeout-ms" => options.worker_timeout_ms = number(&flag, &value)?,
                "--provider-retries" => {
                    options.provider_retries = u32::try_from(number(&flag, &value)?).unwrap_or(u32::MAX)
                }
                "--provider-backoff-ms" => options.provider_backoff_ms = number(&flag, &value)?,
                "--policy-interpreter" => options.policy_interpreter = PathBuf::from(value),
                "--policy-timeout-ms" => options.policy_timeout_ms = number(&flag, &value)?,
                "--project" => {
                    if !nucleus_protocol::valid_project(&value) {
                        return Err(format!("--project {value:?} is not a project's name"));
                    }
                    options.project = Some(value);
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        options.out = out.ok_or("--out is required")?;
        options.prover_executable = executable.ok_or("--prover-executable is required")?;
        if options.discovery_endpoint == options.explore_endpoint {
            return Err("--discovery-endpoint and --explore-endpoint must differ".to_owned());
        }
        if options.max_concurrency == 0 {
            return Err("--max-concurrency must be at least 1, or every task is refused".to_owned());
        }
        if options.max_width == 0 {
            return Err("--max-width must be at least 1, or every task is refused".to_owned());
        }
        if options.max_problems_per_task == 0 {
            return Err("--max-problems-per-task must be at least 1, or every task is refused".to_owned());
        }
        if options.max_queued_runs == 0 {
            return Err("--max-queued-runs must be at least 1, or every task is refused".to_owned());
        }
        // A running run counts among the queued or running ones.
        if options.max_running_runs == 0 || options.max_running_runs > options.max_queued_runs {
            return Err("--max-running-runs must be between 1 and --max-queued-runs".to_owned());
        }
        if options.policy_timeout_ms == 0 {
            return Err("--policy-timeout-ms must be at least 1".to_owned());
        }
        if options.worker_timeout_ms == 0 {
            return Err("--worker-timeout-ms must be at least 1".to_owned());
        }
        match (options.max_usd_micro_per_problem, no_usd_ceiling) {
            (0, false) => {
                return Err(
                    "--max-usd-per-problem <usd> is required; pass --no-usd-ceiling to run tasks with \
                     no per-problem USD ceiling"
                        .to_owned(),
                );
            }
            (1.., true) => {
                return Err("--no-usd-ceiling contradicts --max-usd-per-problem; give one of them".to_owned());
            }
            _ => {}
        }
        // The kernel names an executable by its absolute guest path, and the
        // policy process gets an empty environment, hence no PATH: only
        // absolute paths mean what they say. Both travel as text.
        let mut paths = vec![("--prover-executable", options.prover_executable.as_path())];
        paths.push(("--policy-interpreter", options.policy_interpreter.as_path()));
        paths.extend(options.run_executable.as_deref().map(|path| ("--run-executable", path)));
        for (flag, path) in paths {
            if !path.is_absolute() {
                return Err(format!("{flag} must be an absolute path, got {}", path.display()));
            }
            if path.to_str().is_none() {
                return Err(format!("{flag} must be valid UTF-8"));
            }
        }
        Ok(options)
    }

    /// What a task may ask for.
    #[must_use]
    pub fn ceilings(&self) -> Ceilings {
        Ceilings {
            max_width: self.max_width,
            max_concurrency: self.max_concurrency,
            max_usd_micro_per_problem: self.max_usd_micro_per_problem,
            max_problems_per_task: self.max_problems_per_task,
        }
    }

    /// The wall clock one round of `limits` gets (module doc, "The loop", 4).
    #[must_use]
    pub fn round_timeout(&self, limits: &Limits) -> Duration {
        Duration::from_millis(
            limits
                .calls_per_continuation
                .saturating_add(limits.collection_calls)
                .saturating_mul(self.worker_timeout_ms),
        )
    }

    /// Startup check of what the options point at: the output directory must
    /// exist (every run is created under it) and the worker executable must
    /// be a file this process can see. The tasks and policies directories are
    /// checked when a task refers into them.
    pub fn check_paths(&self) -> Result<(), String> {
        if !self.out.is_dir() {
            return Err(format!("--out {}: not a directory this harness can see", self.out.display()));
        }
        if !self.prover_executable.is_file() {
            return Err(format!(
                "--prover-executable {}: not a file this harness can see (is it bound into its sandbox?)",
                self.prover_executable.display()
            ));
        }
        if let Some(run) = &self.run_executable
            && !run.is_file()
        {
            return Err(format!(
                "--run-executable {}: not a file this service can see (is it bound into its sandbox?)",
                run.display()
            ));
        }
        Ok(())
    }

    /// The policy a task's `policy` field selects, once resolved to `program`.
    #[must_use]
    pub fn policy_source(&self, program: Option<PathBuf>) -> PolicySource {
        match program {
            None => PolicySource::Builtin,
            Some(program) => PolicySource::Process {
                program,
                interpreter: self.policy_interpreter.clone(),
                timeout: Duration::from_millis(self.policy_timeout_ms),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::tests::{TEST_INTERPRETER, bare_args, base_args, limits, parse, scratch};

    #[test]
    fn builtin_policy_needs_no_interpreter_but_python_refuses_relative_paths() {
        let dir = scratch("builtin_no_interpreter");
        let prover = dir.join("prover");
        let script = dir.join("policy.py");
        std::fs::write(&prover, "fixture").unwrap();
        std::fs::write(&script, "fixture").unwrap();
        let args = [
            "--out".to_owned(),
            dir.to_string_lossy().into_owned(),
            "--prover-executable".to_owned(),
            prover.to_string_lossy().into_owned(),
            "--no-usd-ceiling".to_owned(),
        ];
        let options = Options::parse(args.clone()).unwrap();
        options.check_paths().unwrap();
        options.policy_source(None).check().unwrap();
        let bad = PolicySource::Process {
            program: script,
            interpreter: "python3".into(),
            timeout: Duration::from_secs(1),
        };
        assert!(bad.check().unwrap_err().contains("absolute --policy-interpreter"));
        let mut explicit = args.to_vec();
        explicit.extend(["--policy-interpreter".to_owned(), "python3".to_owned()]);
        assert!(Options::parse(explicit).unwrap_err().contains("absolute"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn options_need_an_output_directory_and_the_prover_executable_and_have_the_service_defaults() {
        assert!(parse(&["--prover-executable", "/p"]).unwrap_err().contains("--out"));
        assert!(parse(&["--out", "/o"]).unwrap_err().contains("--prover-executable"));
        let ok = parse(&base_args()).unwrap();
        assert_eq!(
            ok.ceilings(),
            Ceilings {
                max_concurrency: 4,
                max_usd_micro_per_problem: 0,
                max_problems_per_task: DEFAULT_MAX_PROBLEMS_PER_TASK,
                max_width: DEFAULT_MAX_WIDTH
            }
        );
        assert_eq!(
            (ok.max_queued_runs, ok.max_running_runs, ok.worker_timeout_ms),
            (DEFAULT_MAX_QUEUED_RUNS, 1, DEFAULT_WORKER_TIMEOUT_MS)
        );
        assert_eq!(
            (ok.discovery_endpoint.as_str(), ok.explore_endpoint.as_str(), ok.lean_endpoint.as_str()),
            ("discovery", "explore", "lean")
        );
        assert_eq!(
            (ok.policy_interpreter.as_path(), ok.policy_timeout_ms),
            (Path::new(DEFAULT_POLICY_INTERPRETER), DEFAULT_POLICY_TIMEOUT_MS)
        );
        assert!(ok.prover_args.is_empty() && ok.run_executable.is_none());

        let mut args = bare_args();
        args.extend([
            "--prover-arg",
            "--model",
            "--prover-arg",
            "deepseek/deepseek-v4-flash",
            "--max-problems-per-task",
            "50",
            "--max-queued-runs",
            "3",
            "--max-running-runs",
            "2",
            "--run-executable",
            "/programs/nucleus-discovery",
            "--worker-timeout-ms",
            "1000",
            "--tasks-dir",
            "/t",
            "--policies-dir",
            "/p",
            "--discovery-endpoint",
            "disc2",
            "--explore-endpoint",
            "explore2",
            "--lean-endpoint",
            "lean2",
            "--max-concurrency",
            "2",
            "--max-width",
            "4",
            "--max-usd-per-problem",
            "20",
            "--policy-interpreter",
            TEST_INTERPRETER,
            "--policy-timeout-ms",
            "250",
        ]);
        let tuned = parse(&args).unwrap();
        assert_eq!(tuned.prover_args, ["--model", "deepseek/deepseek-v4-flash"], "in order");
        assert_eq!(
            (tuned.tasks_dir.as_path(), tuned.policies_dir.as_path()),
            (Path::new("/t"), Path::new("/p"))
        );
        assert_eq!(
            (
                tuned.discovery_endpoint.as_str(),
                tuned.explore_endpoint.as_str(),
                tuned.lean_endpoint.as_str()
            ),
            ("disc2", "explore2", "lean2")
        );
        assert_eq!(
            tuned.ceilings(),
            Ceilings {
                max_concurrency: 2,
                max_usd_micro_per_problem: 20_000_000,
                max_problems_per_task: 50,
                max_width: 4
            }
        );
        assert_eq!((tuned.max_queued_runs, tuned.max_running_runs, tuned.worker_timeout_ms), (3, 2, 1000));
        assert_eq!(tuned.run_executable.as_deref(), Some(Path::new("/programs/nucleus-discovery")));
        assert_eq!(
            tuned.round_timeout(&Limits { calls_per_continuation: 2, ..limits() }),
            Duration::from_millis(7000),
            "a round includes the five collection calls and gets the per-call allowance times the calls of one worker"
        );
        assert_eq!(
            (tuned.policy_interpreter.as_path(), tuned.policy_timeout_ms),
            (Path::new(TEST_INTERPRETER), 250)
        );
    }

    #[test]
    fn misconfigured_options_are_refused_with_a_reason() {
        let with = |extra: &[&str]| parse(&[&base_args()[..], extra].concat());
        let bare = |extra: &[&str]| parse(&[&bare_args()[..], extra].concat());
        assert!(
            parse(&["--out", "/o", "--prover-executable", "prover", "--no-usd-ceiling"])
                .unwrap_err()
                .contains("absolute")
        );
        assert!(with(&["--policy-interpreter", "python3"]).unwrap_err().contains("absolute"));
        assert!(with(&["--max-concurrency", "0"]).unwrap_err().contains("--max-concurrency"));
        assert!(with(&["--max-width", "0"]).unwrap_err().contains("--max-width"));
        assert!(with(&["--max-problems-per-task", "0"]).unwrap_err().contains("--max-problems-per-task"));
        assert!(with(&["--max-queued-runs", "0"]).unwrap_err().contains("--max-queued-runs"));
        assert!(with(&["--max-running-runs", "0"]).unwrap_err().contains("--max-running-runs"));
        assert!(
            with(&["--max-queued-runs", "2", "--max-running-runs", "3"]).is_err(),
            "a running run is queued too"
        );
        assert!(with(&["--run-executable", "nucleus-discovery"]).unwrap_err().contains("absolute"));
        assert!(with(&["--worker-timeout-ms", "0"]).unwrap_err().contains("--worker-timeout-ms"));
        assert!(with(&["--policy-timeout-ms", "0"]).is_err());
        // The USD ceiling: required, or declared absent, never silently absent.
        let missing = bare(&[]).unwrap_err();
        assert!(
            missing.contains("--max-usd-per-problem") && missing.contains("--no-usd-ceiling"),
            "{missing}"
        );
        assert!(bare(&[]).unwrap_err().contains("required"), "the ceiling is required or declared absent");
        assert!(with(&["--max-usd-per-problem", "20"]).unwrap_err().contains("contradicts"));
        let capped = bare(&["--max-usd-per-problem", "20"]).unwrap();
        assert_eq!(capped.max_usd_micro_per_problem, 20_000_000);
        assert!(with(&["--explore-endpoint", "discovery"]).unwrap_err().contains("differ"));
        assert!(with(&["--lean-endpoint", "not a name"]).is_err());
        assert!(with(&["--out"]).unwrap_err().contains("needs a value"));
        for bad in ["1.2345678", ".5", "abc", "-1", "1e3", "1.2.3", ""] {
            assert!(bare(&["--max-usd-per-problem", bad]).is_err(), "{bad:?} is not an exact USD amount");
        }
        assert!(with(&["--price-in", "1.5"]).is_err(), "the service prices nothing; the provider does");
    }

    #[test]
    fn path_checks_need_the_output_directory_and_the_executable_to_exist() {
        let dir = scratch("paths");
        let exe = dir.join("prover");
        std::fs::write(&exe, "").unwrap();
        let with = |out: &Path, exe: &Path| {
            parse(&[
                "--out",
                out.to_str().unwrap(),
                "--prover-executable",
                exe.to_str().unwrap(),
                "--no-usd-ceiling",
            ])
            .unwrap()
            .check_paths()
        };
        assert!(with(&dir, &exe).is_ok());
        assert!(with(&dir.join("missing"), &exe).unwrap_err().contains("--out"));
        assert!(with(&dir, &dir.join("missing")).unwrap_err().contains("--prover-executable"));
        assert!(with(&dir, &dir).unwrap_err().contains("--prover-executable"), "a directory is not a file");
        let (out, exe) = (dir.to_str().unwrap(), exe.to_str().unwrap());
        let missing = dir.join("missing").to_str().unwrap().to_owned();
        let run = |path: &str| {
            parse(&["--out", out, "--prover-executable", exe, "--no-usd-ceiling", "--run-executable", path])
                .unwrap()
                .check_paths()
        };
        assert!(run(exe).is_ok());
        assert!(run(&missing).unwrap_err().contains("--run-executable"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
