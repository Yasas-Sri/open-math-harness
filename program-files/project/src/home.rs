//! The project's folder as this host sees it: one directory opened once, and
//! every other path resolved beneath it by the Linux kernel with no symbolic
//! link followed anywhere (`openat2` with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS`). Every project's processes can change anything in
//! the folder at any time; resolving every path this way keeps what this
//! host touches inside the folder whatever they plant there.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use nucleus_projects_interface::Dir;
use nucleus_protocol::{Error, ErrorCode};
use rustix::fs::{Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// Where the host keeps what it stages, in the folder but outside every
/// working tree: uploads before they are published, removed files before
/// they are deleted, a new repository before it becomes `repo/`.
pub const STAGING: &str = ".nucleus";

fn beneath() -> ResolveFlags {
    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS
}

/// The error a failed resolution or file operation reports.
pub fn errno(what: &str, e: Errno) -> Error {
    let (code, why) = match e {
        Errno::LOOP | Errno::XDEV => {
            (ErrorCode::PermissionDenied, "the path crosses a symbolic link".to_owned())
        }
        Errno::NOENT => (ErrorCode::InvalidInput, "no such file or directory".to_owned()),
        Errno::EXIST => (ErrorCode::InvalidInput, "it exists already".to_owned()),
        Errno::NOTDIR => (ErrorCode::InvalidInput, "a component is not a directory".to_owned()),
        Errno::ISDIR => (ErrorCode::InvalidInput, "it is a directory".to_owned()),
        Errno::NOTEMPTY => (ErrorCode::InvalidInput, "the directory is not empty".to_owned()),
        Errno::ACCESS | Errno::PERM => (ErrorCode::PermissionDenied, std::io::Error::from(e).to_string()),
        _ => (ErrorCode::ServiceError, std::io::Error::from(e).to_string()),
    };
    Error::new(code, format!("{what}: {why}"))
}

/// Opens `path` (relative, `/`-separated, `.` for `dir` itself) beneath `dir`.
pub fn open_beneath(dir: impl AsFd, path: &str, flags: OFlags, mode: Mode) -> Result<OwnedFd, Errno> {
    let path = if path.is_empty() { "." } else { path };
    rustix::fs::openat2(dir, path, flags | OFlags::CLOEXEC, mode, beneath())
}

/// A directory beneath `dir`, for further `*at` calls. A link as its last
/// component is refused like any other (`ELOOP`, from `RESOLVE_NO_SYMLINKS`;
/// `O_NOFOLLOW` would make it a "not a directory" instead).
pub fn open_dir(dir: impl AsFd, path: &str) -> Result<OwnedFd, Errno> {
    open_beneath(dir, path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
}

/// The project's folder, opened once: the files actions' repository is in
/// it, and what this host stages, on the same filesystem mount, so that a
/// staged file is renamed into place.
pub struct Home {
    fd: OwnedFd,
}

impl Home {
    pub fn open(path: &Path) -> Result<Self, String> {
        let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Self { fd })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The working directory `dir` names (it must exist).
    pub fn working(&self, dir: &Dir) -> Result<OwnedFd, Error> {
        let path = dir.path();
        open_dir(&self.fd, &path).map_err(|e| errno(&format!("the working directory {path}"), e))
    }

    /// `<staging>/<name>`, created when missing.
    pub fn staging(&self, name: &str) -> Result<OwnedFd, Error> {
        let mut dir = self.fd.try_clone().map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))?;
        for part in [STAGING, name] {
            match rustix::fs::mkdirat(&dir, part, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(errno(&format!("{STAGING}/{name}"), e)),
            }
            dir = open_dir(&dir, part).map_err(|e| errno(&format!("{STAGING}/{name}"), e))?;
        }
        Ok(dir)
    }
}

/// The directory above `folder`, opened with no link followed anywhere on
/// its path, and the folder's name in it.
fn parent_of(folder: &Path) -> Result<(OwnedFd, String), String> {
    let (Some(parent), Some(name)) = (folder.parent(), folder.file_name().and_then(|n| n.to_str())) else {
        return Err(format!("{} names no folder", folder.display()));
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let resolve = ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    let fd = rustix::fs::openat2(rustix::fs::CWD, parent, flags, Mode::empty(), resolve)
        .map_err(|e| format!("cannot open {} without following links: {e}", parent.display()))?;
    Ok((fd, name.to_owned()))
}

/// Makes a new project's folder, mode 0700, never over anything of the
/// name.
pub fn make(folder: &Path) -> Result<(), String> {
    let (parent, name) = parent_of(folder)?;
    rustix::fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o700))
        .map_err(|e| format!("cannot make {}: {e}", folder.display()))
}

/// Removes a project's folder with everything in it, through the directory
/// above it, opened with no link followed: a link in it is removed, never
/// what it leads to.
pub fn remove(folder: &Path) -> Result<(), Error> {
    let failed = |why: String| Error::new(ErrorCode::ServiceError, why);
    let (parent, name) = parent_of(folder).map_err(failed)?;
    let parent = cap_std::fs::Dir::from_std_file(std::fs::File::from(parent));
    match parent.remove_dir_all(&name) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failed(format!("removing {}: {e}", folder.display()))),
    }
}

/// A fresh name for something staged: unpredictable and never reused in
/// this process (creation with `O_EXCL` or `RENAME_NOREPLACE` guards the rest).
pub fn fresh_name(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNT.fetch_add(1, Ordering::SeqCst));
    format!("{prefix}-{}-{:016x}", std::process::id(), hasher.finish())
}
