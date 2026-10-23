//! Filesystem service.
//!
//! Interface `fs` version 1. A resource path names a file or directory beneath
//! the service root.
//!
//! | Action | Effect class | Payload | Result |
//! | --- | --- | --- | --- |
//! | `read` | observation | `{}`, or `{"offset": n?, "length": n?}` | `{"content": <utf-8 text>, "offset": n, "total": n, "eof": bool}` |
//! | `list` | observation | `{}` | `{"entries": [<name>...]}` |
//! | `create` | reversible mutation | `{"content": <text>}` | `{}` |
//! | `append` | reversible mutation | `{"content": <text>, "at": n?}` | `{}` |
//! | `edit` | reversible mutation | `{"old": <text>, "new": <text>}` | `{"line": n}` |
//! | `remove` | **irreversible** | `{}` | `{}` |
//!
//! Every call moves at most [`MAX_FILE_BYTES`] of content, and a caller gets
//! no more than it asked for. A whole-file `read` (no `offset`, no `length`)
//! refuses a larger file. A **ranged** `read` returns at most `length` bytes
//! (default and most: [`MAX_FILE_BYTES`]) of the file from byte `offset`
//! (default 0), cut back to a character boundary when the range ends inside a
//! multi-byte character, and says where it stands — `offset`, the file's
//! `total` length, and `eof` when the content reached the end — so a caller
//! walks a large file in as many calls as it chooses, continuing from `offset
//! + content.len()`. The reply is JSON: a caller picks `length` so that the
//! escaped text fits the protocol's frame limit. `create` and `append` accept
//! at most [`MAX_FILE_BYTES`] per call. Calls are served one at a time: a
//! `read` of a FIFO blocks the service until a writer opens it, which the
//! kernel tests in `tests/tests/slice.rs` use as their stall.
//!
//! `remove` deletes one file (never a directory). The content is gone: there is
//! no inverse and no compensation. In the VM the `fs` endpoint grants
//! `remove` to the approvals guard alone, whose policy asks a person for each
//! use; the memory workspace's instance (`memory_files`) grants it to the
//! memory service.
//!
//! `create` never replaces an existing file, so its inverse is removal of the
//! file it created, assuming nobody else touched that path. `append` extends
//! an existing file and never creates one; with `at`, the file must be exactly
//! `at` bytes long at the time of the call, so a caller writing in batches
//! notices another writer (a check, not a lock: the window between it and the
//! write is the host's). Its inverse is truncating the file back to its length
//! before the call, under that same assumption. `edit` replaces the one
//! occurrence of `old` in an existing UTF-8 file of at most
//! [`MAX_EDIT_FILE_BYTES`] with `new`, and answers the 1-based line where it
//! starts; `old` found zero or several times is refused and nothing changes.
//! The new content is written to a fresh file beside it and renamed over it,
//! keeping its permission bits, so a reader sees the old or the new text,
//! never a mix (like `at`, not a lock: a write between the read and the
//! rename is lost). Its inverse is the edit from `new` back to `old`. This
//! service implements none of these inverses.
//!
//! Privileges: the directory it was started with, read-write, and nothing
//! else. The kernel has already checked the caller's capability against the
//! resource path; this service checks the path again at the moment of access.
//! Every open is an `openat2` beneath the root with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS`: the Linux kernel resolves the path and refuses `..`
//! out of the root, absolute paths and **any** symlink in it, including ones
//! created after the capability check: the kernel
//! checked the caller's scope against the literal path, and a link inside the
//! root would make a name in one scope reach a file in another.
//! A compromised instance can read and write everything under its root and
//! lie to its callers; beyond its root it reaches what its sandbox holds or,
//! as a project's host, what the projects' Linux user may.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use nucleus_protocol::{
    ACTION_PROVIDE, ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl,
    KERNEL_ENDPOINT, Payload, ResourceFrom, ResourcePath,
};
use nucleus_sdk::{Connection, Dispatch, Event};
use serde::{Deserialize, Serialize};

/// Largest file `read` returns and `create` accepts, in bytes.
pub const MAX_FILE_BYTES: u64 = 128 * 1024;

