//! The workspace as this service sees it: one directory opened once, every
//! other path resolved beneath it by the Linux kernel with no symbolic link
//! followed anywhere (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`).

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use nucleus_protocol::{Error, ErrorCode};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// Longest path, in bytes, of a repository or of a changed file (the Linux
/// `PATH_MAX`). A longer one is refused.
pub const MAX_PATH_BYTES: usize = 4096;

fn beneath() -> ResolveFlags {
    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> Error {
    Error::new(code, message.into())
}

fn crossed_link(e: Errno) -> bool {
    matches!(e, Errno::LOOP | Errno::XDEV)
}

/// A relative path as `openat2` takes it: `.` for the directory itself.
fn relative(path: &Path) -> &Path {
    if path.as_os_str().is_empty() { Path::new(".") } else { path }
}

/// What a path names beneath `dir`, as opened there without following a link
/// anywhere.
pub enum Content {
    /// A regular file, open for reading, and its size.
    File(std::fs::File, u64),
    /// A symbolic link's target: what git stores and shows for a link.
    Link(Vec<u8>),
    /// A directory or a special file.
    Other,
}

/// `path` beneath `dir` with `O_PATH`, which never blocks whatever the file
/// is, and its status; `None` when nothing is there.
fn open_path(dir: BorrowedFd<'_>, path: &str) -> Result<Option<(OwnedFd, rustix::fs::Stat)>, Error> {
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat2(dir, path, flags, Mode::empty(), beneath()) {
        Ok(fd) => {
            let stat = rustix::fs::fstat(&fd).map_err(|e| io_error(path, e))?;
            Ok(Some((fd, stat)))
        }
        Err(Errno::NOENT | Errno::NOTDIR) => Ok(None),
        Err(e) if crossed_link(e) => Err(link_refused(path)),
        Err(e) => Err(io_error(path, e)),
    }
}

/// Longest [`version`] token.
pub const MAX_VERSION_BYTES: usize = 256;

/// The `version` of a changed file (interface module doc): its `HEAD` object
/// and what `path` names beneath `dir` now — inode, size, times and type, from
/// one `fstat` through a no-follow, beneath-only open, never its content.
/// Nothing there, or a path through a link, is part of the token too.
pub fn version(dir: BorrowedFd<'_>, path: &str, head: Option<&str>) -> String {
    let head = head.unwrap_or("-");
    match open_path(dir, path) {
        Ok(Some((_, stat))) => format!(
            "{head}:{}:{}:{}.{}:{}.{}:{:o}",
            stat.st_ino,
            stat.st_size,
            stat.st_mtime,
            stat.st_mtime_nsec,
            stat.st_ctime,
            stat.st_ctime_nsec,
            stat.st_mode
        ),
        Ok(None) => format!("{head}:absent"),
        Err(_) => format!("{head}:unreadable"),
    }
}

/// The size of what `path` names beneath `dir`; `None` when nothing is there.
pub fn size(dir: BorrowedFd<'_>, path: &str) -> Result<Option<u64>, Error> {
    Ok(open_path(dir, path)?.map(|(_, stat)| u64::try_from(stat.st_size).unwrap_or(0)))
}

/// Opens `path` beneath `dir` to read what it holds; `None` when nothing is
/// there. A regular file is reopened through the descriptor that was
/// checked, never by its name again, and without blocking.
pub fn open(dir: BorrowedFd<'_>, path: &str) -> Result<Option<Content>, Error> {
    let Some((fd, stat)) = open_path(dir, path)? else { return Ok(None) };
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(&fd, "", Vec::new()).map_err(|e| io_error(path, e))?;
            Ok(Some(Content::Link(target.into_bytes())))
        }
        FileType::RegularFile => {
            let this = format!("/proc/self/fd/{}", fd.as_raw_fd());
            let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let file = rustix::fs::open(this, flags, Mode::empty()).map_err(|e| io_error(path, e))?;
            Ok(Some(Content::File(file.into(), u64::try_from(stat.st_size).unwrap_or(0))))
        }
        _ => Ok(Some(Content::Other)),
    }
}

