//! Confinement. Every program of the runtime's own runs in a verified
//! bubblewrap sandbox (Linux): user, PID, network, IPC, UTS and cgroup
//! namespaces and a private mount tree. Nucleus runs only in the user's Linux
//! VM, so there is no other backend. A failed probe is fatal, never a
//! fallback. A project's process is the one thing that is not sandboxed: it
//! runs as the projects' Linux user (module `identity`).
//!
//! What the sandbox provides: no ambient environment; no filesystem access
//! beyond the program's own executable, all of `/usr` (read-only), the binds
//! the operator declared (read-only unless marked writable) and, with the
//! network, the name-resolution and CA files; no network unless
//! [`ProgramSpec::network`] is set; and no descendants that outlive the
//! process.
//!
//! **Paths.** A program is configured with *guest* paths (`/data`,
//! `/secrets/key`) and each bind says which host path stands behind one.
//!
//! **Network** is all-or-nothing: the sandbox filters no egress.
//!
//! The sandbox does not limit CPU, memory or disk, or filter system calls.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A host directory or file made visible inside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    pub host: PathBuf,
    /// Absolute path inside the sandbox.
    pub guest: PathBuf,
    pub writable: bool,
}

/// How to start one configured program.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub binds: Vec<Bind>,
    /// Operator grant of unrestricted host network access. Deny by default.
    pub network: bool,
}

/// Why confinement could not be established.
#[derive(Debug)]
pub enum ConfinementError {
    /// The backend is missing or cannot create the required namespaces.
    Unavailable(String),
    /// A program specification cannot be confined as written.
    InvalidSpec(String),
}

impl fmt::Display for ConfinementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "confinement backend unavailable: {why}"),
            Self::InvalidSpec(why) => write!(f, "program cannot be confined: {why}"),
        }
    }
}

impl std::error::Error for ConfinementError {}

/// Where the program's executable appears inside a remapping sandbox.
const GUEST_EXECUTABLE: &str = "/nucleus/program";

// Name resolution and public system trust for ordinary network clients. Keep
// these narrower than /etc/ssl or /etc/pki, which can contain private keys.
// Debian/Ubuntu use /etc/ssl/certs; Fedora's bundle links into ca-trust/extracted.
const NETWORK_SYSTEM_PATHS: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/ssl/certs",
    "/etc/ssl/cert.pem",
    "/etc/pki/tls/certs",
    "/etc/pki/tls/cert.pem",
    "/etc/pki/ca-trust/extracted",
];

/// A program's own cgroup: its `cgroup.procs`, which the child enters before
/// it executes anything, and its `cgroup.kill`, which the lifetime watcher
/// writes.
pub type CgroupFiles = (std::os::fd::OwnedFd, std::os::fd::OwnedFd);

/// Builds a confined program command.
pub trait Confinement: Send + Sync {
    /// Builds the command for `spec`. The caller attaches stdio.
    fn command(&self, spec: &ProgramSpec) -> Result<Command, ConfinementError> {
        self.launch(spec, None)
    }
    /// Builds the command for `spec`, started in `cgroup` when given.
    fn launch(&self, spec: &ProgramSpec, cgroup: Option<CgroupFiles>) -> Result<Command, ConfinementError>;
}

/// The bubblewrap backend, verified before use. `tool` overrides where
/// `bwrap` is looked up.
pub fn detect(tool: Option<&Path>) -> Result<Box<dyn Confinement>, ConfinementError> {
    Ok(Box::new(Bubblewrap::probe(tool.unwrap_or(Path::new("bwrap")))?))
}

