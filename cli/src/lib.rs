//! Launcher: validates an operator configuration, boots the kernel under the
//! supervisor, starts the configured processes in order, and shuts down. It
//! runs the kernel and the supervisor in its own process, so for trust it is
//! inside the kernel's boundary; the kernel never links it
//! (`tests/tests/architecture.rs`).
//!
//! The configuration file is operator authority. It is the only place root
//! capabilities and sandbox binds come from, so it is validated strictly:
//! unknown fields, unknown programs, and invalid names are startup errors.
//! Validation checks form, not breadth: an over-broad grant or bind is
//! honoured as written. The platform's values file fills the parameters the
//! configuration declares, in program arguments only (`parameters.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nucleus_kernel::{CapSpec, Config as Limits, SecurityEvent};
use nucleus_platform::{Bind, EventSink, ProgramSpec, Supervisor, SupervisorConfig};
use nucleus_protocol::{
    ACTION_PROVIDE, ActionName, EndpointName, KERNEL_ENDPOINT, ProcessId, ProgramName, ResourcePath,
};
use serde::Deserialize;

pub mod parameters;
pub mod serve;

use parameters::Parameters;
pub use parameters::{Arg, ParamName, ParamSpec, ParameterEvent};

/// A resident composition runs until `nucleus serve` is signalled to stop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifetime {
    #[default]
    Completion,
    Resident,
}

/// Operator configuration, as written in JSON. `A` is a program argument:
/// an [`Arg`] as written, a `String` once the parameters are filled in.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherConfig<A = String> {
    #[serde(default)]
    pub lifetime: Lifetime,
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u64,
    /// How long to wait for a booted service to provide its endpoint.
    #[serde(default = "default_ready_ms")]
    pub ready_timeout_ms: u64,
    #[serde(default)]
    pub limits: LimitOverrides,
    /// Values the platform may set, each in the program arguments that name
    /// it.
    #[serde(default)]
    pub parameters: BTreeMap<ParamName, ParamSpec>,
    pub programs: BTreeMap<ProgramName, ProgramConfig<A>>,
    /// Processes to start, in order. Later entries may depend on earlier
    /// ones, never the other way round ([`LauncherConfig::check_boot_order`]).
    pub boot: Vec<BootEntry>,
    /// The project identities. With them, every program runs in a cgroup of
    /// its own, and a `spawn_as` holder may start programs as a project,
    /// through the setuid `helper`: ordinary processes of the projects' Linux
    /// user, in no sandbox, in the project's cgroup.
    #[serde(default)]
    pub identities: Option<IdentitiesConfig>,
}

/// The helper that starts a project's process as the projects' user, whom
/// its own root-owned policy names, and how many projects may have
/// processes at once.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentitiesConfig {
    pub helper: PathBuf,
    /// Most projects with processes at once, counted by their cgroups.
    pub max_active: usize,
}

impl IdentitiesConfig {
    fn validate(&self) -> Result<(), String> {
        if !self.helper.is_absolute() {
            return Err("identities.helper must be an absolute path".into());
        }
        if self.max_active == 0 {
            return Err("identities.max_active must be at least 1".into());
        }
        Ok(())
    }
}

fn default_grace_ms() -> u64 {
    2_000
}
fn default_ready_ms() -> u64 {
    5_000
}

/// Optional replacements for the kernel's default limits.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitOverrides {
    pub max_processes: Option<usize>,
    pub max_handles_per_process: Option<usize>,
    pub max_pending_per_caller: Option<usize>,
    pub max_inflight_per_endpoint: Option<usize>,
    pub max_deadline_ms: Option<u64>,
}

/// One startable program. Relative paths are resolved against the
/// configuration file's directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramConfig<A = String> {
    pub executable: PathBuf,
    #[serde(default = "Vec::new")]
    pub args: Vec<A>,
    #[serde(default)]
    pub binds: Vec<BindConfig>,
    /// Unrestricted host network access for this program. Off unless stated.
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindConfig {
    pub host: PathBuf,
    pub guest: PathBuf,
    #[serde(default)]
    pub writable: bool,
}