/// Largest file `edit` changes, in bytes; its `old` and `new` together are at
/// most [`MAX_FILE_BYTES`].
pub const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;

/// Startup options, from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub root: PathBuf,
    pub endpoint: EndpointName,
    /// Appends one JSON line per dispatch received. For audit and tests.
    pub access_log: Option<PathBuf>,
}

impl Options {
    /// Parses `--root <dir> [--endpoint <name>] [--access-log <file>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut root = None;
        let mut endpoint = "fs".to_owned();
        let mut access_log = None;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--endpoint" => endpoint = value,
                "--access-log" => access_log = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            root: root.ok_or("--root is required")?,
            endpoint: EndpointName::new(endpoint).map_err(|e| e.to_string())?,
            access_log,
        })
    }
}

/// The interface this service provides under `endpoint`.
pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    // The path is the resource (addressing), so it is stripped from the payload;
    // what remains is the payload each action decodes.
    let path = || serde_json::json!({"type": "string", "description": "Path relative to the workspace root, e.g. work/notes.txt"});
    let file = ResourceFrom::Segments { param: "path".to_owned() };
    let object = |props: serde_json::Value, required: serde_json::Value| serde_json::json!({"type": "object", "properties": props, "required": required});
    Ok(InterfaceDecl {
        endpoint,
        version: 1,
        description: "File paths are relative to this filesystem service's configured workspace. \
            Use the execution environment and workspace shown with the tool. \
            A path in another environment names a different file; files are not automatically synchronized. \
            Paths are not host absolute paths. Creating a file does not execute it or register a plugin."
            .into(),
        actions: BTreeMap::from([
            (
                ActionName::new("read")?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Read a UTF-8 text file from the workspace: the whole file (at most 131072 bytes), or the \
                     byte range starting at `offset` of at most `length` bytes, cut back to a character \
                     boundary. The reply gives the content, its offset, the file's total length and whether \
                     the end was reached, so a large file is read in several calls.",
                    object(
                        serde_json::json!({
                            "path": path(),
                            "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start at; default 0"},
                            "length": {"type": "integer", "minimum": 1, "maximum": MAX_FILE_BYTES, "description": "Most bytes to return; default and maximum 131072"}
                        }),
                        serde_json::json!(["path"]),
                    ),
                    file.clone(),
                ),
            ),
            (
                ActionName::new("list")?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "List the entries of a directory in the workspace.",
                    object(serde_json::json!({"path": path()}), serde_json::json!(["path"])),
                    file.clone(),
                ),
            ),
            (
                ActionName::new("create")?,
                ActionDecl::tool(
                    EffectClass::ReversibleMutation,
                    "Create a new text file. Fails if the file already exists.",
                    object(
                        serde_json::json!({"path": path(), "content": {"type": "string"}}),
                        serde_json::json!(["path", "content"]),
                    ),
                    file.clone(),
                ),
            ),
            (
                ActionName::new("append")?,
                ActionDecl::tool(
                    EffectClass::ReversibleMutation,
                    // `at` stays out of the tool: a model cannot know a file's byte
                    // length after its own writes, and a wrong guess refuses the
                    // append. Programs writing in batches still send it.
                    "Append text to the end of an existing file (at most 131072 bytes per call). Fails if \
                     the file does not exist.",
                    object(
                        serde_json::json!({
                            "path": path(),
                            "content": {"type": "string"}
                        }),
                        serde_json::json!(["path", "content"]),
                    ),
                    file.clone(),
                ),
            ),
            (
                ActionName::new("edit")?,
                ActionDecl::tool(
                    EffectClass::ReversibleMutation,
                    "Replace one exact piece of text in an existing text file. `old` must occur exactly once: \
                     include enough surrounding lines to make it unique. Answers the line where `new` starts.",
                    object(
                        serde_json::json!({
                            "path": path(),
                            "old": {"type": "string", "description": "The exact text to replace, including whitespace"},
                            "new": {"type": "string", "description": "The replacement text"}
                        }),
                        serde_json::json!(["path", "old", "new"]),
                    ),
                    file.clone(),
                ),
            ),
            (
                ActionName::new("remove")?,
                ActionDecl::tool(
                    EffectClass::Irreversible,
                    "Permanently delete one file. This cannot be undone.",
                    object(serde_json::json!({"path": path()}), serde_json::json!(["path"])),
                    file,
                ),
            ),
        ]),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    content: String,
}