fn validate(spec: &ProgramSpec) -> Result<(), ConfinementError> {
    let invalid = |why: String| Err(ConfinementError::InvalidSpec(why));
    if !spec.executable.is_absolute() || !spec.executable.is_file() {
        return invalid(format!("executable {} is not an absolute file path", spec.executable.display()));
    }
    for bind in &spec.binds {
        // Guest paths are always written POSIX-style, on every host.
        if !bind.host.is_absolute() || !bind.guest.to_string_lossy().starts_with('/') {
            return invalid(format!("bind {} must use absolute paths", bind.host.display()));
        }
        if !bind.host.exists() {
            return invalid(format!("bind source {} does not exist", bind.host.display()));
        }
        if shadows_base(&bind.guest) {
            return invalid(format!("bind target {} would replace a base mount", bind.guest.display()));
        }
    }
    Ok(())
}

fn shadows_base(guest: &Path) -> bool {
    let guest = guest.to_string_lossy();
    let under = |base: &str| guest == base || guest.starts_with(&format!("{base}/"));
    guest == "/"
        || ["/usr", "/proc", "/dev", "/bin", "/lib", "/lib64", "/nucleus", "/etc/resolv.conf"]
            .iter()
            .any(|b| under(b))
        || NETWORK_SYSTEM_PATHS.iter().any(|base| under(base))
}

/// Resolves a [`nucleus_protocol::ChildProgram`] (guest paths in the parent's
/// namespace) into a [`ProgramSpec`] with host paths, for a child a process
/// starts with `SpawnExecutable`. Each guest path is mapped through the
/// parent's binds and canonicalized; a path (or a symlink) that resolves
/// outside the parent's bind is refused, so the child can reach nothing its
/// parent could not. The kernel has already checked the namespace is a subset
/// in the abstract; this catches what only the real filesystem can show.
pub fn resolve_child(
    parent: &ProgramSpec,
    child: &nucleus_protocol::ChildProgram,
) -> Result<ProgramSpec, ConfinementError> {
    let invalid = |why: String| ConfinementError::InvalidSpec(why);
    // The host path `guest` maps to, canonicalized and confirmed to stay
    // beneath its bind's canonical host root.
    let map = |guest: &str| -> Result<PathBuf, ConfinementError> {
        let (bind, rest) = bind_of(parent, guest)?;
        let mut host = bind.host.clone();
        host.extend(rest.split('/').filter(|s| !s.is_empty()));
        let root = canonical(&bind.host)?;
        let resolved = canonical(&host)?;
        if !resolved.starts_with(&root) {
            return Err(invalid(format!("{guest} resolves outside {}", bind.host.display())));
        }
        Ok(resolved)
    };

    let mut binds = Vec::with_capacity(child.binds.len());
    for bind in &child.binds {
        let host = map(&bind.path)?;
        let guest = bind.mount.clone().unwrap_or_else(|| bind.path.clone());
        binds.push(Bind { host, guest: PathBuf::from(guest), writable: bind.writable });
    }
    Ok(ProgramSpec {
        executable: map(&child.executable)?,
        args: child.args.clone(),
        binds,
        network: child.network,
    })
}

/// The bind of `parent` whose guest path is a prefix of `guest`, longest
/// first, and the rest of `guest` beneath it.
fn bind_of<'a>(parent: &'a ProgramSpec, guest: &str) -> Result<(&'a Bind, String), ConfinementError> {
    parent
        .binds
        .iter()
        .filter_map(|bind| {
            let g = bind.guest.to_string_lossy().trim_end_matches('/').to_owned();
            let rest = guest.strip_prefix(&g)?;
            (rest.is_empty() || rest.starts_with('/')).then_some((g.len(), bind, rest.to_owned()))
        })
        .max_by_key(|(len, _, _)| *len)
        .map(|(_, bind, rest)| (bind, rest))
        .ok_or_else(|| {
            ConfinementError::InvalidSpec(format!(
                "{guest} is not beneath anything the spawning process can see"
            ))
        })
}

/// A verified bubblewrap installation (Linux).
#[derive(Clone, Debug)]
pub struct Bubblewrap {
    bwrap: PathBuf,
}

