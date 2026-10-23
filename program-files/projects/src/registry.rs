//! The projects directory is the registry: a project is a
//! folder `projects/<name>`, of the projects' user. With projects'
//! processes the project's host makes that folder, as that user, and removes
//! it; without (a composition with no projects' processes), it is a
//! directory this service makes and removes.
//!
//! Four kinds of entry are this service's own:
//!
//! - `.creating-<project>`, a file, while the project is being created. One
//!   that a restart finds belongs to a creation that never ended: that
//!   project is deleted.
//! - `.deleting-<project>`, the project's folder, renamed when its deletion
//!   begins. Under that name no host or agent of it starts (nothing names
//!   it), the project is listed as being deleted, and its name is not taken
//!   again until the deletion has ended.
//! - `.archiving-<project>`, the folder while the caller stops the project's
//!   work. It prevents new execution but does not claim the work has stopped.
//! - `.archived-<project>`, the preserved folder while execution is disabled.
//!   Its name remains reserved; unarchiving restores the active folder name.
//!
//! Every project's processes can write the projects directory too: what one
//! does to these entries it could do to every project's folder anyway.
//! Everything here is resolved beneath the opened projects directory with no
//! link followed.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use nucleus_projects_interface::valid_name;
use nucleus_protocol::{Error, ErrorCode};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags};
use rustix::io::Errno;

/// The marker of a project being created.
pub const CREATING: &str = ".creating-";
/// The name of a project's home while it is being deleted.
pub const DELETING: &str = ".deleting-";
/// The folder of an archived project. No execution interface resolves this name.
/// Archiving preserves the data but neither freezes nor isolates it: other
/// projects' commands, running as the same Linux user, can still read and
/// change the archived folder.
pub const ARCHIVED: &str = ".archived-";
pub const ARCHIVING: &str = ".archiving-";

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

fn failed(what: &str, e: Errno) -> Error {
    Error::new(ErrorCode::ServiceError, format!("{what}: {}", std::io::Error::from(e)))
}

/// One project of a page of the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub deleting: bool,
    pub archived: bool,
    pub archiving: bool,
}

pub struct Registry {
    path: PathBuf,
    fd: OwnedFd,
    /// Whether the projects' hosts make and remove the folders.
    identities: bool,
}

