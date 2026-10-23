//! A project's host processes: which program serves which interface, and
//! how one is started. Each is an ordinary service program
//! whose root is the project's folder; it runs as the project, an ordinary
//! process of the projects' Linux user in the project's cgroup, and
//! provides a `dynamic.<n>` endpoint this service allocated and keeps the
//! only invoking handle on.
//!
//! A folder's path is the same for this service and for its hosts: the
//! composition binds the projects directory into this service's sandbox at
//! its real path, and a host, in no sandbox, sees the real one. The
//! projects' shared home is only a path this service hands its hosts; where
//! a host is a sandboxed child of this service (no projects' processes), it
//! must be in this service's sandbox too.

use nucleus_projects_interface as projects;
use nucleus_protocol::{ActionName, ChildBind, ChildProgram, EndpointName, Error};

/// Which interface a host serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// `nucleus-project`: git and files (the window's `projects` and `files`).
    Project,
    Diffs,
    Fs,
    Shell,
    Pdf,
    /// `nucleus-skills`: the skills of the project's repository, and loading
    /// published ones into it (its catalogs from `--skills-catalog`).
    Skills,
    /// `nucleus-shell` as the shell of the project's discovery workers
    /// (`prover-shell`): their workspaces under `runs/` of the home, the
    /// operator's environment, and no IP network (the supervisor starts it
    /// in a network namespace of its own): a result must not come from
    /// looking the proof up.
    ProverShell,
    /// The same with network (`prover-shell-net`), for a run whose task
    /// chose it.
    ProverShellNet,
}

impl Kind {
    /// Every kind: the most hosts one project has running at once.
    pub const ALL: [Kind; 8] = [
        Self::Project,
        Self::Diffs,
        Self::Fs,
        Self::Shell,
        Self::Pdf,
        Self::Skills,
        Self::ProverShell,
        Self::ProverShellNet,
    ];

    fn executable(self) -> &'static str {
        match self {
            Self::Project => "nucleus-project",
            Self::Diffs => "nucleus-diffs",
            Self::Fs => "nucleus-fs",
            Self::Shell | Self::ProverShell | Self::ProverShellNet => "nucleus-shell",
            Self::Pdf => "nucleus-pdf",
            Self::Skills => "nucleus-skills",
        }
    }

    /// The actions of the host's endpoint (from its interface declaration).
    pub fn actions(self, extra: &HostConfig) -> Result<std::collections::BTreeSet<ActionName>, Error> {
        let endpoint = EndpointName::new("host")?;
        let declared = match self {
            Self::Project => nucleus_projects_interface::host_interface(endpoint)?,
            Self::Diffs => nucleus_diffs_interface::interface(endpoint)?,
            Self::Fs => nucleus_fs::interface(endpoint)?,
            Self::Shell => {
                nucleus_shell_interface::host_interface(endpoint, extra.shell_description.clone())?
            }
            Self::ProverShell | Self::ProverShellNet => nucleus_shell_interface::interface(
                endpoint,
                extra.workers_description(self).to_owned(),
                false,
            )?,
            Self::Pdf => nucleus_pdf_interface::interface(endpoint)?,
            Self::Skills => nucleus_skills_interface::interface(endpoint)?,
        };
        Ok(declared.actions.into_keys().collect())
    }

    /// Whether the host has the network. The workers' shell has none, and
    /// the kernel cuts it off; the workers' shell with network has it. Every
    /// other host of a project (`plain`) is an ordinary process of the
    /// projects' user and has the VM's; in a composition without projects' processes,
    /// where a host is a sandboxed child of this service, only those that
    /// need it: a shell, the skills' fetches.
    fn network(self, plain: bool) -> bool {
        match self {
            Self::ProverShell => false,
            Self::Shell | Self::ProverShellNet | Self::Skills => true,
            Self::Project | Self::Diffs | Self::Fs | Self::Pdf => plain,
        }
    }

    /// Whether it is a shell of the project's discovery workers.
    pub fn workers(self) -> bool {
        matches!(self, Self::ProverShell | Self::ProverShellNet)
    }
}

/// The endpoints of the two workers' shells: without network, and with.
pub const PROVER_SHELL: &str = "prover-shell";
pub const PROVER_SHELL_NET: &str = "prover-shell-net";

/// What the operator configured for the hosts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostConfig {
    /// The environment of shells before the project's own
    /// ([`environment`]): the tools' `PATH`, `LEAN_PATH`, …
    pub env: Vec<(String, String)>,
    /// Where the host executables are, in this process's namespace.
    pub programs: String,
    /// The projects' shared home (`HOME` of their shells and git): its path,
    /// for this service and for the hosts alike.
    pub home: String,
    /// Extra arguments of every shell host (its environment, limits,
    /// description).
    pub shell_args: Vec<String>,
    /// The description shell hosts declare, which this service declares too.
    pub shell_description: String,
    /// Extra arguments of every workers' shell host (its environment,
    /// limits, description), and the description each of the two declares:
    /// the same but for what they say of their network.
    pub prover_shell_args: Vec<String>,
    pub prover_shell_description: String,
    pub prover_shell_net_description: String,
    /// The catalogs of published skills the skills hosts read, in order.
    pub skills_catalogs: Vec<String>,
}