impl Bubblewrap {
    /// Runs a trivial command under the exact isolation flags used for real
    /// programs. Failure here is fatal by design.
    pub fn probe(bwrap: impl Into<PathBuf>) -> Result<Self, ConfinementError> {
        let this = Self { bwrap: bwrap.into() };
        let mut command = this.base_command();
        command.args(["--", "/usr/bin/true"]).stdin(Stdio::null()).stdout(Stdio::null());
        let output = command.output().map_err(|e| {
            ConfinementError::Unavailable(format!("cannot run {}: {e}", this.bwrap.display()))
        })?;
        if output.status.success() {
            Ok(this)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ConfinementError::Unavailable(format!("probe failed: {}", stderr.trim())))
        }
    }

    fn base_command(&self) -> Command {
        self.started(None)
    }

    /// The command that starts a sandbox. The child enters its cgroup first,
    /// then forks the lifetime watcher, so the watcher is in the cgroup and
    /// nothing of the program starts outside it. The environment is cleared
    /// twice, for bubblewrap itself (`env_clear`) and for the program
    /// (`--clearenv`); `/usr` is bound read-only with `/bin`, `/lib` and
    /// `/lib64` as links into it, and `/proc`, `/dev` and `/tmp` are the
    /// sandbox's own.
    fn started(&self, cgroup: Option<CgroupFiles>) -> Command {
        let mut command = Command::new(&self.bwrap);
        bind_lifetime(&mut command, cgroup);
        command.env_clear().args([
            "--unshare-all",
            "--die-with-parent",
            "--clearenv",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]);
        command
    }
}

/// Binds the program `command` starts to this process's lifetime: in its own
/// cgroup when given one, else as a process group.
pub(crate) fn bind_lifetime(command: &mut Command, cgroup: Option<CgroupFiles>) {
    match cgroup {
        None => nucleus_preexec::bind_parent_lifetime(command),
        Some((procs, kill)) => {
            nucleus_preexec::enter_cgroup(command, procs);
            nucleus_preexec::bind_parent_lifetime_in_cgroup(command, kill);
        }
    }
}

impl Confinement for Bubblewrap {
    fn launch(&self, spec: &ProgramSpec, cgroup: Option<CgroupFiles>) -> Result<Command, ConfinementError> {
        validate(spec)?;
        let mut command = self.started(cgroup);
        bind_opened_sources(&mut command, spec)?;
        if spec.network {
            command.arg("--share-net");
            // A shared network without DNS/NSS configuration or the public CA
            // store cannot serve normal curl/Python TLS clients. Distribution-
            // specific paths are optional; no private key directory is exposed.
            for path in NETWORK_SYSTEM_PATHS {
                network_file(&mut command, spec, path);
            }
        }
        command.arg("--").arg(GUEST_EXECUTABLE).args(&spec.args);
        Ok(command)
    }
}

/// Makes the network system file `path` visible to `spec`'s program. A
/// program that already sees it through one of its binds (one that sees all
/// of the host's `/etc`) keeps that view: bubblewrap cannot mount over a file
/// in a read-only bind, and the file is the same. Where what it sees there is
/// a link out of that bind (`resolv.conf` on systemd-resolved systems), the
/// link's target is bound where the link points.
fn network_file(command: &mut Command, spec: &ProgramSpec, path: &str) {
    let shown = spec.binds.iter().find_map(|bind| {
        let rest = Path::new(path).strip_prefix(&bind.guest).ok()?;
        Some((bind, bind.host.join(rest)))
    });
    let Some((bind, host)) = shown else {
        command.args(["--ro-bind-try", path, path]);
        return;
    };
    if host.is_symlink()
        && let Ok(target) = host.canonicalize()
        && !target.starts_with(&bind.host)
        && !shadows_base(&target)
    {
        command.arg("--ro-bind-try").arg(&target).arg(&target);
    }
}

