//! A repository's skills, in its `.skill/` folder: read and added beneath
//! the repository with no symbolic link followed in any component (`openat2`
//! with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), like the `fs` service. The
//! project's own processes write there too, so nothing found in it is trusted
//! to be what a name says: every file is opened, then checked, then read.
//!
//! A skill is added whole or not at all: its files are written into a hidden
//! folder beside the others, which is then renamed to its name, never over a
//! folder that is there.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

use cap_std::fs::Dir;
use nucleus_protocol::{Error, ErrorCode};
use nucleus_skills_interface::{DIRECTORY, MAX_DESCRIPTION_CHARS, SKILL_FILE, valid_name};
use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};

use crate::front_matter;

/// Largest `SKILL.md` read, in bytes; `load` returns at most this much of it.
pub const MAX_INSTRUCTIONS_BYTES: u64 = 256 * 1024;
/// Most files of one skill, and most bytes all of them hold.
pub const MAX_FILES: usize = 500;
pub const MAX_SKILL_BYTES: u64 = 16 * 1024 * 1024;
/// A skill's files: each path, relative to its folder, and content.
pub type Files = Vec<(String, Vec<u8>)>;
/// Deepest folder of a skill.
const MAX_DEPTH: usize = 8;

fn refuse(code: ErrorCode, message: impl Into<String>) -> Error {
    Error::new(code, message.into())
}

fn io(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::ServiceError, format!("{what}: {e}"))
}

/// Opens `path` beneath `dir`, following no link; refuses a link as such.
fn open_beneath(
    dir: impl rustix::fd::AsFd,
    path: &Path,
    flags: OFlags,
    mode: Mode,
) -> Result<OwnedFd, Error> {
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    rustix::fs::openat2(dir, path, flags | OFlags::CLOEXEC, mode, resolve).map_err(|e| match e {
        rustix::io::Errno::LOOP | rustix::io::Errno::XDEV => refuse(
            ErrorCode::PermissionDenied,
            format!("{}: a symbolic link or outside the repository", path.display()),
        ),
        rustix::io::Errno::NOENT => {
            refuse(ErrorCode::InvalidInput, format!("{} is not there", path.display()))
        }
        e => io(&path.display().to_string(), e),
    })
}

fn open_dir(dir: impl rustix::fd::AsFd, path: &Path) -> Result<OwnedFd, Error> {
    open_beneath(dir, path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
}

/// Reads at most `limit` bytes of the regular file at `path`, and whether it
/// holds more.
fn read_file(dir: impl rustix::fd::AsFd, path: &Path, limit: u64) -> Result<(Vec<u8>, bool), Error> {
    // Non-blocking, so a FIFO planted under the name cannot hang the service.
    let fd = open_beneath(dir, path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd).map_err(|e| io("stat", e))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(refuse(ErrorCode::InvalidInput, format!("{} is not a regular file", path.display())));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd).take(limit + 1).read_to_end(&mut bytes).map_err(|e| io("read", e))?;
    let more = bytes.len() as u64 > limit;
    bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok((bytes, more))
}

/// One skill found in the folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub description: String,
}

/// A skill as `load` returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub instructions: String,
    pub truncated: bool,
    pub files: Vec<String>,
}

/// The repositories' skills a host serves: their folders, beneath its root.
pub struct Store {
    root: OwnedFd,
}