impl HostConfig {
    /// What the workers' shell `kind` declares.
    pub fn workers_description(&self, kind: Kind) -> &str {
        match kind {
            Kind::ProverShellNet => &self.prover_shell_net_description,
            _ => &self.prover_shell_description,
        }
    }
}

/// The environment of a project's shells: the operator's,
/// with the projects' shared home `home` as `HOME`, what is installed there
/// first on `PATH` (`~/.local/bin`, `~/.cargo/bin`, `~/.elan/bin`), the XDG
/// directories and npm's global prefix in it, so installing software there
/// just works, for every project alike, and the VM's `/tmp` as the
/// temporary directory.
pub fn environment(operator: &[(String, String)], home: &str) -> Vec<(String, String)> {
    const SET: [&str; 10] = [
        "HOME",
        "PATH",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "NPM_CONFIG_PREFIX",
        "TMPDIR",
        "TMP",
        "TEMP",
    ];
    let path = operator.iter().rev().find(|(name, _)| name == "PATH").map_or("/usr/bin:/bin", |(_, v)| v);
    let mut env: Vec<(String, String)> =
        operator.iter().filter(|(name, _)| !SET.contains(&name.as_str())).cloned().collect();
    let within = |sub: &str| format!("{home}/{sub}");
    env.extend([
        ("HOME".to_owned(), home.to_owned()),
        (
            "PATH".to_owned(),
            format!("{}:{}:{}:{path}", within(".local/bin"), within(".cargo/bin"), within(".elan/bin")),
        ),
        ("XDG_CONFIG_HOME".to_owned(), within(".config")),
        ("XDG_CACHE_HOME".to_owned(), within(".cache")),
        ("XDG_DATA_HOME".to_owned(), within(".local/share")),
        ("XDG_STATE_HOME".to_owned(), within(".local/state")),
        ("NPM_CONFIG_PREFIX".to_owned(), within(".local")),
        ("TMPDIR".to_owned(), "/tmp".to_owned()),
        ("TMP".to_owned(), "/tmp".to_owned()),
        ("TEMP".to_owned(), "/tmp".to_owned()),
    ]);
    env
}

/// The arguments of a host of `kind` for the project's folder at `folder`,
/// all but its endpoint. `plain` says the host runs as the projects' user in
/// no sandbox.
pub fn args(kind: Kind, config: &HostConfig, folder: &str, plain: bool) -> Vec<String> {
    let mut args: Vec<String> = ["--root", folder].map(str::to_owned).to_vec();
    let env = || {
        environment(&config.env, &config.home)
            .into_iter()
            .flat_map(|(n, v)| ["--env".to_owned(), format!("{n}={v}")])
    };
    // What the shell says of its network is what it is started with.
    let network = || {
        let state = if kind.network(plain) { "on" } else { "off" };
        ["--network".to_owned(), state.to_owned()]
    };
    match kind {
        Kind::Project => args.extend(["--tmp", "/tmp", "--home", config.home.as_str()].map(str::to_owned)),
        // Review shows the project's repository, never another repository in
        // its folder; git's own configuration is the shared home's.
        Kind::Diffs => args.extend(
            ["--home", config.home.as_str(), "--repository", projects::REPOSITORY].map(str::to_owned),
        ),
        // A session's shell keeps its background jobs.
        Kind::Shell => {
            args.extend(["--jobs", "on"].map(str::to_owned));
            args.extend(network());
            args.extend(env());
            args.extend(config.shell_args.iter().cloned());
        }
        // A worker's environment is the operator's: no project's installs,
        // and its workspace as its home.
        Kind::ProverShell | Kind::ProverShellNet => {
            args.extend(network());
            args.extend(config.prover_shell_args.iter().cloned());
        }
        Kind::Skills => {
            for catalog in &config.skills_catalogs {
                args.extend(["--catalog".to_owned(), catalog.clone()]);
            }
        }
        Kind::Fs | Kind::Pdf => {}
    }
    args
}

/// The program that serves `kind` for the project's folder at `folder`,
/// providing `endpoint`. With projects' processes (`plain`) it is a process
/// of the projects' user and takes no binds: it sees the folder, the shared
/// home and the VM's tools where they are; the project's host of a project
/// being created (`create`) makes the folder first. Without, it is a
/// sandboxed child of this service, which sees the folder and the shared
/// home at the same paths and whatever the sandbox's base has.
pub fn program(
    kind: Kind,
    config: &HostConfig,
    folder: &str,
    endpoint: &EndpointName,
    (plain, create): (bool, bool),
) -> ChildProgram {
    let mut args = args(kind, config, folder, plain);
    args.extend(["--endpoint".to_owned(), endpoint.as_str().to_owned()]);
    if create {
        args.extend(["--create".to_owned(), "yes".to_owned()]);
    }
    let mut binds = Vec::new();
    if !plain {
        binds.push(ChildBind { path: folder.to_owned(), mount: None, writable: true });
        if !kind.workers() {
            binds.push(ChildBind { path: config.home.clone(), mount: None, writable: true });
        }
    }
    ChildProgram {
        executable: format!("{}/{}", config.programs, kind.executable()),
        args,
        binds,
        network: kind.network(plain),
    }
}
