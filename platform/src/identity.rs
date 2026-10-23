//! A project's processes: ordinary processes of the projects' Linux user, in
//! no sandbox, each project's in a cgroup of its own.
//!
//! The kernel decides which process runs as which project; the supervisor
//! carries it out by running the program through the setuid-root helper
//! `nucleus-as`, which switches to the user its root-owned policy names, in
//! the program's cgroup beneath the project's (`crate::cgroup`), entered
//! before the helper executes. The helper checks its own policy; the
//! supervisor checks only that what it would run is that helper, installed
//! as one, and refuses to start otherwise. A bug in the helper is a root bug;
//! its input is one absolute program and its arguments.
//!
//! Such a process sees the VM as its user does: real paths, `/proc`, `/sys`,
//! `/tmp`, the network. What it may read and write is what the files' owners
//! and modes allow that user, and nothing here adds to it: every project's
//! folder is that user's. It is stopped as every program is, with its
//! cgroup, and with every process of its project by the project's.
//!
//! One user serves every project because what must be kept apart is the
//! runtime (its records, the team's model key, the link to the platform) from
//! the code the projects run. That boundary is Unix ownership and modes alone,
//! which `deploy/vm/activate` checks as `projects` on every activation.
//! Nothing keeps one project from another: any project's command can read,
//! change or delete every project's folder, change the shared home for every
//! project, end every project's processes and trace them as far as the VM's
//! `kernel.yama.ptrace_scope` allows, and so take over any project's agent.
//! Projects also share `/tmp` and the loopback.
//!
//! **One project process is started without network** (`network: false`): the
//! discovery workers' shell, when its run's workers have none. It runs behind
//! [`OFFLINE_BWRAP`] with a network namespace of its own, which holds a
//! loopback interface and nothing else. Bubblewrap needs a user and a mount
//! namespace to make one, so in such a process the VM's files are bound at
//! their own paths, other users' files list as `nobody`'s, setuid programs
//! gain nothing, and `/run` is empty: the sockets of the VM's daemons are
//! there, and a daemon with network would be a way out (the resolver answers
//! DNS queries for any user). The process cannot return to the VM's network:
//! joining a network namespace needs a capability over it that the projects'
//! user does not have. The kernel lets only a `spawn_as` holder start a
//! project's process so, and such a process starts none of its own. What the
//! cut leaves (a Unix socket file outside `/run`, `AF_VSOCK`) lets it talk to
//! a networked process of any project, or leave a program in a project's
//! folder or the shared home that one later runs. `deploy/vm/activate` stops
//! the runtime when anything but systemd listens on vsock; no seccomp filter
//! is added.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::confinement::{Bind, CgroupFiles, ConfinementError, ProgramSpec, bind_lifetime};

/// The bubblewrap that gives the one project process started without network
/// its network namespace. A runtime with identities requires it.
pub const OFFLINE_BWRAP: &str = "/usr/bin/bwrap";

/// Refuses a helper that is not a root-owned setuid file writable only by
/// root, or a host without the bubblewrap the workers' shell without network
/// needs.
pub fn check_helper(helper: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(helper)
        .map_err(|e| format!("the identity helper {}: {e}", helper.display()))?;
    let mode = metadata.mode();
    let why = if !metadata.file_type().is_file() {
        Some("is not a regular file")
    } else if metadata.uid() != 0 {
        Some("is not owned by root")
    } else if mode & 0o4000 == 0 {
        Some("is not setuid")
    } else if mode & 0o022 != 0 {
        Some("can be written by someone other than root")
    } else {
        None
    };
    if let Some(why) = why {
        return Err(format!(
            "the identity helper {} {why}; install it with `install -o root -g nucleus -m 4750`",
            helper.display()
        ));
    }
    if !Path::new(OFFLINE_BWRAP).is_file() {
        return Err(format!(
            "the discovery workers' shell without network runs behind {OFFLINE_BWRAP}, which is missing"
        ));
    }
    Ok(())
}

/// What a project's process sees, for the children it starts: everything,
/// at its real path.
pub(crate) fn whole_vm() -> Vec<Bind> {
    vec![Bind { host: PathBuf::from("/"), guest: PathBuf::from("/"), writable: true }]
}

