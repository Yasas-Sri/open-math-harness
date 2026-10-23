//! `nucleus-as`: the setuid-root program of the Nucleus bundle.
//!
//! ```text
//! nucleus-as -- <program> [arguments]      the runtime's user only
//! ```
//!
//! It has one job: start a project's process as the projects' Linux user.
//! The supervisor runs it for a process the kernel started as a project,
//! after placing the new process in the project's cgroup (before anything
//! executes, while it is still the runtime's), and names `/usr/bin/bwrap`
//! with its arguments as the program when the process must run without
//! network. Installed mode 4750 `root:nucleus`, so only the runtime's group
//! (and root) can run it. It trusts nothing it did not check. Its policy
//! comes from [`CONFIG`], read only when that file, and every directory
//! above it, is owned by root and writable by nobody else; the user comes
//! from `/etc/passwd`, checked the same way. Any failure exits with status
//! 126 and a line on standard error.
//!
//! Its real uid must be the runtime user the policy names. The user it
//! becomes is the one the policy names, which must be exactly one passwd
//! entry, with a uid nobody else has, neither 0 nor the runtime user's, and
//! a primary group that is neither root's nor the runtime user's. It sets
//! the groups to that primary group alone, then the real, effective and
//! saved gid and uid, all nonzero after euid 0, so the kernel clears every
//! capability; it checks that it did, and clears the inheritable and ambient
//! sets too. It sets umask 0077, clears the environment but `HOME` (the
//! user's home, from passwd) and executes the program, an absolute path,
//! keeping inherited descriptors (the kernel channel). What the program is,
//! is the supervisor's choice: the runtime user, who alone may run this
//! helper, may run anything as the projects' user, and nothing as anyone
//! else. It is single-threaded, so the per-thread credential calls it makes
//! are the process's.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// The root-owned policy.
pub const CONFIG: &str = "/etc/nucleus/nucleus-as.conf";
pub const PASSWD: &str = "/etc/passwd";
/// Largest system file it reads.
const MAX_FILE_BYTES: u64 = 16 << 20;

/// What the root-owned configuration says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The only user allowed to run the helper, besides root: the runtime's.
    pub runtime_user: String,
    /// The user every project's process runs as.
    pub user: String,
}

impl Policy {
    /// Parses `key=value` lines; `#` starts a comment. Both keys are
    /// required once, and no other key is accepted.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut values = std::collections::BTreeMap::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (key, value) = line.split_once('=').ok_or_else(|| format!("not key=value: {line:?}"))?;
            if values.insert(key.trim().to_owned(), value.trim().to_owned()).is_some() {
                return Err(format!("{key} is given twice"));
            }
        }
        let mut take = |key: &str| values.remove(key).ok_or_else(|| format!("{key} is missing"));
        let (runtime_user, user) = (take("runtime_user")?, take("user")?);
        if let Some(key) = values.keys().next() {
            return Err(format!("unknown key {key}"));
        }
        if runtime_user.is_empty() || user.is_empty() || runtime_user == user {
            return Err("runtime_user and user must be two users".into());
        }
        Ok(Self { runtime_user, user })
    }
}

/// One line of `/etc/passwd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

/// A plain decimal number: digits only, no sign, no leading zero.
fn canonical(text: &str) -> Option<u32> {
    let number: u32 = text.parse().ok()?;
    (number.to_string() == text).then_some(number)
}

/// The users of a passwd file; lines that do not parse are skipped (they
/// name nobody the policy can accept).
pub fn users(text: &str) -> Vec<User> {
    text.lines()
        .filter_map(|line| match line.split(':').collect::<Vec<_>>().as_slice() {
            [name, _, uid, gid, _, home, ..] => Some(User {
                name: (*name).to_owned(),
                uid: canonical(uid)?,
                gid: canonical(gid)?,
                home: (*home).to_owned(),
            }),
            _ => None,
        })
        .collect()
}

/// Who to become.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub uid: u32,
    /// Its primary group, and its only group.
    pub gid: u32,
    pub home: String,
}

/// Decides whether `caller` may become the projects' user, from the policy
/// and the system's users. Pure, so every refusal is tested.
pub fn decide(policy: &Policy, users: &[User], caller: u32) -> Result<Target, String> {
    let one = |name: &str| {
        let mut named = users.iter().filter(|u| u.name == name);
        match (named.next(), named.next()) {
            (Some(user), None) => Ok(user),
            _ => Err(format!("there must be exactly one user {name}")),
        }
    };
    let runtime = one(&policy.runtime_user)?;
    if caller != runtime.uid {
        return Err(format!("only {} may run this program", policy.runtime_user));
    }
    let user = one(&policy.user)?;
    if user.uid == 0 || user.uid == runtime.uid || users.iter().filter(|u| u.uid == user.uid).count() != 1 {
        return Err(format!(
            "{}'s uid {} must be its own and not root's or the runtime's",
            user.name, user.uid
        ));
    }
    if user.gid == 0 || user.gid == runtime.gid {
        return Err(format!("{}'s group {} must be neither root's nor the runtime's", user.name, user.gid));
    }
    if !Path::new(&user.home).is_absolute() {
        return Err(format!("{}'s home {:?} is not an absolute path", user.name, user.home));
    }
    Ok(Target { uid: user.uid, gid: user.gid, home: user.home.clone() })
}