/// `read`'s payload: nothing (the whole file), or a byte range.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendRequest {
    content: String,
    /// The file's expected length before the call.
    #[serde(default)]
    at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    old: String,
    new: String,
}

#[derive(Serialize)]
struct EditResponse {
    /// The 1-based line where the replacement starts.
    line: usize,
}

#[derive(Serialize)]
struct ReadResponse {
    content: String,
    /// Where `content` starts in the file, in bytes.
    offset: u64,
    /// The file's length in bytes.
    total: u64,
    /// `content` reached the end of the file.
    eof: bool,
}

/// The text of `bytes` read at `offset`: all of it, or — when a ranged read
/// ended inside a multi-byte character and the file goes on — the longest
/// UTF-8 prefix, so the caller continues from the character it cut. An
/// offset inside a character, a length too short for one, and a file that
/// is not UTF-8 text are refused.
fn text_of(e: std::string::FromUtf8Error, offset: u64, end_of_file: bool) -> Result<String, Error> {
    let error = e.utf8_error();
    let valid = error.valid_up_to();
    if valid == 0 && error.error_len().is_some() && offset > 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "offset is not on a character boundary"));
    }
    if error.error_len().is_some() || end_of_file {
        return Err(Error::new(ErrorCode::ServiceError, "read: the file is not UTF-8 text"));
    }
    if valid == 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "length is too short for the character at offset"));
    }
    let mut bytes = e.into_bytes();
    bytes.truncate(valid);
    String::from_utf8(bytes)
        .map_err(|_| Error::new(ErrorCode::ServiceError, "read: the file is not UTF-8 text"))
}

#[derive(Serialize)]
struct ListResponse {
    entries: Vec<String>,
}

#[derive(Serialize)]
struct Done {}

fn io_error(what: &str, e: &std::io::Error) -> Error {
    // `open_beneath` reports an escape attempt as a permission error.
    let code = match e.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::ServiceError,
    };
    Error::new(code, format!("{what}: {e}"))
}

/// How [`FsService::open_beneath`] opens a path.
#[derive(Clone, Copy)]
enum Access {
    Read,
    Append,
    CreateNew,
    Directory,
}

/// The service's whole authority over the host filesystem.
pub struct FsService {
    root: Dir,
}