/// The spec of what a project's process starts, itself a process of the
/// same project (its identity is inherited). Nothing is resolved here: the
/// path is the VM's own, and whether it may be executed is decided when the
/// projects' user executes it, by that user's permissions; the runtime's
/// user could not even look inside a project's folder to say.
pub(crate) fn child_of_project(program: &nucleus_protocol::ChildProgram) -> ProgramSpec {
    ProgramSpec {
        executable: PathBuf::from(&program.executable),
        args: program.args.clone(),
        binds: whole_vm(),
        network: program.network,
    }
}

/// What runs `program` cut off from the network: the arguments in front of
/// it. Everything else is the VM's own, at its own path.
pub(crate) fn offline(program: &Path) -> Command {
    let mut command = Command::new(OFFLINE_BWRAP);
    command.args(["--unshare-net", "--die-with-parent", "--dev-bind", "/", "/", "--tmpfs", "/run", "--"]);
    command.arg(program);
    command
}

/// The command of a project's process: `spec`'s program as the projects'
/// user, through `helper`, in the program's own cgroup. The caller attaches
/// stdio.
pub(crate) fn command(
    helper: &Path,
    spec: &ProgramSpec,
    cgroup: CgroupFiles,
) -> Result<Command, ConfinementError> {
    if !spec.executable.is_absolute() {
        return Err(ConfinementError::InvalidSpec(format!(
            "executable {} is not an absolute path",
            spec.executable.display()
        )));
    }
    let program = if spec.network { Command::new(&spec.executable) } else { offline(&spec.executable) };
    let mut command = Command::new(helper);
    bind_lifetime(&mut command, Some(cgroup));
    command
        .arg("--")
        .arg(program.get_program())
        .args(program.get_args())
        .args(&spec.args)
        .env_clear()
        .current_dir("/");
    Ok(command)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::process::{Output, Stdio};

    /// What the cut of the one project process started without network (the
    /// discovery workers' shell of a run without) is, under the real
    /// bubblewrap, and what it is not. Cut: IP, the VM's own loopback
    /// included (a TCP connection that succeeds without the wrapper fails),
    /// abstract Unix sockets, and the daemons' socket files in `/run`. Not
    /// cut: a Unix socket file anywhere else, and `AF_VSOCK`, which a network
    /// namespace does not hold; the VM's files stay at their own paths
    /// (`/sys`, which a sandbox does not have).
    #[test]
    fn without_network_tcp_and_abstract_sockets_are_cut_and_socket_files_and_vsock_are_not() {
        let tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = tcp.local_addr().unwrap().port().to_string();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("socket");
        let _file_listener = std::os::unix::net::UnixListener::bind(&file).unwrap();
        let name = format!("nucleus-identity-test-{}", std::process::id());
        let _abstract_listener = {
            use std::os::linux::net::SocketAddrExt;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
            std::os::unix::net::UnixListener::bind_addr(&address).unwrap()
        };
        let python = Path::new("/usr/bin/python3");
        let run = |mut command: Command| {
            let output = command
                .args(["-c", PROBE, &port, file.to_str().unwrap(), &name])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let (cut, open) = (run(offline(python)), run(Command::new(python)));
        let line = |text: &str, key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("{key} in {text}"))
                .to_owned()
        };
        for (probe, without, with) in [
            ("tcp ", "refused", "connected"),
            ("abstract ", "refused", "connected"),
            ("file ", "connected", "connected"),
            ("sys ", "yes", "yes"),
        ] {
            assert_eq!((line(&cut, probe).as_str(), line(&open, probe).as_str()), (without, with), "{probe}");
        }
        assert_eq!(line(&cut, "run "), "0", "no daemon's socket to ask instead");
        // Whether this machine has vsock at all, the wrapper changes nothing of it.
        assert_eq!(line(&cut, "vsock "), line(&open, "vsock "));
    }

    const PROBE: &str = r#"import os, socket, sys
def reach(family, address):
    s = socket.socket(family, socket.SOCK_STREAM)
    s.settimeout(1)
    try:
        s.connect(address)
        return 'connected'
    except OSError:
        return 'refused'
print('tcp', reach(socket.AF_INET, ('127.0.0.1', int(sys.argv[1]))))
print('file', reach(socket.AF_UNIX, sys.argv[2]))
print('abstract', reach(socket.AF_UNIX, '\0' + sys.argv[3]))
try:
    socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM).bind((socket.VMADDR_CID_ANY, socket.VMADDR_PORT_ANY))
    print('vsock bound')
except OSError as e:
    print('vsock', e.errno)