impl Store {
    /// Opens the root once, the only use of ambient authority.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let root =
            rustix::fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Self { root })
    }

    /// The repository at `repository` (segments beneath the root), opened.
    fn repository(&self, repository: &[String]) -> Result<OwnedFd, Error> {
        let path: std::path::PathBuf = repository.iter().collect();
        open_dir(&self.root, if repository.is_empty() { Path::new(".") } else { &path })
    }

    /// The repository's `.skill/`, or `None` when it has none.
    fn skills(&self, repository: &[String]) -> Result<Option<OwnedFd>, Error> {
        let repo = self.repository(repository)?;
        match open_dir(&repo, Path::new(DIRECTORY)) {
            Ok(dir) => Ok(Some(dir)),
            Err(e) if e.code == ErrorCode::InvalidInput => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The skills of the repository: each visible folder of `.skill/` with a
    /// valid name and a `SKILL.md`, by name, with its description.
    pub fn list(&self, repository: &[String]) -> Result<Vec<Found>, Error> {
        let Some(skills) = self.skills(repository)? else { return Ok(Vec::new()) };
        let dir = Dir::from_std_file(std::fs::File::from(skills));
        let mut found = Vec::new();
        for entry in dir.entries().map_err(|e| io("list .skill", e))? {
            let entry = entry.map_err(|e| io("list .skill", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !valid_name(&name) {
                continue;
            }
            let Ok((bytes, _)) = read_file(&dir, &Path::new(&name).join(SKILL_FILE), MAX_INSTRUCTIONS_BYTES)
            else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let description = front_matter::parse(&text).description.unwrap_or_default();
            found
                .push(Found { name, description: description.chars().take(MAX_DESCRIPTION_CHARS).collect() });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    /// Whether the repository has a skill (a folder) of this name.
    pub fn has(&self, repository: &[String], name: &str) -> Result<bool, Error> {
        let Some(skills) = self.skills(repository)? else { return Ok(false) };
        match open_dir(&skills, Path::new(name)) {
            Ok(_) => Ok(true),
            Err(e) if e.code == ErrorCode::InvalidInput => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The skill `name`: its `SKILL.md` and its other files.
    pub fn read(&self, repository: &[String], name: &str) -> Result<Loaded, Error> {
        if !valid_name(name) {
            return Err(refuse(ErrorCode::InvalidInput, format!("{name:?} is not a skill name")));
        }
        let skills = self
            .skills(repository)?
            .ok_or_else(|| refuse(ErrorCode::InvalidInput, format!("this project has no skill {name}")))?;
        let folder = open_dir(&skills, Path::new(name)).map_err(|e| {
            if e.code == ErrorCode::InvalidInput {
                refuse(e.code, format!("this project has no skill {name}"))
            } else {
                e
            }
        })?;
        let (bytes, truncated) =
            read_file(&folder, Path::new(SKILL_FILE), MAX_INSTRUCTIONS_BYTES).map_err(|e| {
                refuse(e.code, format!("the skill {name} has no readable {SKILL_FILE}: {}", e.message))
            })?;
        let mut files = Vec::new();
        walk(&Dir::from_std_file(std::fs::File::from(folder)), "", 0, &mut files)?;
        files.retain(|f| f != SKILL_FILE);
        Ok(Loaded { instructions: String::from_utf8_lossy(&bytes).into_owned(), truncated, files })
    }

    /// Adds the skill `name` with `files` (paths relative to its folder, each
    /// already checked by [`file_path`]), whole or not at all, never over a
    /// folder of that name.
    pub fn add(&self, repository: &[String], name: &str, files: &[(String, Vec<u8>)]) -> Result<(), Error> {
        if !valid_name(name) {
            return Err(refuse(ErrorCode::InvalidInput, format!("{name:?} is not a skill name")));
        }
        if let Some(folder) = repository_inside(files) {
            return Err(refuse(
                ErrorCode::InvalidInput,
                format!("the skill holds a git repository at {folder:?}; a skill may not"),
            ));
        }
        let repo = self.repository(repository)?;
        let skills = match open_dir(&repo, Path::new(DIRECTORY)) {
            Ok(dir) => dir,
            Err(e) if e.code == ErrorCode::InvalidInput => {
                match rustix::fs::mkdirat(&repo, DIRECTORY, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(io("make .skill", e)),
                }
                open_dir(&repo, Path::new(DIRECTORY))?
            }
            Err(e) => return Err(e),
        };
        let skills = Dir::from_std_file(std::fs::File::from(skills));
        if self.has(repository, name)? {
            return Err(exists(name));
        }
        let partial = format!(
            ".{name}.partial-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        );
        skills.create_dir(&partial).map_err(|e| io("make the skill's folder", e))?;
        let written = (|| {
            let folder = open_dir(&skills, Path::new(&partial))?;
            for (path, bytes) in files {
                write_file(&folder, path, bytes)?;
            }
            rustix::fs::renameat_with(&skills, &partial, &skills, name, rustix::fs::RenameFlags::NOREPLACE)
                .map_err(
                    |e| if e == rustix::io::Errno::EXIST { exists(name) } else { io("add the skill", e) },
                )
        })();
        if written.is_err() {
            let _ = skills.remove_dir_all(&partial);
        }
        written
    }
}

fn exists(name: &str) -> Error {
    refuse(
        ErrorCode::InvalidInput,
        format!(
            "this project already has a skill {name} (.skill/{name}); load it by name, or remove that folder to replace it"
        ),
    )
}

/// Writes one new file, making its folders, beneath `folder`. A file that
/// begins with `#!` is executable, as a skill's scripts are.
fn write_file(folder: &OwnedFd, path: &str, bytes: &[u8]) -> Result<(), Error> {
    let segments: Vec<&str> = path.split('/').collect();
    let mut parent = rustix::io::dup(folder).map_err(|e| io("dup", e))?;
    for segment in &segments[..segments.len() - 1] {
        match rustix::fs::mkdirat(&parent, *segment, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(io(path, e)),
        }
        parent = open_dir(&parent, Path::new(segment))?;
    }
    let mode = if bytes.starts_with(b"#!") { 0o777 } else { 0o666 };
    let name = segments[segments.len() - 1];
    let fd = open_beneath(
        &parent,
        Path::new(name),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::from_raw_mode(mode),
    )?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).map_err(|e| io(path, e))?;
    file.sync_all().map_err(|e| io(path, e))
}

/// The regular files beneath `dir`, as paths relative to the skill's folder,
/// at most [`MAX_FILES`] and [`MAX_DEPTH`] deep; links are not followed.
fn walk(dir: &Dir, prefix: &str, depth: usize, files: &mut Vec<String>) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut entries: Vec<_> = dir
        .entries()
        .map_err(|e| io("list the skill", e))?
        .filter_map(Result::ok)
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.file_type().ok()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, kind) in entries {
        if files.len() >= MAX_FILES {
            return Ok(());
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        match kind {
            Some(kind) if kind.is_file() => files.push(path),
            Some(kind) if kind.is_dir() => {
                let Ok(sub) = open_dir(dir, Path::new(&name)) else { continue };
                walk(&Dir::from_std_file(std::fs::File::from(sub)), &path, depth + 1, files)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A folder of `files` that git would take for a repository (`HEAD` beside `objects/` and
/// `refs/`, as a bare one is laid out), if any: its configuration could make a git command
/// run in it execute a program (`core.fsmonitor`), as a `.git` folder's could.
fn repository_inside(files: &[(String, Vec<u8>)]) -> Option<String> {
    let paths: std::collections::BTreeSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    paths.iter().find_map(|path| {
        let folder = path.strip_suffix("HEAD").filter(|f| f.is_empty() || f.ends_with('/'))?;
        let holds = |sub: &str| paths.iter().any(|p| p.starts_with(&format!("{folder}{sub}/")));
        (holds("objects") && holds("refs"))
            .then(|| if folder.is_empty() { ".".to_owned() } else { folder.trim_end_matches('/').to_owned() })
    })
}

/// A file's path in a skill, from an archive or a catalog: `/`-separated
/// plain names, none `.` or `..` or empty, at most [`MAX_DEPTH`] deep. None is
/// `.git` (in any case): a skill must not plant a repository's configuration, which git
/// commands run in its folder would obey (`core.fsmonitor` runs a command).
pub fn file_path(path: &str) -> Result<String, Error> {
    let path = path.trim_start_matches("./");
    let segments: Vec<&str> = path.split('/').collect();
    let plain = !path.is_empty()
        && segments.len() <= MAX_DEPTH
        && segments.iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && !s.eq_ignore_ascii_case(".git")
                && !s.contains(['\0', '\\'])
                && s.len() <= 255
        });
    if plain {
        Ok(path.to_owned())
    } else {
        Err(refuse(ErrorCode::InvalidInput, format!("{path:?} is not a file path of a skill")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn repo() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("repo")).unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn at() -> Vec<String> {
        vec!["repo".to_owned()]
    }

    #[test]
    fn a_skill_is_added_whole_listed_and_read() {
        let (dir, store) = repo();
        assert_eq!(store.list(&at()).unwrap(), vec![]);
        let files = vec![
            ("SKILL.md".to_owned(), b"---\nname: proof\ndescription: Prove things.\n---\n# Proof\n".to_vec()),
            ("scripts/run.py".to_owned(), b"#!/usr/bin/env python3\n".to_vec()),
        ];
        store.add(&at(), "proof", &files).unwrap();
        assert_eq!(
            store.list(&at()).unwrap(),
            vec![Found { name: "proof".into(), description: "Prove things.".into() }]
        );
        let read = store.read(&at(), "proof").unwrap();
        assert!(read.instructions.contains("# Proof"));
        assert_eq!(read.files, vec!["scripts/run.py".to_owned()]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("repo/.skill/proof/scripts/run.py"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o100, 0, "a script is executable");
        // Never over a folder that is there; nothing half-written is left.
        assert_eq!(store.add(&at(), "proof", &files).map_err(|e| e.code), Err(ErrorCode::InvalidInput));
        let left: Vec<_> = std::fs::read_dir(dir.path().join("repo/.skill")).unwrap().collect();
        assert_eq!(left.len(), 1);
    }

    /// The project's own processes write `.skill/`: a link there, to the
    /// folder or through it, leads nowhere.
    #[test]
    fn links_in_the_skills_folder_are_not_followed() {
        let (dir, store) = repo();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(outside.join("secret")).unwrap();
        std::fs::write(outside.join("secret/SKILL.md"), "---\ndescription: elsewhere\n---\n").unwrap();
        std::fs::create_dir(dir.path().join("repo/.skill")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.path().join("repo/.skill/linked")).unwrap();
        assert_eq!(store.list(&at()).unwrap(), vec![]);
        assert!(store.read(&at(), "linked").is_err());
        std::fs::remove_dir_all(dir.path().join("repo/.skill")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("repo/.skill")).unwrap();
        assert!(store.list(&at()).is_err());
        let files = vec![("SKILL.md".to_owned(), b"x".to_vec())];
        assert!(store.add(&at(), "new", &files).is_err());
        assert!(!outside.join("new").exists());
    }

    #[test]
    fn a_skill_may_not_hold_a_git_repository() {
        let file = |p: &str| (p.to_owned(), Vec::new());
        let bare = [
            file("SKILL.md"),
            file("evil/HEAD"),
            file("evil/config"),
            file("evil/objects/keep"),
            file("evil/refs/keep"),
        ];
        assert_eq!(repository_inside(&bare), Some("evil".to_owned()));
        let top = [file("SKILL.md"), file("HEAD"), file("objects/a"), file("refs/heads/main")];
        assert_eq!(repository_inside(&top), Some(".".to_owned()));
        // A file named HEAD alone, or folders named objects and refs elsewhere, is not one.
        let plain = [file("SKILL.md"), file("notes/HEAD"), file("data/objects/a"), file("refs/b")];
        assert_eq!(repository_inside(&plain), None);
    }

    #[test]
    fn a_files_path_is_plain() {
        assert_eq!(file_path("./references/a.md").unwrap(), "references/a.md");
        for bad in
            ["", "/etc/passwd", "a/../b", "a//b", "..", "a\\b", ".git/config", "x/.GIT/hooks/pre-commit"]
        {
            assert!(file_path(bad).is_err(), "{bad}");
        }
    }
}