/// A root capability. There is no default for any authority-bearing field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    pub endpoint: EndpointName,
    pub actions: BTreeSet<ActionName>,
    pub resource: ResourcePath,
    pub delegable: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootEntry {
    pub program: ProgramName,
    #[serde(default)]
    pub grants: Vec<GrantConfig>,
    /// Do not boot the next entry until this endpoint has a provider.
    #[serde(default)]
    pub provides: Option<EndpointName>,
    /// The launcher shuts down once every entry with this flag has exited.
    #[serde(default)]
    pub run_to_completion: bool,
}

impl LauncherConfig {
    /// Reads and validates a configuration file, every parameter at its
    /// default.
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_with(path, None, |_| {})
    }

    /// Reads a configuration file, fills in its parameters, and validates
    /// the result. A parameter has its default unless `values`, the
    /// platform's file, gives one it admits; what the file gives wrongly
    /// goes to `report`. Whether the configuration is valid never depends on
    /// that file.
    pub fn load_with(
        path: &Path,
        values: Option<&Path>,
        mut report: impl FnMut(ParameterEvent),
    ) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let declared: LauncherConfig<Arg> =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut config = declared.resolve(values, &mut report)?;
        // `nucleus run config.json` from inside the directory gives a parent
        // of "", which does not canonicalize. The directory it means is this
        // one.
        let base = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => Path::new(".").to_path_buf(),
        };
        let base = base.canonicalize().map_err(|e| format!("{}: {e}", base.display()))?;
        let absolute = |p: &Path| if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
        for program in config.programs.values_mut() {
            program.executable = absolute(&program.executable);
            for bind in &mut program.binds {
                bind.host = absolute(&bind.host);
            }
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.boot.is_empty() {
            return Err("boot list is empty".to_owned());
        }
        if let Some(identities) = &self.identities {
            identities.validate()?;
        }
        for entry in &self.boot {
            if !self.programs.contains_key(&entry.program) {
                return Err(format!("boot entry names unknown program {}", entry.program));
            }
            if entry.grants.iter().any(|g| g.actions.is_empty()) {
                return Err(format!("a grant for {} has no actions", entry.program));
            }
        }
        self.check_boot_order()?;
        if self.lifetime == Lifetime::Completion && !self.boot.iter().any(|e| e.run_to_completion) {
            return Err("no boot entry has run_to_completion; the launcher would never exit".to_owned());
        }
        if self.lifetime == Lifetime::Resident {
            let unique: BTreeSet<_> = self.boot.iter().map(|entry| &entry.program).collect();
            if unique.len() != self.boot.len() || self.boot.iter().any(|entry| entry.run_to_completion) {
                return Err(
                    "resident programs require unique boot names and no run_to_completion entries".into()
                );
            }
        }
        Ok(())
    }

    /// No entry is granted an endpoint that a later entry provides. A handle
    /// records the endpoint's generation when it is installed, at the
    /// holder's start; one taken before the endpoint's first provider is
    /// stale from then on, and the kernel refuses every call made with it
    /// until it is rebound.
    /// An entry provides each endpoint it holds `kernel provide` on, and its
    /// `provides`. Its own endpoints it may hold (to delegate to its
    /// children, whose handles are taken later); endpoints no entry provides
    /// are the business of whoever provides them at run time.
    fn check_boot_order(&self) -> Result<(), String> {
        let mut provider: BTreeMap<&str, usize> = BTreeMap::new();
        for (at, entry) in self.boot.iter().enumerate() {
            let granted = entry
                .grants
                .iter()
                .filter(|g| {
                    g.endpoint.as_str() == KERNEL_ENDPOINT
                        && g.actions.iter().any(|a| a.as_str() == ACTION_PROVIDE)
                })
                .filter_map(|g| match g.resource.segments() {
                    [kind, name] if kind == "endpoint" => Some(name.as_str()),
                    _ => None,
                });
            for name in granted.chain(entry.provides.as_ref().map(EndpointName::as_str)) {
                provider.entry(name).or_insert(at);
            }
        }
        for (at, entry) in self.boot.iter().enumerate() {
            for grant in &entry.grants {
                if let Some(&later) = provider.get(grant.endpoint.as_str())
                    && later > at
                {
                    return Err(format!(
                        "boot entry {} ({}) is granted {}, which boot entry {} ({}) provides only after it starts: \
                         boot {} before {}",
                        at + 1,
                        entry.program,
                        grant.endpoint,
                        later + 1,
                        self.boot[later].program,
                        self.boot[later].program,
                        entry.program,
                    ));
                }
            }
        }
        Ok(())
    }

    fn limits(&self) -> Limits {
        let mut limits = Limits::with_programs(self.programs.keys().cloned().collect());
        let o = &self.limits;
        macro_rules! apply {
            ($($field:ident),*) => { $( if let Some(value) = o.$field { limits.$field = value; } )* };
        }
        apply!(
            max_processes,
            max_handles_per_process,
            max_pending_per_caller,
            max_inflight_per_endpoint,
            max_deadline_ms
        );
        limits.projects = self.identities.is_some();
        limits
    }
}