/// Whether a file or directory may be trusted for the policy: owned by root,
/// writable by nobody else, and of the expected type, never a link.
pub fn trusted(uid: u32, mode: u32, directory: bool) -> Result<(), &'static str> {
    use rustix::fs::FileType;
    let kind = FileType::from_raw_mode(mode);
    let right_kind = if directory { kind == FileType::Directory } else { kind == FileType::RegularFile };
    if !right_kind {
        return Err("it is not of the expected type");
    }
    if uid != 0 {
        return Err("it is not owned by root");
    }
    if mode & 0o022 != 0 {
        return Err("someone other than root can write it");
    }
    Ok(())
}

/// Refuses unless `dir` and every directory above it is root's, writable by
/// nobody else, and no link.
fn check_trusted_dirs(dir: &Path) -> Result<(), String> {
    use rustix::fs::{AtFlags, CWD};
    let mut at = Some(dir);
    while let Some(d) = at {
        let path = if d.as_os_str().is_empty() { Path::new("/") } else { d };
        let fail = |why: String| format!("{}: {why}", path.display());
        let stat =
            rustix::fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| fail(e.to_string()))?;
        trusted(stat.st_uid, stat.st_mode, true).map_err(|why| fail(why.to_owned()))?;
        at = d.parent();
    }
    Ok(())
}

/// Reads a root-owned file that nobody else can change, nor any directory
/// above it. Opened without following a link; checked on what was opened.
fn read_trusted(path: &str) -> Result<String, String> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    let fail = |why: String| format!("{path}: {why}");
    if let Some(parent) = Path::new(path).parent() {
        check_trusted_dirs(parent)?;
    }
    let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| fail(e.to_string()))?;
    let stat = rustix::fs::fstat(&fd).map_err(|e| fail(e.to_string()))?;
    trusted(stat.st_uid, stat.st_mode, false).map_err(|why| fail(why.to_owned()))?;
    let mut text = String::new();
    std::fs::File::from(fd)
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| fail(e.to_string()))?;
    Ok(text)
}

/// Becomes `target` for good and checks that no capability is left.
fn become_user(target: &Target) -> Result<(), String> {
    use rustix::process::{Gid, Uid, getegid, geteuid, getgid, getuid};
    use rustix::thread::{
        CapabilitySet, CapabilitySets, clear_ambient_capability_set, get_keep_capabilities, set_capabilities,
        set_thread_groups, set_thread_res_gid, set_thread_res_uid,
    };
    let (uid, gid) = (Uid::from_raw(target.uid), Gid::from_raw(target.gid));
    set_thread_groups(&[gid]).map_err(|e| format!("setgroups: {e}"))?;
    set_thread_res_gid(gid, gid, gid).map_err(|e| format!("setresgid: {e}"))?;
    set_thread_res_uid(uid, uid, uid).map_err(|e| format!("setresuid: {e}"))?;
    if getuid() != uid || geteuid() != uid || getgid() != gid || getegid() != gid {
        return Err("the uid or gid did not change".into());
    }
    let held: Vec<u32> =
        rustix::process::getgroups().map_err(|e| e.to_string())?.iter().map(|g| g.as_raw()).collect();
    if held != [target.gid] {
        return Err("the supplementary groups did not change".into());
    }
    let none = CapabilitySet::empty();
    let sets = rustix::thread::capabilities(None).map_err(|e| format!("capget: {e}"))?;
    if sets.effective != none || sets.permitted != none {
        return Err("capabilities survived the uid change".into());
    }
    set_capabilities(None, CapabilitySets { effective: none, permitted: none, inheritable: none })
        .map_err(|e| format!("capset: {e}"))?;
    clear_ambient_capability_set().map_err(|e| format!("ambient capabilities: {e}"))?;
    if get_keep_capabilities().map_err(|e| e.to_string())? {
        return Err("capabilities would be kept".into());
    }
    Ok(())
}

const USAGE: &str = "usage: nucleus-as -- <program> [arguments]";