impl FsService {
    /// Opens the root once. This is the only use of ambient authority.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        Ok(Self { root: Dir::open_ambient_dir(root, ambient_authority())? })
    }

    fn relative(resource: &ResourcePath) -> PathBuf {
        if resource.segments().is_empty() { PathBuf::from(".") } else { resource.segments().iter().collect() }
    }

    /// Opens `path` beneath the root; the kernel resolves it and refuses a
    /// symlink in any component, so what is opened is what the caller's
    /// scope was checked against.
    fn open_beneath(&self, path: &Path, access: Access) -> std::io::Result<std::fs::File> {
        Self::open_at(&self.root, path, access)
    }

    fn open_at(dir: impl rustix::fd::AsFd, path: &Path, access: Access) -> std::io::Result<std::fs::File> {
        use rustix::fs::{Mode, OFlags, ResolveFlags};
        // `openat2` refuses a mode unless it creates.
        let (flags, mode) = match access {
            Access::Read => (OFlags::RDONLY, Mode::empty()),
            Access::Append => (OFlags::WRONLY | OFlags::APPEND, Mode::empty()),
            Access::CreateNew => (OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, Mode::from_raw_mode(0o666)),
            Access::Directory => (OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()),
        };
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        let flags = flags | OFlags::CLOEXEC;
        match rustix::fs::openat2(dir, path, flags, mode, resolve) {
            Ok(fd) => Ok(fd.into()),
            Err(e) if e == rustix::io::Errno::LOOP || e == rustix::io::Errno::XDEV => {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the path crosses a symbolic link or leaves the root",
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn entries(&self, path: &Path) -> std::io::Result<Vec<String>> {
        Dir::from_std_file(self.open_beneath(path, Access::Directory)?)
            .entries()?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect()
    }

    /// The parent directory, resolved like any other path, and the last component.
    fn parent_of<'a>(&self, path: &'a Path) -> std::io::Result<(Dir, &'a Path)> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file"));
        };
        let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
        Ok((Dir::from_std_file(self.open_beneath(parent, Access::Directory)?), Path::new(name)))
    }

    /// The last component is unlinked, never followed, and `remove_file`
    /// refuses directories.
    fn remove(&self, path: &Path) -> std::io::Result<()> {
        let (dir, name) = self.parent_of(path)?;
        dir.remove_file(name)
    }

    /// Reads the file through its opened parent, writes the new text to a
    /// fresh file in that directory and renames it over the name: the rename
    /// replaces the directory entry and never follows it.
    fn edit(&self, path: &Path, request: &EditRequest) -> Result<usize, Error> {
        let (dir, name) = self.parent_of(path).map_err(|e| io_error("edit", &e))?;
        let file = Self::open_at(&dir, name, Access::Read).map_err(|e| io_error("edit", &e))?;
        let metadata = file.metadata().map_err(|e| io_error("stat", &e))?;
        if !metadata.is_file() {
            return Err(Error::new(ErrorCode::InvalidInput, "edit: not a file"));
        }
        if metadata.len() > MAX_EDIT_FILE_BYTES {
            return Err(Error::new(ErrorCode::InvalidInput, "edit: the file exceeds the edit limit"));
        }
        let mut content = String::new();
        (&file)
            .take(MAX_EDIT_FILE_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|_| Error::new(ErrorCode::ServiceError, "edit: the file is not UTF-8 text"))?;
        let at = match content.match_indices(&request.old).take(2).collect::<Vec<_>>()[..] {
            [(at, _)] => at,
            [] => return Err(Error::new(ErrorCode::InvalidInput, "edit: `old` was not found in the file")),
            _ => {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "edit: `old` occurs more than once; include more surrounding text",
                ));
            }
        };
        let edited = format!("{}{}{}", &content[..at], request.new, &content[at + request.old.len()..]);
        if edited.len() as u64 > MAX_EDIT_FILE_BYTES {
            return Err(Error::new(ErrorCode::InvalidInput, "edit: the result exceeds the edit limit"));
        }
        let temporary = PathBuf::from(format!(
            ".{}.edit-{}-{}",
            name.display(),
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        ));
        let written = (|| {
            let mut out =
                dir.open_with(&temporary, cap_std::fs::OpenOptions::new().write(true).create_new(true))?;
            out.write_all(edited.as_bytes())?;
            out.set_permissions(cap_std::fs::Permissions::from_std(metadata.permissions()))?;
            out.sync_all()?;
            dir.rename(&temporary, &dir, name)
        })();
        if let Err(e) = written {
            let _ = dir.remove_file(&temporary);
            return Err(io_error("edit", &e));
        }
        Ok(content[..at].matches('\n').count() + 1)
    }

    /// Performs one action. Payloads are untrusted input.
    pub fn handle(&self, action: &str, resource: &ResourcePath, payload: &Payload) -> Result<Payload, Error> {
        let path = Self::relative(resource);
        match action {
            "read" => {
                let request: ReadRequest = payload.decode()?;
                let ranged = request.offset.is_some() || request.length.is_some();
                let offset = request.offset.unwrap_or(0);
                let length = request.length.unwrap_or(MAX_FILE_BYTES);
                if length > MAX_FILE_BYTES {
                    return Err(Error::new(ErrorCode::InvalidInput, "length exceeds the read limit"));
                }
                let mut file = self.open_beneath(&path, Access::Read).map_err(|e| io_error("open", &e))?;
                let total = file.metadata().map_err(|e| io_error("stat", &e))?.len();
                if !ranged && total > MAX_FILE_BYTES {
                    return Err(Error::new(ErrorCode::ServiceError, "file exceeds the read limit"));
                }
                if offset > total {
                    return Err(Error::new(ErrorCode::InvalidInput, "offset is past the end of the file"));
                }
                if offset > 0 {
                    file.seek(SeekFrom::Start(offset)).map_err(|e| io_error("seek", &e))?;
                }
                let mut bytes = Vec::new();
                (&mut file).take(length).read_to_end(&mut bytes).map_err(|e| io_error("read", &e))?;
                let end_of_file = offset + bytes.len() as u64 >= total;
                let content = match String::from_utf8(bytes) {
                    Ok(content) => content,
                    Err(e) => text_of(e, offset, end_of_file)?,
                };
                let eof = offset + content.len() as u64 >= total;
                Payload::encode(&ReadResponse { content, offset, total, eof })
            }
            "list" => {
                payload.decode::<Empty>()?;
                let mut entries = self.entries(&path).map_err(|e| io_error("list", &e))?;
                entries.sort();
                Payload::encode(&ListResponse { entries })
            }
            "create" => {
                let request: CreateRequest = payload.decode()?;
                if request.content.len() as u64 > MAX_FILE_BYTES {
                    return Err(Error::new(ErrorCode::InvalidInput, "content exceeds the create limit"));
                }
                let mut file =
                    self.open_beneath(&path, Access::CreateNew).map_err(|e| io_error("create", &e))?;
                file.write_all(request.content.as_bytes()).map_err(|e| io_error("write", &e))?;
                file.sync_all().map_err(|e| io_error("sync", &e))?;
                Payload::encode(&Done {})
            }
            "append" => {
                let request: AppendRequest = payload.decode()?;
                if request.content.len() as u64 > MAX_FILE_BYTES {
                    return Err(Error::new(ErrorCode::InvalidInput, "content exceeds the append limit"));
                }
                // No `create`: a missing file is an error, never a new file.
                let mut file =
                    self.open_beneath(&path, Access::Append).map_err(|e| io_error("append", &e))?;
                if let Some(at) = request.at {
                    let len = file.metadata().map_err(|e| io_error("stat", &e))?.len();
                    if len != at {
                        return Err(Error::new(
                            ErrorCode::ServiceError,
                            format!("append: the file is {len} bytes long, not {at}"),
                        ));
                    }
                }
                file.write_all(request.content.as_bytes()).map_err(|e| io_error("write", &e))?;
                file.sync_all().map_err(|e| io_error("sync", &e))?;
                Payload::encode(&Done {})
            }
            "edit" => {
                let request: EditRequest = payload.decode()?;
                if request.old.is_empty() {
                    return Err(Error::new(ErrorCode::InvalidInput, "edit: `old` is empty"));
                }
                if (request.old.len() + request.new.len()) as u64 > MAX_FILE_BYTES {
                    return Err(Error::new(
                        ErrorCode::InvalidInput,
                        "edit: `old` and `new` exceed the edit limit",
                    ));
                }
                let line = self.edit(&path, &request)?;
                Payload::encode(&EditResponse { line })
            }
            "remove" => {
                payload.decode::<Empty>()?;
                self.remove(&path).map_err(|e| io_error("remove", &e))?;
                Payload::encode(&Done {})
            }
            _ => Err(Error::new(ErrorCode::InvalidInput, "unknown action")),
        }
    }
}