/// Opens every host source once, refusing any symlink in its path, and hands
/// bubblewrap the descriptors instead of names it would resolve again. Paths
/// must already be resolved: supervisors resolve operator paths at start, and
/// [`resolve_child`] checks a child's. A component replaced by a symlink after
/// that check fails here instead of mounting whatever the link names.
fn bind_opened_sources(command: &mut Command, spec: &ProgramSpec) -> Result<(), ConfinementError> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
    use std::os::fd::AsRawFd;
    let sources = std::iter::once((&spec.executable, Path::new(GUEST_EXECUTABLE), false))
        .chain(spec.binds.iter().map(|bind| (&bind.host, bind.guest.as_path(), bind.writable)));
    let mut opened = Vec::with_capacity(spec.binds.len() + 1);
    for (host, guest, writable) in sources {
        let fd = openat2(CWD, host, OFlags::PATH | OFlags::CLOEXEC, Mode::empty(), ResolveFlags::NO_SYMLINKS)
            .map_err(|e| {
                ConfinementError::InvalidSpec(format!(
                    "cannot open {} without following links: {e}",
                    host.display()
                ))
            })?;
        command
            .arg(if writable { "--bind-fd" } else { "--ro-bind-fd" })
            .arg(fd.as_raw_fd().to_string())
            .arg(guest);
        opened.push(fd);
    }
    // Bubblewrap closes each descriptor after mounting it, before the program runs.
    nucleus_preexec::inherit_descriptors(command, opened);
    Ok(())
}

fn canonical(path: &Path) -> Result<PathBuf, ConfinementError> {
    path.canonicalize()
        .map_err(|e| ConfinementError::InvalidSpec(format!("cannot resolve {}: {e}", path.display())))
}

