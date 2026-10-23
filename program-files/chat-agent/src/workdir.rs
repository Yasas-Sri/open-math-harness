//! The session's working directory, as this agent reads files there: the
//! files a person attached to a message (`attachments`), the images
//! `view_image` shows (`images`) and the images its replies show
//! (`reply_images`).
//!
//! It is opened once, at start, from `--workdir`, which the sessions service
//! sets to the repository in the session's project's home (`<home>/repo`).
//! Every path is resolved beneath it by the Linux kernel
//! with no symbolic link followed anywhere (`openat2` with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS`), non-blocking, and only a regular file is read:
//! size, kind and content are checked on the file actually opened, never on
//! a name looked up again. In the VM the agent runs as the projects' user,
//! so it reads only what that user may, and the resolution keeps it in
//! the working directory whatever the project's own processes plant there.

use std::io::Read;
use std::os::fd::OwnedFd;

use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// Why a file could not be used, in words for the person and the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

/// Why a file that is not there could not be used.
pub const NOT_THERE: &str = "it is not there";

impl Refused {
    fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

/// The working directory, opened.
pub struct Workdir {
    fd: OwnedFd,
    /// Where it is in this process's namespace, without a trailing `/`.
    path: String,
}

impl Workdir {
    /// Opens the directory at `path`, an absolute path of plain names.
    pub fn open(path: &str) -> Result<Self, String> {
        let plain = path
            .strip_prefix('/')
            .is_some_and(|rest| nucleus_projects_interface::segments(rest.trim_end_matches('/')).is_ok());
        if !plain {
            return Err(format!("--workdir {path}: an absolute path of plain names"));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd =
            rustix::fs::open(path, flags, Mode::empty()).map_err(|e| format!("--workdir {path}: {e}"))?;
        Ok(Self { fd, path: path.trim_end_matches('/').to_owned() })
    }

    /// `path` relative to this directory: a relative path as it is, an
    /// absolute one only beneath this directory. `.` segments mean nothing.
    pub fn relative(&self, path: &str) -> Result<String, Refused> {
        let inside = match path.strip_prefix('/') {
            None => path,
            Some(_) => path
                .strip_prefix(self.path.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| Refused(format!("it is not in the working directory {}", self.path)))?,
        };
        let parts: Vec<&str> = inside.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
        let joined = parts.join("/");
        match nucleus_projects_interface::segments(&joined) {
            Ok(segments) if !segments.is_empty() => Ok(joined),
            _ => Err(Refused::new("its path is not a relative path of plain names in the working directory")),
        }
    }

    /// Opens the regular file at `relative` (a path [`Self::relative`]
    /// returned), and says how large it is.
    pub fn open_file(&self, relative: &str) -> Result<(std::fs::File, u64), Refused> {
        self.open_regular(relative).map_err(Refused::from)
    }

    /// [`Self::open_file`], saying why in a form a program reads.
    pub fn open_regular(&self, relative: &str) -> Result<(std::fs::File, u64), Unopened> {
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        // Non-blocking, so a FIFO planted under the name cannot hang the agent.
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd =
            rustix::fs::openat2(&self.fd, relative, flags, Mode::empty(), resolve).map_err(Unopened::Open)?;
        let stat = rustix::fs::fstat(&fd).map_err(Unopened::Stat)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(Unopened::NotRegular);
        }
        Ok((fd.into(), u64::try_from(stat.st_size).unwrap_or(u64::MAX)))
    }
}

/// Why [`Workdir::open_regular`] opened no file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unopened {
    /// The path could not be opened: `LOOP` or `XDEV` for a symbolic link in
    /// it, `NOENT` when nothing is there.
    Open(Errno),
    /// What was opened could not be looked at.
    Stat(Errno),
    /// What was opened is no regular file.
    NotRegular,
}

impl From<Unopened> for Refused {
    fn from(why: Unopened) -> Self {
        Self::new(match why {
            Unopened::Open(Errno::LOOP | Errno::XDEV) => "a symbolic link is in its path".to_owned(),
            Unopened::Open(Errno::NOENT) => NOT_THERE.to_owned(),
            Unopened::Open(Errno::NOTDIR) => "a part of its path is not a directory".to_owned(),
            Unopened::Open(Errno::ACCESS | Errno::PERM) => "it may not be read".to_owned(),
            Unopened::Open(e) => format!("it could not be opened: {}", std::io::Error::from(e)),
            Unopened::Stat(e) => format!("it could not be read: {e}"),
            Unopened::NotRegular => "it is not a regular file".to_owned(),
        })
    }
}

/// The whole of a file of `size` bytes, which must be at most `limit`: a
/// file that is larger, or grew while it was read, is refused.
pub fn read_all(file: std::fs::File, size: u64, limit: usize) -> Result<Vec<u8>, Refused> {
    if size > limit as u64 {
        return Err(Refused(format!("it is larger than {}", human(limit as u64))));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Refused(format!("it could not be read: {e}")))?;
    if bytes.len() as u64 != size {
        return Err(Refused::new("it changed while it was read"));
    }
    Ok(bytes)
}

/// At most the first `limit` bytes of a file.
pub fn read_head(file: std::fs::File, limit: usize) -> Result<Vec<u8>, Refused> {
    let mut bytes = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| Refused(format!("it could not be read: {e}")))?;
    Ok(bytes)
}

/// The text of `head`, the first bytes of a file of `size` bytes, when it is
/// UTF-8 text (no NUL; a character cut at the end of a short read is dropped).
pub fn text(head: &[u8], size: u64) -> Option<&str> {
    let text = match std::str::from_utf8(head) {
        Ok(text) => text,
        // Cut inside a character because only the head was read.
        Err(e) if e.error_len().is_none() && (head.len() as u64) < size => {
            std::str::from_utf8(&head[..e.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    (!text.contains('\0')).then_some(text)
}

/// A size as people read it: `812 B`, `182.4 KiB`, `3.1 MiB`.
pub fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{:.1} MiB", value / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn paths_are_relative_to_the_working_directory_and_plain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_owned();
        let workdir = Workdir::open(&root).unwrap();
        assert_eq!(workdir.relative("plots/./fig.png").unwrap(), "plots/fig.png");
        assert_eq!(workdir.relative(&format!("{root}/plots/fig.png")).unwrap(), "plots/fig.png");
        for outside in
            ["../secret.png", "plots/../../x.png", "/etc/passwd", "", ".", &format!("{root}x/a.png")]
        {
            assert!(workdir.relative(outside).is_err(), "{outside:?}");
        }
        assert!(Workdir::open("relative/dir").is_err() && Workdir::open("/a/../b").is_err());
    }

    #[test]
    fn text_is_utf8_without_nul_and_a_cut_character_at_the_end_is_dropped() {
        assert_eq!(text("é ok".as_bytes(), 5), Some("é ok"));
        assert_eq!(text(&"aé".as_bytes()[..2], 10), Some("a"), "the head ends inside a character");
        assert_eq!(text(&"aé".as_bytes()[..2], 2), None, "the whole file ends inside one");
        assert_eq!(text(b"PK\x03\x04\0\0", 6), None);
        assert_eq!(text(b"\xff\xfe", 2), None);
    }
}