print('sys', 'yes' if os.path.isdir('/sys/devices/system/cpu') else 'no')
print('run', len(os.listdir('/run')))
"#;

    // ---- In a Nucleus VM only -----------------------------------------------
    //
    // What Unix permissions carry alone, against the real `projects` user and
    // the real setuid helper. Run as the runtime user in a cgroup it may make
    // children in, in a VM that `activate` has set up:
    //
    //   systemd-run --wait --pipe --collect -p User=nucleus -p Group=nucleus \
    //     -p Delegate=yes <this test binary> --ignored --test-threads=1 in_a_vm
    //
    // Each test makes the project folders it works in as the projects' user
    // and removes them when it ends: they are named `zz-test-…` and may show
    // in a window's list meanwhile.
    //
    // No CI job runs these (CI runs `cargo test --workspace` without
    // `--ignored`); `deploy/vm/activate` checks part of the same as the real
    // `projects` user on every activation.

    const HELPER: &str = "/opt/nucleus/bin/nucleus-as";
    const PROJECTS: &str = "/workspace/nucleus/work/projects";

    /// A project folder made for a test, as the projects' user; removed when
    /// it is dropped.
    struct Project {
        home: String,
    }

    impl Project {
        fn new(tag: &str) -> Self {
            let home = format!("{PROJECTS}/zz-test-{tag}-{}", std::process::id());
            let made = run_as(true, "mkdir -m 0700 \"$1\" && mkdir \"$1/repo\"", &[&home]);
            assert!(made.status.success(), "{}", said(&made));
            Self { home }
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = run_as(true, "rm -rf --one-file-system \"$1\"", &[&self.home]);
        }
    }

    /// Runs `script` with `sh` as the projects' user, through the helper and
    /// in a cgroup of its own, as the supervisor starts a project's process.
    fn run_as(network: bool, script: &str, args: &[&str]) -> Output {
        let root = crate::cgroup::CgroupRoot::open_shared().expect("a delegated cgroup");
        let cgroup = root.create(&format!("test-{}", std::process::id())).unwrap();
        let spec = ProgramSpec {
            executable: "/usr/bin/sh".into(),
            args: ["-c", script, "sh"].into_iter().chain(args.iter().copied()).map(str::to_owned).collect(),
            binds: Vec::new(),
            network,
        };
        let files = (cgroup.procs().unwrap(), cgroup.kill_file().unwrap());
        let output = command(Path::new(HELPER), &spec, files).unwrap().stdin(Stdio::null()).output().unwrap();
        cgroup.kill().unwrap();
        cgroup.remove_when_empty(std::time::Instant::now() + std::time::Duration::from_secs(5)).unwrap();
        output
    }

    fn said(output: &Output) -> String {
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
    }

    /// A project's process is an ordinary process of the projects' user: it
    /// reads and writes every project's folder and the shared home, sees
    /// `/sys`, and is denied the runtime's state, the model key and the
    /// link's configuration, the runs' records, the runtime's processes and
    /// pid 1 in `/proc`, the runtime's environment, opening again a pipe the
    /// runtime gave it (its kernel channel is one), sudo, cron, at and the
    /// helper.
    #[test]
    #[ignore = "needs a Nucleus VM: the setuid helper `activate` installs; run as the runtime user in a delegated cgroup (see the comment above these tests)"]
    fn in_a_vm_a_project_s_process_reaches_every_project_and_nothing_of_the_runtime() {
        let (ours, other) = (Project::new("mine"), Project::new("theirs"));
        let environ = format!("/proc/{}/environ", std::process::id());
        let probe = r#"
            ok() { echo "ok:$1"; }
            [ "$(id -un)" = projects ] && [ "$(id -G)" = "$(id -g)" ] && ok user
            [ "$HOME" = /workspace/nucleus/work/home ] && [ -w "$HOME" ] && ok home
            [ -d /sys/devices/system/cpu ] && [ -r /proc/cpuinfo ] && ok real-paths
            echo mine > "$1/.folders-test" && cat "$1/.folders-test" >/dev/null && ok own-folder
            echo also > "$2/.folders-test" && rm "$2/.folders-test" && ok other-folder
            for kept in /var/lib/nucleus /var/lib/nucleus/secrets /var/lib/nucleus/sessions \
                /var/lib/nucleus/memories /workspace/nucleus/runs /workspace/nucleus/scratch; do
                ls "$kept" >/dev/null 2>&1 && echo "LEAK:$kept"
            done
            cat /var/lib/nucleus/secrets/model/key >/dev/null 2>&1 && echo "LEAK:model-key"
            cat /var/lib/nucleus/secrets/link/platform.json >/dev/null 2>&1 && echo "LEAK:link-token"
            cat "$3" >/dev/null 2>&1 && echo "LEAK:runtime-environ"
            # `/proc` shows a user only its own processes (hidepid).
            [ -e "${3%/environ}" ] && echo "LEAK:runtime-process"
            [ -e /proc/1 ] && echo "LEAK:pid-1"
            [ -e /proc/$$ ] && ok own-process
            # Its own output is a pipe the runtime made, as its kernel channel
            # is: it writes to the descriptor it was given and cannot open the
            # pipe again by name.
            ( : >> /proc/self/fd/1 ) 2>/dev/null && echo "LEAK:kernel-channel"
            sudo -n true 2>/dev/null && echo "LEAK:sudo"
            command -v crontab >/dev/null && crontab - </dev/null 2>/dev/null && echo "LEAK:cron"
            command -v atq >/dev/null && atq >/dev/null 2>&1 && echo "LEAK:at"
            /opt/nucleus/bin/nucleus-as -- /usr/bin/id >/dev/null 2>&1 && echo "LEAK:helper"
            rm -f "$1/.folders-test"
            true
        "#;
        let output = run_as(true, probe, &[&ours.home, &other.home, &environ]);
        let text = said(&output);
        for expected in
            ["ok:user", "ok:home", "ok:real-paths", "ok:own-folder", "ok:other-folder", "ok:own-process"]
        {
            assert!(text.contains(expected), "{expected} missing from: {text}");
        }
        assert!(!text.contains("LEAK:"), "a project's process reached what is the runtime's: {text}");
    }

    /// The runtime stops a project's process, which it cannot signal, with
    /// everything it started, also what left its session: the cgroup. What
    /// it knows of them is the cgroup too: their `/proc` entries are hidden
    /// from the runtime's user.
    #[test]
    #[ignore = "needs a Nucleus VM: the setuid helper `activate` installs; run as the runtime user in a delegated cgroup (see the comment above these tests)"]
    fn in_a_vm_a_project_s_process_is_stopped_with_all_it_started() {
        let root = crate::cgroup::CgroupRoot::open_shared().expect("a delegated cgroup");
        let cgroup = root.create(&format!("test-{}-tree", std::process::id())).unwrap();
        let spec = ProgramSpec {
            executable: "/usr/bin/sh".into(),
            args: vec!["-c".into(), "setsid sleep 300 & exec sleep 300".into()],
            binds: Vec::new(),
            network: true,
        };
        let files = (cgroup.procs().unwrap(), cgroup.kill_file().unwrap());
        let mut child = command(Path::new(HELPER), &spec, files)
            .unwrap()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // The watcher, the program and the process that left its session:
        // the last exists only once the program runs as the projects' user.
        assert!(
            waited(|| cgroup.members().len() == 3),
            "the project's processes did not start: {:?}",
            cgroup.members()
        );
        assert!(
            child.kill().is_err() || child.try_wait().unwrap().is_none(),
            "a signal does not reach another user"
        );
        cgroup.kill().unwrap();
        child.wait().unwrap();
        cgroup.remove_when_empty(std::time::Instant::now() + std::time::Duration::from_secs(5)).unwrap();
    }

    /// Whether `condition` came to hold within five seconds.
    fn waited(condition: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    /// The agent's shell in a VM: the real shell host, outside any sandbox,
    /// as the projects' user. The supervisor's lifetime watcher is its child
    /// and runs as the runtime's user, so the host can neither end it nor
    /// wait for it, and it is not what a command left. A command's output
    /// and exit status come back, what it left running is ended and counted,
    /// also a process `/proc` hides from its own user, and the host is then
    /// alone with its watcher.
    #[test]
    #[ignore = "needs a Nucleus VM: the setuid helper `activate` installs; run as the runtime user in a delegated cgroup (see the comment above these tests)"]
    fn in_a_vm_the_shell_host_answers_a_command_and_ends_what_it_left() {
        use nucleus_protocol::{
            ACTION_PROVIDE, ActionName, DispatchId, EndpointName, FromKernel, Generation, HandleId,
            HandleInfo, KERNEL_ENDPOINT, Payload, ProcessId, ResourcePath, ResponseBody, ToKernel,
            read_frame, write_frame,
        };
        const ENDPOINT: &str = "dynamic.1";
        let project = Project::new("shell");
        let root = crate::cgroup::CgroupRoot::open_shared().expect("a delegated cgroup");
        let cgroup = root.create(&format!("test-{}-shell", std::process::id())).unwrap();
        let spec = ProgramSpec {
            executable: "/opt/nucleus/bin/nucleus-shell".into(),
            args: ["--root", &project.home, "--endpoint", ENDPOINT, "--network", "on"]
                .map(str::to_owned)
                .to_vec(),
            binds: Vec::new(),
            network: true,
        };
        let files = (cgroup.procs().unwrap(), cgroup.kill_file().unwrap());
        let mut command = command(Path::new(HELPER), &spec, files).unwrap();
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let (mut to, mut from) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let provide = HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new(KERNEL_ENDPOINT).unwrap(),
            actions: [ActionName::new(ACTION_PROVIDE).unwrap()].into(),
            resource: ResourcePath::new(["endpoint", ENDPOINT]).unwrap(),
            delegable: false,
            expires_in_ms: None,
            max_calls: None,
        };
        write_frame(&mut to, &FromKernel::Hello { process: ProcessId(2), handles: vec![provide] }).unwrap();
        match read_frame::<_, ToKernel>(&mut from).expect("the host provides its endpoint") {
            ToKernel::Provide { request, .. } => {
                let bound = Ok(ResponseBody::Bound { generation: Generation(1) });
                write_frame(&mut to, &FromKernel::Response { request, result: bound }).unwrap();
            }
            other => panic!("the host began with {other:?}"),
        }
        let mut exec = |n: u64, command: &str| -> serde_json::Value {
            let call = FromKernel::Dispatch {
                dispatch: DispatchId(n),
                endpoint: EndpointName::new(ENDPOINT).unwrap(),
                caller: ProcessId(1),
                action: ActionName::new("exec").unwrap(),
                resource: ResourcePath::new(["repo"]).unwrap(),
                payload: Payload::encode(&serde_json::json!({ "command": command })).unwrap(),
                remaining_ms: 60_000,
            };
            write_frame(&mut to, &call).unwrap();
            match read_frame::<_, ToKernel>(&mut from).expect("the host answers") {
                ToKernel::Reply { result: Ok(payload), .. } => payload.decode().unwrap(),
                other => panic!("the host answered {other:?}"),
            }
        };
        let answer = exec(1, "sleep 300 >/dev/null 2>&1 & echo out; echo err >&2; exit 3");
        assert_eq!(
            (&answer["output"], &answer["exit_code"], &answer["stopped_leftovers"]),
            (&serde_json::json!("out\nerr\n"), &serde_json::json!(3), &serde_json::json!(1)),
            "{answer}"
        );
        // PR_SET_DUMPABLE (4) off: no entry in a `/proc` mounted with hidepid.
        let undumpable = "python3 -c 'import ctypes, time; ctypes.CDLL(None).prctl(4, 0); time.sleep(300)' \
                          >/dev/null 2>&1 & sleep 1; echo ok";
        let answer = exec(2, undumpable);
        assert_eq!(
            (&answer["output"], &answer["stopped_leftovers"]),
            (&serde_json::json!("ok\n"), &serde_json::json!(1)),
            "{answer}"
        );
        assert!(waited(|| cgroup.members().len() == 2), "left in the host's cgroup: {:?}", cgroup.members());
        cgroup.kill().unwrap();
        child.wait().unwrap();
        cgroup.remove_when_empty(std::time::Instant::now() + std::time::Duration::from_secs(5)).unwrap();
    }

    /// The discovery workers' network setting for a project's process:
    /// started without network it has no IP, not even the VM's loopback, and
    /// no daemon's socket in `/run`; started with, it connects.
    #[test]
    #[ignore = "needs a Nucleus VM: the setuid helper `activate` installs; run as the runtime user in a delegated cgroup (see the comment above these tests)"]
    fn in_a_vm_a_project_s_process_has_the_ip_network_only_when_started_with_it() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let probe = format!("exec /usr/bin/python3 -c \"$1\" {port} /nonexistent none");
        let cut = said(&run_as(false, &probe, &[PROBE]));
        assert!(cut.contains("tcp refused\n") && cut.contains("run 0\n"), "started without network: {cut}");
        let open = said(&run_as(true, &probe, &[PROBE]));
        assert!(open.contains("tcp connected\n"), "started with network: {open}");
    }
}