/// Parses the command line: the program, an absolute path never looked up
/// on a `PATH`, and its arguments.
pub fn request(args: &[OsString]) -> Result<(&OsString, &[OsString]), String> {
    match args {
        [_, separator, program, rest @ ..] if separator == "--" => {
            if !Path::new(program).is_absolute() {
                return Err("the program must be an absolute path".into());
            }
            Ok((program, rest))
        }
        _ => Err(USAGE.into()),
    }
}

/// The whole program; it returns only on failure.
pub fn run(args: Vec<OsString>) -> Result<Infallible, String> {
    let (program, rest) = request(&args)?;
    let policy = Policy::parse(&read_trusted(CONFIG)?).map_err(|e| format!("{CONFIG}: {e}"))?;
    let target = decide(&policy, &users(&read_trusted(PASSWD)?), rustix::process::getuid().as_raw())?;
    become_user(&target)?;
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let error = Command::new(program).args(rest).env_clear().env("HOME", &target.home).exec();
    Err(format!("cannot run {}: {error}", program.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    const POLICY: &str = "# written by activate\nruntime_user=nucleus\nuser=projects\n";
    const PASSWD_TEXT: &str = "root:x:0:0:root:/root:/bin/bash\n\
        nucleus:x:998:998::/var/lib/nucleus:/usr/sbin/nologin\n\
        projects:x:61000:61000::/workspace/nucleus/work/home:/usr/sbin/nologin\n";

    /// The one right way gets the projects' user with its own group alone;
    /// every other caller, and every user file that would make it someone
    /// else, is refused.
    #[test]
    fn only_the_runtime_user_becomes_the_projects_user_and_nothing_else() {
        let policy = Policy::parse(POLICY).unwrap();
        let home = "/workspace/nucleus/work/home".to_owned();
        assert_eq!(decide(&policy, &users(PASSWD_TEXT), 998), Ok(Target { uid: 61000, gid: 61000, home }));
        for caller in [0, 61000, 1000] {
            assert!(decide(&policy, &users(PASSWD_TEXT), caller).is_err(), "caller {caller}");
        }
        let projects = "projects:x:61000:61000::/workspace/nucleus/work/home:/usr/sbin/nologin\n";
        for (passwd, why) in [
            (PASSWD_TEXT.replace(projects, ""), "no projects user"),
            (format!("{PASSWD_TEXT}{projects}"), "two of them"),
            (PASSWD_TEXT.replace(":61000:61000:", ":0:61000:"), "root's uid"),
            (PASSWD_TEXT.replace(":61000:61000:", ":998:61000:"), "the runtime's uid"),
            (format!("{PASSWD_TEXT}other:x:61000:5::/:/x\n"), "a uid someone else has"),
            (PASSWD_TEXT.replace(":61000:61000:", ":61000:0:"), "root's group"),
            (PASSWD_TEXT.replace(":61000:61000:", ":61000:998:"), "the runtime's group"),
            (PASSWD_TEXT.replace("::/workspace/nucleus/work/home:", "::home:"), "a relative home"),
            (PASSWD_TEXT.replace("nucleus:x:998", "nucleus:x:01"), "no runtime user"),
        ] {
            assert!(decide(&policy, &users(&passwd), 998).is_err(), "{why}");
        }
        for bad in
            ["runtime_user=nucleus\n", "user=projects\nruntime_user=projects\n", &format!("{POLICY}x=1\n")]
        {
            assert!(Policy::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// It runs one program, named by an absolute path after `--`, never one
    /// looked up on a `PATH`.
    #[test]
    fn the_command_line_names_one_absolute_program() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let ok = args(&["nucleus-as", "--", "/opt/nucleus/bin/nucleus-shell", "--root", "/x"]);
        let (program, rest) = request(&ok).unwrap();
        assert_eq!((program.to_str(), rest.len()), (Some("/opt/nucleus/bin/nucleus-shell"), 2));
        for bad in [
            &["nucleus-as", "--"][..],
            &["nucleus-as", "/bin/sh"],
            &["nucleus-as", "--", "sh"],
            &["nucleus-as", "--", "bin/sh"],
            &["nucleus-as"],
        ] {
            assert!(request(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_files_the_policy_comes_from_are_checked() {
        let (file, dir, link) = (0o100_644, 0o040_755, 0o120_777);
        assert_eq!(trusted(0, file, false), Ok(()));
        assert_eq!(trusted(0, dir, true), Ok(()));
        assert!(trusted(998, file, false).is_err(), "owned by the runtime user");
        assert!(trusted(0, 0o100_664, false).is_err(), "group-writable");
        assert!(trusted(0, 0o040_757, true).is_err(), "world-writable directory");
        assert!(trusted(0, link, false).is_err() && trusted(0, file, true).is_err(), "wrong type");
    }
}