impl Registry {
    pub fn open(path: &Path, identities: bool) -> Result<Self, String> {
        let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| format!("cannot open the projects directory {}: {e}", path.display()))?;
        Ok(Self { path: path.to_owned(), fd, identities })
    }

    /// The path of a folder: the same in this process's sandbox and for the
    /// project's own processes, which see the real one.
    pub fn home_path(&self, dirname: &str) -> String {
        format!("{}/{dirname}", self.path.display())
    }

    /// What `projects/<name>` is: `None` when nothing has the name.
    fn kind(&self, name: &str) -> Result<Option<FileType>, Error> {
        match rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(failed(name, e)),
        }
    }

    fn is_dir(&self, name: &str) -> Result<bool, Error> {
        Ok(self.kind(name)? == Some(FileType::Directory))
    }

    fn entries(&self) -> Result<Vec<String>, Error> {
        let dir = rustix::fs::openat2(
            &self.fd,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH,
        )
        .map_err(|e| failed("projects", e))?;
        let listing = cap_std::fs::Dir::from_std_file(std::fs::File::from(dir));
        Ok(listing
            .entries()
            .map_err(|e| Error::new(ErrorCode::ServiceError, e.to_string()))?
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .collect())
    }

    /// A page of the projects after `after`, by name, at most `limit`, and
    /// whether more follow; only `only`, when the caller is narrowed to it.
    /// A project being created is not listed; one being deleted is, as such.
    /// The listing is read twice (the markers first), and a page holds no
    /// more than `limit` names at any moment, however many projects there
    /// are.
    pub fn page(
        &self,
        after: Option<&str>,
        limit: usize,
        only: Option<&str>,
    ) -> Result<(Vec<Listed>, bool), Error> {
        let creating: BTreeSet<String> = self
            .entries()?
            .into_iter()
            .filter_map(|name| name.strip_prefix(CREATING).map(str::to_owned))
            .collect();
        let mut page: BTreeMap<String, (bool, bool, bool)> = BTreeMap::new();
        let mut more = false;
        for entry in self.entries()? {
            let (name, deleting, archived, archiving) = match entry.strip_prefix(DELETING) {
                Some(name) => (name.to_owned(), true, false, false),
                None => match entry.strip_prefix(ARCHIVED) {
                    Some(name) => (name.to_owned(), false, true, false),
                    None => match entry.strip_prefix(ARCHIVING) {
                        Some(name) => (name.to_owned(), false, false, true),
                        None => (entry.clone(), false, false, false),
                    },
                },
            };
            if !valid_name(&name)
                || creating.contains(&name)
                || only.is_some_and(|only| only != name)
                || after.is_some_and(|after| name.as_str() <= after)
                || !self.is_dir(&entry)?
            {
                continue;
            }
            page.insert(name, (deleting, archived, archiving));
            if page.len() > limit {
                page.pop_last();
                more = true;
            }
        }
        Ok((
            page.into_iter()
                .map(|(name, (deleting, archived, archiving))| Listed { name, deleting, archived, archiving })
                .collect(),
            more,
        ))
    }

    /// Refused unless `project` exists, is not being created, and is not
    /// being deleted.
    pub fn exists(&self, project: &str) -> Result<(), Error> {
        if !valid_name(project) {
            return Err(invalid(format!("{project:?} is not a project name")));
        }
        if self.is_dir(project)? && self.kind(&format!("{CREATING}{project}"))?.is_none() {
            Ok(())
        } else {
            Err(invalid(format!("there is no project {project}")))
        }
    }

    /// Whether anything has the name of `project`, as itself, being created
    /// or being deleted.
    pub fn taken(&self, project: &str) -> Result<bool, Error> {
        for name in [
            project.to_owned(),
            format!("{CREATING}{project}"),
            format!("{DELETING}{project}"),
            format!("{ARCHIVED}{project}"),
            format!("{ARCHIVING}{project}"),
        ] {
            if self.kind(&name)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn archivable(&self, project: &str) -> Result<(), Error> {
        if !valid_name(project) {
            return Err(invalid("invalid project name"));
        }
        if self.kind(&format!("{CREATING}{project}"))?.is_some()
            || self.kind(&format!("{DELETING}{project}"))?.is_some()
        {
            return Err(invalid("the project is being created or deleted"));
        }
        Ok(())
    }

    /// Gives `project` its archiving name, which prevents new execution before
    /// its work is stopped, without claiming that work has stopped.
    pub fn begin_archive(&self, project: &str) -> Result<(), Error> {
        self.archivable(project)?;
        let pending = format!("{ARCHIVING}{project}");
        let stored = format!("{ARCHIVED}{project}");
        match (self.is_dir(project)?, self.is_dir(&pending)?, self.is_dir(&stored)?) {
            (true, false, false) => self.rename(project, &pending),
            (false, true, false) | (false, false, true) => Ok(()),
            _ => Err(invalid("the project is missing or has conflicting folders")),
        }
    }

    /// Mark stopped work archived, or restore its folder without starting anything.
    pub fn archive(&self, project: &str, archived: bool) -> Result<(), Error> {
        self.archivable(project)?;
        let pending = format!("{ARCHIVING}{project}");
        let stored = format!("{ARCHIVED}{project}");
        let (from, to) = if archived {
            if self.is_dir(project)? {
                return Err(invalid("the project has not begun archiving"));
            }
            (pending.as_str(), stored.as_str())
        } else {
            if self.kind(&pending)?.is_some() {
                return Err(invalid("the project has not finished archiving"));
            }
            (stored.as_str(), project)
        };
        match (self.is_dir(from)?, self.is_dir(to)?) {
            (true, false) => self.rename(from, to),
            (false, true) => Ok(()),
            _ => Err(invalid("the project is missing or has conflicting folders")),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Error> {
        rustix::fs::renameat_with(&self.fd, from, &self.fd, to, RenameFlags::NOREPLACE)
            .map_err(|e| failed(from, e))
    }

    pub fn stoppable(&self, project: &str) -> Result<(), Error> {
        if valid_name(project)
            && (self.is_dir(&format!("{ARCHIVED}{project}"))?
                || self.is_dir(&format!("{ARCHIVING}{project}"))?)
        {
            Ok(())
        } else {
            self.exists(project)
        }
    }

    /// Marks `project` as being created, never over anything.
    pub fn mark_creating(&self, project: &str) -> Result<(), Error> {
        let flags = OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let name = format!("{CREATING}{project}");
        rustix::fs::openat(&self.fd, name.as_str(), flags, Mode::from_raw_mode(0o600)).map(drop).map_err(
            |e| {
                if e == Errno::EXIST {
                    invalid(format!("{project} is being created"))
                } else {
                    failed(&name, e)
                }
            },
        )
    }

    /// Its creation has ended, either way.
    pub fn unmark_creating(&self, project: &str) -> Result<(), Error> {
        let name = format!("{CREATING}{project}");
        match rustix::fs::unlinkat(&self.fd, name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(failed(&name, e)),
        }
    }

    /// The projects whose creation never ended: a restart found their marker.
    pub fn interrupted(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .entries()?
            .into_iter()
            .filter_map(|name| name.strip_prefix(CREATING).filter(|p| valid_name(p)).map(str::to_owned))
            .collect())
    }

    /// Without projects' processes, a new project's folder: a directory this
    /// service makes. With them the project's host makes it.
    pub fn make_home(&self, project: &str) -> Result<(), Error> {
        if self.identities {
            return Ok(());
        }
        rustix::fs::mkdirat(&self.fd, project, Mode::from_raw_mode(0o700)).map_err(|e| failed(project, e))
    }

    /// Begins `project`'s deletion: its folder takes its deleting name, unless
    /// it has it already. `false`: there is nothing of it to delete.
    pub fn begin_delete(&self, project: &str) -> Result<bool, Error> {
        if !valid_name(project) {
            return Err(invalid(format!("{project:?} is not a project name")));
        }
        let gone = format!("{DELETING}{project}");
        for prefix in [ARCHIVING, ARCHIVED] {
            let archived = format!("{prefix}{project}");
            if self.is_dir(&archived)? {
                rustix::fs::renameat_with(
                    &self.fd,
                    archived.as_str(),
                    &self.fd,
                    gone.as_str(),
                    RenameFlags::NOREPLACE,
                )
                .map_err(|e| failed(project, e))?;
            }
        }
        match (self.is_dir(project)?, self.is_dir(&gone)?) {
            (false, false) => Ok(false),
            (false, true) => Ok(true),
            (true, true) => Err(Error::new(
                ErrorCode::ServiceError,
                format!("both {project} and {gone} exist; one must be removed by hand"),
            )),
            (true, false) => {
                rustix::fs::renameat_with(&self.fd, project, &self.fd, gone.as_str(), RenameFlags::NOREPLACE)
                    .map(|()| true)
                    .map_err(|e| failed(project, e))
            }
        }
    }

    /// Without projects' processes, a deleted project's folder goes with
    /// everything in it. With them the project's host removes it.
    pub fn remove_deleted(&self, project: &str) -> Result<(), Error> {
        if self.identities {
            return Ok(());
        }
        let projects = match self.fd.try_clone() {
            Ok(fd) => cap_std::fs::Dir::from_std_file(std::fs::File::from(fd)),
            Err(e) => return Err(Error::new(ErrorCode::ServiceError, e.to_string())),
        };
        match projects.remove_dir_all(format!("{DELETING}{project}")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::new(ErrorCode::ServiceError, format!("removing {project}: {e}"))),
        }
    }
}