/// One line of the access log: who asked for what, and how large the request
/// was (the payload's JSON size, so a run of ranged reads or appends is
/// accountable by volume).
#[derive(Serialize)]
struct AccessRecord<'a> {
    caller: u64,
    action: &'a str,
    resource: &'a ResourcePath,
    payload_bytes: usize,
}

fn log_access(log: &mut Option<std::fs::File>, dispatch: &Dispatch) -> Result<(), String> {
    let Some(file) = log else { return Ok(()) };
    let record = AccessRecord {
        caller: dispatch.caller.0,
        action: dispatch.action.as_str(),
        resource: &dispatch.resource,
        payload_bytes: dispatch.payload.len(),
    };
    let mut line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    line.push('\n');
    file.write_all(line.as_bytes()).map_err(|e| format!("access log: {e}"))
}

/// Provides the endpoint and serves dispatches until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let service = FsService::open(&options.root).map_err(|e| format!("open root: {e}"))?;
    let mut log = match &options.access_log {
        None => None,
        Some(path) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("open access log: {e}"))?,
        ),
    };
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    let interface = interface(options.endpoint.clone()).map_err(|e| e.to_string())?;
    kernel.provide(authority, interface).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    // As a project's host, its router forwards nothing before this.
    kernel.announce_ready().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

    loop {
        match kernel.next_event().map_err(|e| e.to_string())? {
            Event::Dispatch(dispatch) => {
                // A failing audit log stops the service rather than serving unrecorded.
                log_access(&mut log, &dispatch)?;
                let result = service.handle(dispatch.action.as_str(), &dispatch.resource, &dispatch.payload);
                kernel.reply(dispatch.id, result).map_err(|e| e.to_string())?;
            }
            // Dispatches are served one at a time to completion, so there is
            // never a running one to cancel.
            Event::CancelDispatch(_) | Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn path(segments: &[&str]) -> ResourcePath {
        ResourcePath::new(segments.iter().copied()).unwrap()
    }

    fn empty() -> Payload {
        Payload::encode(&serde_json::json!({})).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, FsService) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("root/work")).unwrap();
        std::fs::write(dir.path().join("root/work/a.txt"), "inside").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
        let service = FsService::open(&dir.path().join("root")).unwrap();
        (dir, service)
    }

    #[test]
    fn reads_lists_and_creates_beneath_the_root() {
        let (dir, service) = fixture();
        let read: serde_json::Value =
            service.handle("read", &path(&["work", "a.txt"]), &empty()).unwrap().decode().unwrap();
        assert_eq!(read["content"], "inside");

        let content = Payload::encode(&serde_json::json!({"content": "new"})).unwrap();
        service.handle("create", &path(&["work", "b.txt"]), &content).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("root/work/b.txt")).unwrap(), "new");
        let again = service.handle("create", &path(&["work", "b.txt"]), &content);
        assert!(again.is_err(), "create never replaces a file");

        let list: serde_json::Value =
            service.handle("list", &path(&["work"]), &empty()).unwrap().decode().unwrap();
        assert_eq!(list["entries"], serde_json::json!(["a.txt", "b.txt"]));
    }

    #[test]
    fn symlinks_cannot_lead_outside_the_root() {
        let (dir, service) = fixture();
        let work = dir.path().join("root/work");
        std::os::unix::fs::symlink("../../outside.txt", work.join("relative")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), work.join("absolute")).unwrap();
        std::os::unix::fs::symlink("../..", work.join("up")).unwrap();
        std::os::unix::fs::symlink("a.txt", work.join("benign")).unwrap();

        for escape in [vec!["work", "relative"], vec!["work", "absolute"], vec!["work", "up", "outside.txt"]]
        {
            let got = service.handle("read", &path(&escape), &empty());
            assert!(got.is_err(), "{escape:?} escaped: {got:?}");
        }
        let content = Payload::encode(&serde_json::json!({"content": "x"})).unwrap();
        assert!(service.handle("create", &path(&["work", "up", "planted.txt"]), &content).is_err());
        assert!(!dir.path().join("planted.txt").exists());
        assert!(service.handle("remove", &path(&["work", "up", "outside.txt"]), &empty()).is_err());
        assert!(dir.path().join("outside.txt").exists(), "a link cannot aim `remove` outside the root");
        // A scope is a path prefix, so a link inside the root must not turn
        // a name in one scope into a file in another.
        {
            std::fs::create_dir(dir.path().join("root/other")).unwrap();
            std::fs::write(dir.path().join("root/other/b.txt"), "theirs").unwrap();
            std::os::unix::fs::symlink("../other/b.txt", work.join("across")).unwrap();
            std::os::unix::fs::symlink("../other", work.join("dir")).unwrap();
            let x = Payload::encode(&serde_json::json!({"content": "x"})).unwrap();
            for (action, target, payload) in [
                ("read", vec!["work", "benign"], empty()),
                ("read", vec!["work", "across"], empty()),
                ("append", vec!["work", "across"], x.clone()),
                ("edit", vec!["work", "across"], edit("theirs", "mine")),
                ("edit", vec!["work", "dir", "b.txt"], edit("theirs", "mine")),
                ("read", vec!["work", "dir", "b.txt"], empty()),
                ("list", vec!["work", "dir"], empty()),
                ("create", vec!["work", "dir", "new.txt"], x),
                ("remove", vec!["work", "dir", "b.txt"], empty()),
            ] {
                let got = service.handle(action, &path(&target), &payload);
                assert!(got.is_err(), "{action} {target:?} crossed a link: {got:?}");
            }
            assert_eq!(std::fs::read_to_string(dir.path().join("root/other/b.txt")).unwrap(), "theirs");
            assert!(!dir.path().join("root/other/new.txt").exists());
        }
    }

    fn edit(old: &str, new: &str) -> Payload {
        Payload::encode(&serde_json::json!({"old": old, "new": new})).unwrap()
    }

    #[test]
    fn edit_replaces_exactly_one_occurrence_and_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, service) = fixture();
        let file = dir.path().join("root/work/a.txt");
        std::fs::write(&file, "one\ntwo\ntwo\nthree\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let a = path(&["work", "a.txt"]);
        let got = service.handle("edit", &a, &edit("three", "3")).unwrap();
        assert_eq!(got.decode::<serde_json::Value>().unwrap(), serde_json::json!({"line": 4}));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\ntwo\n3\n");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o750);
        for refused in [edit("two", "2"), edit("four", "4"), edit("", "x")] {
            assert!(service.handle("edit", &a, &refused).is_err());
        }
        assert!(service.handle("edit", &path(&["work", "missing.txt"]), &edit("a", "b")).is_err());
        assert!(service.handle("edit", &path(&["work"]), &edit("a", "b")).is_err(), "not a directory");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\ntwo\ntwo\n3\n",
            "a refused edit changes nothing"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path().join("root/work"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.txt"], "no temporary file is left behind");
    }

    #[test]
    fn remove_deletes_one_file_beneath_the_root_and_nothing_else() {
        let (dir, service) = fixture();
        service.handle("remove", &path(&["work", "a.txt"]), &empty()).unwrap();
        assert!(!dir.path().join("root/work/a.txt").exists());
        assert!(service.handle("remove", &path(&["work", "a.txt"]), &empty()).is_err(), "already gone");
        assert!(service.handle("remove", &path(&["work"]), &empty()).is_err(), "not a directory");
        assert!(dir.path().join("root/work").is_dir());
        assert!(dir.path().join("outside.txt").exists());
    }

    #[test]
    fn a_ranged_read_is_bounded_and_says_where_it_stands() {
        let (dir, service) = fixture();
        // 13 bytes: `é` is bytes 1–2, `ö` bytes 8–9.
        std::fs::write(dir.path().join("root/work/u.txt"), "héllo wörld").unwrap();
        let read = |offset: Option<u64>, length: Option<u64>| -> Result<serde_json::Value, Error> {
            let mut payload = serde_json::Map::new();
            if let Some(o) = offset {
                payload.insert("offset".into(), o.into());
            }
            if let Some(l) = length {
                payload.insert("length".into(), l.into());
            }
            let payload = Payload::encode(&payload).unwrap();
            service.handle("read", &path(&["work", "u.txt"]), &payload).and_then(|p| p.decode())
        };
        let whole = read(None, None).unwrap();
        assert_eq!(
            whole,
            serde_json::json!({"content": "héllo wörld", "offset": 0, "total": 13, "eof": true}),
            "a whole-file read says where it stands"
        );
        // A range ending inside `é` hands back what fits; the caller continues from offset + length.
        let first = read(None, Some(2)).unwrap();
        assert_eq!((first["content"].as_str(), first["eof"].as_bool()), (Some("h"), Some(false)));
        assert_eq!(read(Some(1), Some(2)).unwrap()["content"], "é");
        let tail = read(Some(3), Some(100)).unwrap();
        assert_eq!(
            (
                tail["content"].as_str(),
                tail["offset"].as_u64(),
                tail["total"].as_u64(),
                tail["eof"].as_bool()
            ),
            (Some("llo wörld"), Some(3), Some(13), Some(true))
        );
        let end = read(Some(13), Some(5)).unwrap();
        assert_eq!((end["content"].as_str(), end["eof"].as_bool()), (Some(""), Some(true)), "at the end");
        // Refused: more than the limit, an offset past the end or inside a
        // character, a length too short for the character at the offset.
        let code = |r: Result<serde_json::Value, Error>| r.map_err(|e| e.code);
        assert_eq!(code(read(None, Some(MAX_FILE_BYTES + 1))), Err(ErrorCode::InvalidInput));
        assert_eq!(code(read(Some(14), None)), Err(ErrorCode::InvalidInput));
        assert_eq!(code(read(Some(2), Some(4))), Err(ErrorCode::InvalidInput));
        assert_eq!(code(read(Some(1), Some(1))), Err(ErrorCode::InvalidInput));
        // A file over the limit: a whole-file read still refuses it; ranges walk it.
        let big = "x".repeat(MAX_FILE_BYTES as usize + 10);
        std::fs::write(dir.path().join("root/work/big.txt"), &big).unwrap();
        let whole_big = service.handle("read", &path(&["work", "big.txt"]), &empty());
        assert_eq!(whole_big.map_err(|e| e.code), Err(ErrorCode::ServiceError));
        let last = Payload::encode(&serde_json::json!({"offset": MAX_FILE_BYTES})).unwrap();
        let last: serde_json::Value =
            service.handle("read", &path(&["work", "big.txt"]), &last).unwrap().decode().unwrap();
        assert_eq!((last["content"].as_str().map(str::len), last["eof"].as_bool()), (Some(10), Some(true)));
        let full = Payload::encode(&serde_json::json!({"length": MAX_FILE_BYTES})).unwrap();
        let full: serde_json::Value =
            service.handle("read", &path(&["work", "big.txt"]), &full).unwrap().decode().unwrap();
        assert_eq!(
            full["content"].as_str().map(str::len),
            Some(MAX_FILE_BYTES as usize),
            "never more than asked"
        );
        assert_eq!(full["eof"], false);
    }

    #[test]
    fn append_extends_an_existing_file_and_never_creates_one() {
        let (dir, service) = fixture();
        let payload = |text: &str, at: Option<u64>| {
            let mut object = serde_json::Map::new();
            object.insert("content".into(), text.into());
            if let Some(at) = at {
                object.insert("at".into(), at.into());
            }
            Payload::encode(&object).unwrap()
        };
        let a = || std::fs::read_to_string(dir.path().join("root/work/a.txt")).unwrap();
        service.handle("append", &path(&["work", "a.txt"]), &payload(" and more", None)).unwrap();
        assert_eq!(a(), "inside and more");
        // With `at`, the file must be exactly that long: a batch writer notices another writer.
        service.handle("append", &path(&["work", "a.txt"]), &payload("!", Some(15))).unwrap();
        assert_eq!(a(), "inside and more!");
        assert!(service.handle("append", &path(&["work", "a.txt"]), &payload("?", Some(15))).is_err());
        assert_eq!(a(), "inside and more!", "a refused append writes nothing");
        // Never creates, never a directory, bounded per call.
        assert!(service.handle("append", &path(&["work", "new.txt"]), &payload("x", None)).is_err());
        assert!(!dir.path().join("root/work/new.txt").exists());
        assert!(service.handle("append", &path(&["work"]), &payload("x", None)).is_err());
        let huge = payload(&"x".repeat(MAX_FILE_BYTES as usize + 1), None);
        let got = service.handle("append", &path(&["work", "a.txt"]), &huge);
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::InvalidInput));
        assert_eq!(a(), "inside and more!");
        let declared = interface(EndpointName::new("fs").unwrap()).unwrap();
        let tool = declared.actions[&ActionName::new("append").unwrap()].tool.clone().unwrap();
        assert!(tool.params["properties"].get("at").is_none(), "a model is never asked for a byte length");
    }

    #[test]
    fn payloads_are_validated() {
        let (_dir, service) = fixture();
        let extra = Payload::encode(&serde_json::json!({"content": "x", "mode": "0777"})).unwrap();
        let got = service.handle("create", &path(&["work", "c.txt"]), &extra);
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::InvalidInput));
        let huge = Payload::encode(&serde_json::json!({"content": "x".repeat(200 * 1024)})).unwrap();
        let got = service.handle("create", &path(&["work", "d.txt"]), &huge);
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::InvalidInput));
    }
}