impl LauncherConfig<Arg> {
    /// This configuration with each parameter's value in the arguments that
    /// name it. Nothing but the arguments changes.
    fn resolve(
        self,
        values: Option<&Path>,
        report: &mut dyn FnMut(ParameterEvent),
    ) -> Result<LauncherConfig, String> {
        let mut filled = Parameters::declared(&self.parameters)?;
        filled.check(self.programs.values().flat_map(|program| &program.args))?;
        if let Some(values) = values {
            filled.fill(values, report);
        }
        let LauncherConfig {
            lifetime,
            grace_ms,
            ready_timeout_ms,
            limits,
            parameters,
            programs,
            boot,
            identities,
        } = self;
        let programs = programs
            .into_iter()
            .map(|(name, program)| {
                let ProgramConfig { executable, args, binds, network } = program;
                Ok((name, ProgramConfig { executable, args: filled.expand(args)?, binds, network }))
            })
            .collect::<Result<_, String>>()?;
        Ok(LauncherConfig {
            lifetime,
            grace_ms,
            ready_timeout_ms,
            limits,
            parameters,
            programs,
            boot,
            identities,
        })
    }
}

// Console diagnostics are not a durable audit sink. A blocked or failed stderr
// must never suspend the kernel lock.
fn write_console_event(event: &SecurityEvent) {
    nucleus_platform::diagnostics::emit(format_args!("event: {event:?}"));
}

/// Reports what the platform's values file gave wrongly where the console
/// operator sees security events.
pub fn write_parameter_event(event: ParameterEvent) {
    nucleus_platform::diagnostics::emit(format_args!("event: {event:?}"));
}