pub(crate) fn with_resolved_paths(spec: &ProgramSpec) -> Result<ProgramSpec, ConfinementError> {
    let mut resolved = spec.clone();
    resolved.executable = canonical(&spec.executable)?;
    for bind in &mut resolved.binds {
        bind.host = canonical(&bind.host)?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(host: &str, guest: &str, writable: bool) -> Bind {
        Bind { host: host.into(), guest: guest.into(), writable }
    }

    mod resolve {
        #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
        use super::*;
        use nucleus_protocol::{ChildBind, ChildProgram};

        fn child(executable: &str, binds: Vec<ChildBind>, network: bool) -> ChildProgram {
            ChildProgram { executable: executable.to_owned(), args: vec![], binds, network }
        }

        #[test]
        fn resolves_guest_paths_to_host_paths_and_refuses_a_symlink_escape() {
            let dir = tempfile::tempdir().unwrap();
            let work = dir.path().join("work");
            std::fs::create_dir_all(&work).unwrap();
            std::fs::write(work.join("tool"), "#!/bin/true\n").unwrap();
            // A parent that sees the real `work` directory as guest `/work`.
            let parent = ProgramSpec {
                executable: work.join("tool"),
                args: vec![],
                binds: vec![Bind { host: work.clone(), guest: "/work".into(), writable: true }],
                network: false,
            };

            // A child within the parent's namespace resolves to real host paths.
            let ok = resolve_child(
                &parent,
                &child(
                    "/work/tool",
                    vec![ChildBind { path: "/work".to_owned(), mount: None, writable: true }],
                    false,
                ),
            )
            .unwrap();
            assert_eq!(ok.executable, work.join("tool").canonicalize().unwrap());
            assert_eq!(ok.binds[0].host, work.canonicalize().unwrap());

            // A symlink under the parent's bind that points outside is refused,
            // because the resolved host path escapes the bind root.
            let outside = dir.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("evil"), "x").unwrap();
            std::os::unix::fs::symlink(&outside, work.join("link")).unwrap();
            let escaped = resolve_child(&parent, &child("/work/link/evil", vec![], false));
            assert!(matches!(escaped, Err(ConfinementError::InvalidSpec(_))), "{escaped:?}");

            // A guest path the parent cannot see is refused.
            assert!(resolve_child(&parent, &child("/etc/passwd", vec![], false)).is_err());
        }

        #[test]
        fn a_child_bind_swapped_for_a_symlink_after_the_check_is_not_mounted() {
            let dir = tempfile::tempdir().unwrap();
            let (work, outside) = (dir.path().join("work"), dir.path().join("outside"));
            std::fs::create_dir_all(work.join("sub")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret"), "outside").unwrap();
            let sh = Path::new("/bin/sh").canonicalize().unwrap();
            let parent = ProgramSpec {
                executable: sh.clone(),
                args: vec![],
                binds: vec![
                    Bind { host: work.clone(), guest: "/work".into(), writable: true },
                    Bind { host: sh, guest: "/sh".into(), writable: false },
                ],
                network: false,
            };
            let bind =
                ChildBind { path: "/work/sub".to_owned(), mount: Some("/data".to_owned()), writable: true };
            let mut spec = resolve_child(&parent, &child("/sh", vec![bind], false)).unwrap();
            spec.args = vec!["-c".into(), "cat /data/secret".into()];
            // The parent swaps its checked directory for a link out of its bind before launch.
            std::fs::remove_dir(work.join("sub")).unwrap();
            std::os::unix::fs::symlink("../outside", work.join("sub")).unwrap();
            if let Ok(mut command) = Bubblewrap::probe("bwrap").unwrap().command(&spec) {
                let output = command.stderr(Stdio::null()).output().unwrap();
                assert!(!String::from_utf8_lossy(&output.stdout).contains("outside"), "the link was mounted");
            }
        }
    }

    mod bubblewrap {
        use super::*;

        #[test]
        fn a_missing_backend_is_an_error_not_a_fallback() {
            assert!(matches!(Bubblewrap::probe("/nonexistent/bwrap"), Err(ConfinementError::Unavailable(_))));
            assert!(matches!(
                detect(Some(Path::new("/nonexistent/bwrap"))),
                Err(ConfinementError::Unavailable(_))
            ));
        }

        #[test]
        fn a_backend_that_cannot_isolate_is_rejected() {
            // `false` runs but does not establish anything.
            assert!(matches!(Bubblewrap::probe("/usr/bin/false"), Err(ConfinementError::Unavailable(_))));
        }

        #[test]
        fn binds_cannot_replace_the_base_filesystem() -> Result<(), ConfinementError> {
            let bwrap = Bubblewrap::probe("bwrap")?;
            let exe = std::env::current_exe().map_err(|e| ConfinementError::InvalidSpec(e.to_string()))?;
            for guest in [
                "/",
                "/usr/lib",
                "/proc",
                "/nucleus/program",
                "relative",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/ssl/certs",
                "/etc/pki/ca-trust/extracted",
            ] {
                let spec = ProgramSpec {
                    executable: exe.clone(),
                    args: vec![],
                    binds: vec![bind("/tmp", guest, true)],
                    network: false,
                };
                assert!(matches!(bwrap.command(&spec), Err(ConfinementError::InvalidSpec(_))), "{guest}");
            }
            Ok(())
        }

        #[test]
        fn the_network_is_shared_only_when_the_operator_says_so() -> Result<(), ConfinementError> {
            let bwrap = Bubblewrap::probe("bwrap")?;
            let exe = std::env::current_exe().map_err(|e| ConfinementError::InvalidSpec(e.to_string()))?;
            let shares = |network: bool| -> Result<bool, ConfinementError> {
                let spec = ProgramSpec { executable: exe.clone(), args: vec![], binds: vec![], network };
                Ok(bwrap.command(&spec)?.get_args().any(|arg| arg == "--share-net"))
            };
            assert!(!shares(false)?);
            assert!(shares(true)?);
            Ok(())
        }

        #[test]
        fn networked_sandbox_has_public_system_trust_and_name_resolution_files()
        -> Result<(), Box<dyn std::error::Error>> {
            let script = r#"import json, os, socket, ssl
assert os.path.isfile('/etc/resolv.conf')
assert os.path.isfile('/etc/hosts')
assert os.path.isfile('/etc/nsswitch.conf')
assert socket.getaddrinfo('localhost', 443)
paths = ssl.get_default_verify_paths()
assert paths.cafile or paths.capath, paths
context = ssl.create_default_context()
assert context.get_ca_certs(), context.cert_store_stats()
for path in [paths.cafile, paths.capath]:
    if path:
        assert os.statvfs(path).f_flag & os.ST_RDONLY, path
assert not os.path.lexists('/etc/ssl/private')
assert not os.path.lexists('/etc/pki/tls/private')
print(json.dumps({'ca_count': len(context.get_ca_certs()), 'cafile': paths.cafile, 'capath': paths.capath}))
"#;
            let spec = ProgramSpec {
                executable: "/usr/bin/python3".into(),
                args: vec!["-c".into(), script.into()],
                binds: vec![],
                network: true,
            };
            let output = Bubblewrap::probe("bwrap")?.command(&with_resolved_paths(&spec)?)?.output()?;
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let observed: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert!(observed["ca_count"].as_u64().is_some_and(|count| count > 0));
            Ok(())
        }

        /// A program that binds the host's whole `/etc` (a plugin may ask for
        /// it): users' names, the alternatives behind
        /// `/usr/bin`'s links and the loader's cache are there, and with the
        /// network it still resolves names, its `resolv.conf` being a link out
        /// of `/etc` on systemd-resolved systems.
        #[test]
        fn a_program_that_sees_etc_sees_all_of_it_and_still_resolves_names()
        -> Result<(), Box<dyn std::error::Error>> {
            let script = r#"import os, pwd, socket, sys
assert pwd.getpwuid(0).pw_name == 'root'
assert os.path.isfile('/etc/ld.so.cache')
for link in sys.argv[2:]:
    assert os.path.exists(link), link
if sys.argv[1] == 'true':
    assert open('/etc/resolv.conf').read() is not None
    assert socket.getaddrinfo('localhost', 443)
"#;
            // The commands whose alternative is a program of the system, as
            // the host resolves them (`awk` on Debian and Ubuntu).
            let mut alternatives = Vec::new();
            for entry in std::fs::read_dir("/usr/bin")? {
                let link = entry?.path();
                let into_alternatives =
                    std::fs::read_link(&link).is_ok_and(|to| to.starts_with("/etc/alternatives"));
                if into_alternatives && link.canonicalize().is_ok_and(|to| to.starts_with("/usr")) {
                    alternatives.push(link.to_string_lossy().into_owned());
                }
            }
            let bwrap = Bubblewrap::probe("bwrap")?;
            for network in [false, true] {
                let mut args = vec!["-c".into(), script.into(), network.to_string()];
                args.extend(alternatives.iter().cloned());
                let spec = ProgramSpec {
                    executable: "/usr/bin/python3".into(),
                    args,
                    binds: vec![bind("/etc", "/etc", false)],
                    network,
                };
                let output = bwrap.command(&with_resolved_paths(&spec)?)?.output()?;
                assert!(
                    output.status.success(),
                    "network={network}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        }

        #[test]
        fn a_real_sandbox_connects_only_with_network_authority() -> Result<(), Box<dyn std::error::Error>> {
            // A local listener needs no external service, credentials or DNS.
            // Keeping it open is enough for TCP connect; no server worker can hang.
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
            let port = listener.local_addr()?.port();
            let script = r#"import socket, sys
try:
    with socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=1):
        connected = True
except OSError:
    connected = False
assert connected == (sys.argv[2] == 'true'), connected
"#;
            let bwrap = Bubblewrap::probe("bwrap")?;
            for network in [false, true] {
                let spec = ProgramSpec {
                    executable: "/usr/bin/python3".into(),
                    args: vec!["-c".into(), script.into(), port.to_string(), network.to_string()],
                    binds: vec![],
                    network,
                };
                let output = bwrap.command(&with_resolved_paths(&spec)?)?.output()?;
                assert!(
                    output.status.success(),
                    "network={network}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        }
    }
}