fn link_refused(path: &str) -> Error {
    refuse(ErrorCode::PermissionDenied, format!("{path}: the path crosses a symbolic link"))
}

fn io_error(path: impl std::fmt::Debug, e: Errno) -> Error {
    let code = if e == Errno::ACCESS { ErrorCode::PermissionDenied } else { ErrorCode::ServiceError };
    Error::new(code, format!("{path:?}: {}", std::io::Error::from(e)))
}

/// The segments of a `/`-separated relative path from a request, refused
/// unless each is a plain name: no `.`, `..`, empty segment, absolute path,
/// NUL, and never `.git`.
pub fn segments<'a>(path: &'a str, what: &str) -> Result<Vec<&'a str>, Error> {
    if path.len() > MAX_PATH_BYTES {
        return Err(refuse(
            ErrorCode::InvalidInput,
            format!("the {what} path is over {MAX_PATH_BYTES} bytes"),
        ));
    }
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." || part.contains('\0') || *part == ".git" {
            return Err(refuse(
                ErrorCode::InvalidInput,
                format!("the {what} {path:?} must be a relative path of plain names, outside .git"),
            ));
        }
    }
    Ok(parts)
}

/// The directory tree the service reads: the only use of ambient authority.
pub struct Workspace {
    root: Dir,
    path: PathBuf,
    /// The one repository Review shows, relative to the root.
    repository: String,
}

impl Workspace {
    pub fn open(path: &Path, repository: &str) -> std::io::Result<Self> {
        Ok(Self {
            root: Dir::open_ambient_dir(path, ambient_authority())?,
            path: path.to_owned(),
            repository: repository.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_dir(&self, path: &Path) -> Result<OwnedFd, Errno> {
        let flags =
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        rustix::fs::openat2(&self.root, relative(path), flags, Mode::empty(), beneath())
    }

    /// The repository Review shows: its name, relative to the root (`""`
    /// is the root itself).
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Opens the repository `name` if it is the one this service shows: a
    /// directory beneath the root reached through no symbolic link, with a
    /// `.git` directory or file inside. Git then runs in this descriptor.
    pub fn open_repository(&self, name: &str) -> Result<OwnedFd, Error> {
        segments(name, "repository")?;
        if name != self.repository {
            return Err(refuse(
                ErrorCode::InvalidInput,
                format!("Review shows the project's repository {:?} only", self.repository),
            ));
        }
        let fd = match self.open_dir(Path::new(name)) {
            Ok(fd) => fd,
            Err(e) if crossed_link(e) => return Err(link_refused(name)),
            Err(Errno::NOENT | Errno::NOTDIR) => {
                return Err(refuse(ErrorCode::InvalidInput, format!("there is no repository at {name:?}")));
            }
            Err(e) => return Err(io_error(name, e)),
        };
        match rustix::fs::statat(&fd, ".git", AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if matches!(
                    FileType::from_raw_mode(stat.st_mode),
                    FileType::Directory | FileType::RegularFile
                ) =>
            {
                Ok(fd)
            }
            Ok(_) | Err(Errno::NOENT) => {
                Err(refuse(ErrorCode::InvalidInput, format!("{name:?} is not a git repository")))
            }
            Err(e) => Err(io_error(name, e)),
        }
    }
}

/// Refuses `path` beneath the repository when a directory on the way to it is
/// a symbolic link. A directory that is not there is fine: a deleted file's
/// parent may be gone too.
pub fn check_parents(repository: BorrowedFd<'_>, path: &str) -> Result<(), Error> {
    let Some((parent, _)) = path.rsplit_once('/') else { return Ok(()) };
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    match rustix::fs::openat2(repository, parent, flags, Mode::empty(), beneath()) {
        Ok(_) | Err(Errno::NOENT | Errno::NOTDIR) => Ok(()),
        Err(e) if crossed_link(e) => Err(link_refused(path)),
        Err(e) => Err(io_error(path, e)),
    }
}