/// Boots everything, prints events to stderr, waits for the run-to-completion
/// processes, and shuts down. Returns an error if startup fails or shutdown
/// is not clean.
pub fn run(config: LauncherConfig) -> Result<(), String> {
    if config.lifetime != Lifetime::Completion {
        return Err("resident configurations require nucleus serve".into());
    }
    let mut supervisor = start_supervisor(&config, Arc::new(write_console_event))?;
    let mut awaited = Vec::new();
    for entry in &config.boot {
        let process = boot_entry(&supervisor, entry, config.ready_timeout_ms)?;
        if entry.run_to_completion {
            awaited.push(process);
        }
    }
    while awaited.iter().any(|p| supervisor.is_live(*p)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = supervisor.shutdown();
    if report.is_clean() { Ok(()) } else { Err(format!("shutdown was not clean: {report:?}")) }
}

/// Each process's queue of messages to it, and how often the supervisor looks at its
/// processes.
const OUTBOUND_QUEUE: usize = 64;
const TICK: Duration = Duration::from_millis(50);

fn start_supervisor(config: &LauncherConfig, sink: EventSink) -> Result<Supervisor, String> {
    let programs = config
        .programs
        .iter()
        .map(|(name, p)| {
            let binds = p
                .binds
                .iter()
                .map(|b| Bind { host: b.host.clone(), guest: b.guest.clone(), writable: b.writable })
                .collect();
            (
                name.clone(),
                ProgramSpec {
                    executable: p.executable.clone(),
                    args: p.args.clone(),
                    binds,
                    network: p.network,
                },
            )
        })
        .collect();
    let supervisor_config = SupervisorConfig {
        limits: config.limits(),
        programs,
        sandbox_tool: None,
        grace: Duration::from_millis(config.grace_ms),
        outbound_queue: OUTBOUND_QUEUE,
        tick: TICK,
        identity_helper: config.identities.as_ref().map(|i| i.helper.clone()),
        // A runtime with projects cannot signal their processes: every
        // program is a cgroup of its own then.
        cgroups: config.identities.is_some(),
        max_active_projects: config.identities.as_ref().map_or(1, |i| i.max_active),
    };
    Supervisor::start(supervisor_config, sink).map_err(|e| e.to_string())
}

fn boot_entry(
    supervisor: &Supervisor,
    entry: &BootEntry,
    ready_timeout_ms: u64,
) -> Result<ProcessId, String> {
    let grants = entry
        .grants
        .iter()
        .map(|g| CapSpec {
            endpoint: g.endpoint.clone(),
            actions: g.actions.clone(),
            resource: g.resource.clone(),
            ttl_ms: None,
            delegable: g.delegable,
            max_calls: None,
        })
        .collect();
    let process = supervisor.boot(entry.program.clone(), grants).map_err(|e| e.to_string())?;
    if let Some(endpoint) = &entry.provides {
        let provided =
            |s: &Supervisor| s.provider_of(endpoint).is_some_and(|(provider, _)| provider == process);
        // Stop waiting as soon as the service is known to have died.
        supervisor
            .wait_until(Duration::from_millis(ready_timeout_ms), |s| provided(s) || !s.is_live(process));
        if !provided(supervisor) {
            let why = if supervisor.is_live(process) { "did not provide it in time" } else { "exited first" };
            supervisor.terminate(process);
            return Err(format!("{} was expected to provide {endpoint} but {why}", entry.program));
        }
    }
    Ok(process)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn load(json: &str) -> Result<LauncherConfig, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json).unwrap();
        LauncherConfig::load(&path)
    }

    const MINIMAL: &str = r#"{
        "programs": {"agent": {"executable": "bin/agent", "binds": [{"host": "out", "guest": "/out", "writable": true}]}},
        "boot": [{"program": "agent", "run_to_completion": true,
                  "grants": [{"endpoint": "fs", "actions": ["read"], "resource": ["work"], "delegable": false}]}]
    }"#;

    #[test]
    fn relative_paths_resolve_against_the_config_file() {
        let config = load(MINIMAL).unwrap();
        let agent = &config.programs[&ProgramName::new("agent").unwrap()];
        assert!(agent.executable.is_absolute() && agent.executable.ends_with("bin/agent"));
        assert!(agent.binds[0].host.is_absolute());
    }

    #[test]
    fn misconfiguration_is_a_startup_error() {
        for (bad, why) in [
            (
                MINIMAL.replace("\"delegable\": false", "\"delegable\": false, \"admin\": true"),
                "unknown field",
            ),
            (MINIMAL.replace("\"program\": \"agent\"", "\"program\": \"ghost\""), "unknown program"),
            (MINIMAL.replace("[\"read\"]", "[]"), "empty actions"),
            (MINIMAL.replace("[\"work\"]", "[\"..\"]"), "traversal in resource"),
            (MINIMAL.replace(", \"delegable\": false", ""), "delegable has no default"),
            (MINIMAL.replace("\"run_to_completion\": true,", ""), "nothing to wait for"),
            (identities(r#""helper": "nucleus-as", "max_active": 96"#), "relative helper"),
            (identities(r#""helper": "/h", "max_active": 0"#), "no active project"),
            (
                MINIMAL.replace(
                    r#""delegable": false}]}]"#,
                    r#""delegable": false}]}, {"program": "agent", "grants": [{"endpoint": "kernel",
                        "actions": ["provide"], "resource": ["endpoint", "fs"], "delegable": false}]}]"#,
                ),
                "an entry granted an endpoint that a later entry provides",
            ),
        ] {
            assert!(load(&bad).is_err(), "{why} should be rejected");
        }
        let helper = r#""helper": "/h", "max_active": 96"#;
        assert!(load(&identities(helper)).unwrap().limits().projects);
        assert!(!load(MINIMAL).unwrap().limits().projects);
    }

    fn identities(fields: &str) -> String {
        MINIMAL.replace("\"programs\":", &format!("\"identities\": {{{fields}}}, \"programs\":"))
    }

    #[test]
    fn a_program_that_cannot_be_confined_stops_the_launcher_before_anything_starts() {
        // `bin/agent` and `out` do not exist, so the sandbox cannot be built.
        let outcome = run(load(MINIMAL).unwrap());
        // Either the program is rejected, or (where bwrap is not installed) the
        // backend itself is; in both cases nothing was started.
        assert!(outcome.is_err_and(|why| why.contains("confine")));
    }
}
