//! The `diffs` service: `git diff` of one repository, for the window's Review
//! (interface in `nucleus-diffs-interface`): in the VM a project's `repo/`,
//! named by `--repository`. It never searches the workspace for
//! repositories: another one there, a worktree included, is git's and the
//! person's. It only displays: it never creates, initialises, commits,
//! stages, locks or writes to a repository, and keeps no store of its own.
//!
//! # What runs
//!
//! `repositories` lists that repository when it holds a `.git` directory or
//! file (reached through no symbolic link) and asks it for its branch with
//! `git rev-parse`.
//! `changes` runs `git status --porcelain=v2` (git's own view, refreshed in
//! memory and never written back) and `git diff <base> --numstat`, where the
//! base is `HEAD`, or the empty tree before the first commit; a file is listed
//! when both agree it changed, plus the untracked files git does not ignore.
//! (Either alone is wrong: the diff lists a binary file whose only change is
//! its timestamp, and status lists a staged change the working tree undid.)
//! For the files of one page it reads the `-U0` patch of each tracked text
//! file big enough to hold a line over [`MAX_LINE_BYTES`], and reads each
//! untracked file itself, with no git run per file: every line of it is
//! added, it is binary by git's test (a NUL byte in its first 8000 bytes, or
//! over 512 MiB), and a symbolic link is its target, as git stores it.
//! Attributes and end-of-line conversion apply to tracked files only. `file`
//! runs `git status` for that one path and then reads it the same way: a
//! tracked path through its `-U0` patch, an untracked one itself, only as far
//! as the page reaches.
//!
//! Every run is `/usr/bin/git` (in the VM, the image's), never a `PATH`
//! lookup, in the repository directory this service opened and checked (the
//! child changes into the descriptor), with nothing inherited: no system
//! configuration, `GIT_OPTIONAL_LOCKS=0`, no prompts, no lazy fetch, `LC_ALL=C`,
//! `PATH=/usr/bin:/bin`. Its home is the person's when the operator names it
//! (`--home`), so their global configuration and excludes count as
//! they do for their own git; otherwise an empty one, with no global
//! configuration. Command-line settings override the repository's and the
//! global configuration: no fsmonitor hook, no untracked cache, no external
//! diff or text conversion, a submodule shown as its two commits (the other
//! formats start a git inside the submodule, which runs the submodule's own
//! diff programs), no colour, no renames, and `diff.autoRefreshIndex=false`
//! (without it `git diff` rewrites `.git/index` even with optional locks off).
//! Each run has its own process group and a hard deadline ([`git::RUN_TIMEOUT`], or
//! less when the caller's deadline is nearer); the group is killed when it ends
//! or overruns. What git prints is read up to a bound; more is an error, never
//! a shortened answer, and a patch streams through one line at a time.
//!
//! # What cannot be switched off, and what it reaches
//!
//! The repository's own configuration and attributes, and the global
//! configuration, still steer git where no override exists. Above all, a
//! clean or process **filter** (`filter.*`) runs a program the configuration
//! names whenever git hashes a working-tree file, as it must to show the same
//! diff the person's git would. Git and everything it starts run as this
//! service does: as a project's host, as the projects' user,
//! so a filter is the project's own code with the project's own reach, and
//! nothing here narrows it. What a filter can do is what that user can, which
//! includes deciding what the file it filters shows as. A process it starts
//! that leaves git's process group lives until the service stops.
//!
//! # What the service holds
//!
//! The workspace, opened once and never written by this service; every other
//! path is resolved beneath it by the Linux kernel with no symbolic link
//! followed (`openat2`). An empty temporary directory, git's `TMPDIR`. With
//! `--home`, the person's home, for git's global configuration. No
//! credentials, no
//! capability beyond providing its endpoint. Requests are untrusted: a
//! repository must be one `repositories` would list, a path a plain relative
//! name that crosses no symbolic link (unless git lists it as tracked: git
//! never reads a tracked file through one), and the resource `[]`. What it
//! reads from the repositories is untrusted input and only ever displayed.
//!
//! # Limits and how a reply stays whole
//!
//! Pages follow the interface (`next`, `next_line`): nothing is cut. A
//! `changes` page ends early rather than run out of the caller's time. What
//! cannot be paged is refused whole, never answered in part: a path over
//! [`workspace::MAX_PATH_BYTES`] and a repository whose change list is over
//! [`MAX_LISTING_BYTES`]. Lines
//! are split on `\n` only, so a `\r` stays part of its line; bytes that are not
//! UTF-8 are shown as U+FFFD. A line that lost or gained its final newline
//! shows as deleted and added with the same text (the interface has no mark
//! for it). A file name that is not UTF-8 makes `changes` fail, naming it. A path that is
//! both deleted from the index and present untracked (`git rm --cached`) is
//! shown as `git diff HEAD` shows it: deleted. A path whose type changed (a
//! file became a link) shows its old lines deleted and its new ones added.
//!
//! One request is served at a time. A request's time counts from when it can
//! have arrived, also when it waited behind another; one whose caller has too
//! little left is answered `Timeout` without running git.

mod diff;
mod git;
mod untracked;
mod workspace;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::ops::Bound;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nucleus_diffs_interface::{
    ACTION_CHANGES, ACTION_FILE, ACTION_REPOSITORIES, Changes, ChangesRequest, ENDPOINT, FileDiff,
    FileRequest, FileSummary, MAX_LINE_BYTES, PAGE_BYTES, PAGE_FILES, Repositories, Repository, Status,
    interface,
};
use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Dispatch, Event as KernelEvent};
use serde::Deserialize;

use crate::diff::{
    Event, Page, Patch, Record, Scan, StatusReport, parse_numstat, parse_status, summary_cost,
};
use crate::git::{Exit, Flow, Git};
use crate::workspace::{Content, Workspace, check_parents, segments};

/// Kept back from the caller's deadline so the reply arrives before it.
const REPLY_MARGIN: Duration = Duration::from_millis(500);
/// Less time than this left, and git is not started at all.
const MIN_BUDGET: Duration = Duration::from_secs(1);
/// Most bytes read from `git status` or `git diff --numstat`: a repository
/// with more changed paths than this is refused, not listed in part.
pub const MAX_LISTING_BYTES: usize = 32 << 20;
/// Most bytes read from a command that prints a line or two per input.
const MAX_SHORT_OUTPUT: usize = 64 << 10;

/// Options shared by every patch this service reads.
const PATCH: &[&str] = &["-U0", "--inter-hunk-context=0", "--no-ext-diff", "--no-textconv", "--no-color"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The workspace.
    pub root: PathBuf,
    /// The person's home, where their global git configuration and excludes
    /// live (in the VM, the projects' shared home the shell uses as `HOME`). Git
    /// reads it and nothing writes there. Without it git has no global
    /// configuration.
    pub home: Option<PathBuf>,
    /// The one repository Review shows, relative to the root
    /// (`--repository`); the root itself when none is named. A project's host
    /// names `repo`.
    pub repository: String,
    pub endpoint: EndpointName,
}

impl Options {
    /// Parses `--root <workspace> [--home <dir>] [--repository <dir>] [--endpoint <name>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut root = None;
        let mut home = None;
        let mut repository = String::new();
        let mut endpoint = ENDPOINT.to_owned();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--home" => home = Some(PathBuf::from(value)),
                "--repository" => {
                    workspace::segments(&value, "repository").map_err(|e| e.message)?;
                    repository = value;
                }
                "--endpoint" => endpoint = value,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if home.as_ref().is_some_and(|home: &PathBuf| !home.is_absolute()) {
            return Err("--home must be an absolute path".to_owned());
        }
        Ok(Self {
            root: root.ok_or("--root is required")?,
            home,
            repository,
            endpoint: EndpointName::new(&endpoint).map_err(|e| e.to_string())?,
        })
    }
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> Error {
    Error::new(code, message.into())
}

fn os<'a>(args: &[&'a str]) -> Vec<&'a OsStr> {
    args.iter().map(|a| OsStr::new(*a)).collect()
}

/// A changed path, before its page is filled in.
#[derive(Clone, Debug)]
enum Change {
    Tracked { status: Status, head: Option<String>, lines: Option<(u64, u64)> },
    Untracked,
}

/// The payload of `repositories`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

/// The service: the workspace and the way it runs git.
pub struct DiffService {
    workspace: Workspace,
    git: Git,
}

impl DiffService {
    /// Opens the workspace `root`; `home` is the person's (see
    /// [`Options::home`]), `repository` the one Review shows ([`Options::repository`]).
    pub fn open(root: &Path, home: Option<&Path>, repository: &str) -> Result<Self, String> {
        workspace::segments(repository, "repository").map_err(|e| e.message)?;
        let workspace = Workspace::open(root, repository)
            .map_err(|e| format!("cannot open the workspace {}: {e}", root.display()))?;
        let git = Git::new(workspace.path(), home)?;
        Ok(Self { workspace, git })
    }

    /// Serves one request. `deadline` is when the reply must be ready.
    pub fn serve(
        &self,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline: Instant,
    ) -> Result<Payload, Error> {
        if !resource.segments().is_empty() {
            return Err(refuse(
                ErrorCode::InvalidInput,
                "the resource of every diffs action is [], the workspace",
            ));
        }
        match action {
            ACTION_REPOSITORIES => {
                let NoArguments {} = payload.decode()?;
                Payload::encode(&self.repositories(deadline)?)
            }
            ACTION_CHANGES => Payload::encode(&self.changes(&payload.decode()?, deadline)?),
            ACTION_FILE => Payload::encode(&self.file(&payload.decode()?, deadline)?),
            _ => Err(refuse(ErrorCode::InvalidInput, "unknown action")),
        }
    }

    /// The repository Review shows, when there is one: none before the
    /// project's repository is made.
    pub fn repositories(&self, deadline: Instant) -> Result<Repositories, Error> {
        let path = self.workspace.repository().to_owned();
        let repository = match self.workspace.open_repository(&path) {
            Ok(fd) => fd,
            Err(e) if e.code == ErrorCode::InvalidInput => {
                return Ok(Repositories { repositories: Vec::new() });
            }
            Err(e) => return Err(e),
        };
        let branch = self
            .branch(repository.as_fd(), deadline)
            .map_err(|e| Error::new(e.code, format!("the repository {path:?}: {}", e.message)))?;
        Ok(Repositories { repositories: vec![Repository { path, branch }] })
    }

    /// The checked-out branch; `None` when `HEAD` is detached, before the
    /// first commit, or unreadable (then `changes` says why).
    fn branch(&self, repository: BorrowedFd<'_>, deadline: Instant) -> Result<Option<String>, Error> {
        let args = os(&["rev-parse", "--symbolic-full-name", "--verify", "-q", "HEAD"]);
        let (exit, out) = self.git.output(repository, &args, None, MAX_SHORT_OUTPUT, deadline)?;
        if exit.code != Some(0) {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&out);
        Ok(name.trim_end_matches('\n').strip_prefix("refs/heads/").map(str::to_owned))
    }

    fn status(
        &self,
        repository: BorrowedFd<'_>,
        path: Option<&str>,
        deadline: Instant,
    ) -> Result<StatusReport, Error> {
        let mut args = os(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--no-ahead-behind",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
            "--no-renames",
        ]);
        if let Some(path) = path {
            args.extend([OsStr::new("--"), OsStr::new(path)]);
        }
        let (exit, out) = self.git.output(repository, &args, None, MAX_LISTING_BYTES, deadline)?;
        if exit.code != Some(0) {
            return Err(exit.failure("status"));
        }
        parse_status(&out)
    }

    /// What the working tree is compared with: `HEAD`'s commit, or before the
    /// first commit the empty tree in the repository's own object format.
    fn base(
        &self,
        repository: BorrowedFd<'_>,
        head: Option<&str>,
        deadline: Instant,
    ) -> Result<String, Error> {
        if let Some(commit) = head {
            return Ok(commit.to_owned());
        }
        let args = os(&["hash-object", "-t", "tree", "/dev/null"]);
        let (exit, out) = self.git.output(repository, &args, None, MAX_SHORT_OUTPUT, deadline)?;
        if exit.code != Some(0) {
            return Err(exit.failure("hash-object"));
        }
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    fn numstat(
        &self,
        repository: BorrowedFd<'_>,
        base: &str,
        deadline: Instant,
    ) -> Result<BTreeMap<String, Option<(u64, u64)>>, Error> {
        let args = os(&[
            "diff",
            "--numstat",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--ignore-submodules=dirty",
            base,
            "--",
        ]);
        let (exit, out) = self.git.output(repository, &args, None, MAX_LISTING_BYTES, deadline)?;
        if exit.code != Some(0) {
            return Err(exit.failure("diff --numstat"));
        }
        parse_numstat(&out)
    }

    pub fn changes(&self, request: &ChangesRequest, deadline: Instant) -> Result<Changes, Error> {
        let fd = self.workspace.open_repository(&request.repository)?;
        let repository = fd.as_fd();
        let status = self.status(repository, None, deadline)?;
        let base = self.base(repository, status.head.as_deref(), deadline)?;
        let counts = self.numstat(repository, &base, deadline)?;
        let mut changed: BTreeMap<String, Change> = BTreeMap::new();
        for record in status.records {
            match record {
                // Listed by status but not by the diff: a staged change undone
                // in the working tree, or a file that is only stat-dirty.
                Record::Tracked { path, status, head } => {
                    if let Some(lines) = counts.get(&path) {
                        changed.insert(path, Change::Tracked { status, head, lines: *lines });
                    }
                }
                Record::Untracked { path } => {
                    changed.entry(path).or_insert(Change::Untracked);
                }
            }
        }
        let from = request.after.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        let mut page = Vec::new();
        let mut bytes = 0;
        let mut next = None;
        for (path, change) in changed.range::<str, _>((from, Bound::Unbounded)) {
            let cost = summary_cost(path);
            if page.len() == PAGE_FILES || (!page.is_empty() && bytes + cost > PAGE_BYTES) {
                next = page.last().map(|(path, _): &(String, Change)| path.clone());
                break;
            }
            bytes += cost;
            page.push((path.clone(), change.clone()));
        }
        let files = self.summarize(repository, &base, page, deadline, &mut next)?;
        Ok(Changes { repository: request.repository.clone(), files, next })
    }

    /// Fills in one page. When time runs out part-way, the page ends at the
    /// last file done and `next` moves back to it.
    fn summarize(
        &self,
        repository: BorrowedFd<'_>,
        base: &str,
        page: Vec<(String, Change)>,
        deadline: Instant,
        next: &mut Option<String>,
    ) -> Result<Vec<FileSummary>, Error> {
        let heads: Vec<&str> = page
            .iter()
            .filter_map(|(_, change)| match change {
                Change::Tracked { head: Some(head), lines: Some(_), .. } => Some(head.as_str()),
                _ => None,
            })
            .collect();
        let sizes = self.blob_sizes(repository, &heads, deadline)?;
        let mut files: Vec<FileSummary> = Vec::with_capacity(page.len());
        for (path, change) in page {
            match self.summary(repository, base, path, &change, &sizes, deadline) {
                Ok(Some(summary)) => files.push(summary),
                // Gone, or no longer a file, since `git status` listed it.
                Ok(None) => {}
                Err(e) if e.code == ErrorCode::Timeout && !files.is_empty() => {
                    *next = files.last().map(|f| f.path.clone());
                    return Ok(files);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(files)
    }

    /// Sizes of blobs, by object name, in one `git cat-file` run.
    fn blob_sizes(
        &self,
        repository: BorrowedFd<'_>,
        blobs: &[&str],
        deadline: Instant,
    ) -> Result<HashMap<String, u64>, Error> {
        if blobs.is_empty() {
            return Ok(HashMap::new());
        }
        let input: Vec<u8> = blobs.iter().flat_map(|b| format!("{b}\n").into_bytes()).collect();
        let args = os(&["cat-file", "--batch-check=%(objectsize)"]);
        let (exit, out) = self.git.output(repository, &args, Some(input), MAX_SHORT_OUTPUT, deadline)?;
        if exit.code != Some(0) {
            return Err(exit.failure("cat-file"));
        }
        let text = String::from_utf8_lossy(&out);
        // A missing object prints `<name> missing`: its size stays unknown.
        Ok(blobs
            .iter()
            .zip(text.lines())
            .filter_map(|(blob, line)| line.parse().ok().map(|size| ((*blob).to_owned(), size)))
            .collect())
    }

    fn summary(
        &self,
        repository: BorrowedFd<'_>,
        base: &str,
        path: String,
        change: &Change,
        sizes: &HashMap<String, u64>,
        deadline: Instant,
    ) -> Result<Option<FileSummary>, Error> {
        let head = match change {
            Change::Tracked { head, .. } => head.as_deref(),
            Change::Untracked => None,
        };
        let version = workspace::version(repository, &path, head);
        let mut summary = FileSummary {
            path,
            status: Status::Added,
            additions: 0,
            deletions: 0,
            untracked: false,
            binary: false,
            too_large: false,
            version,
        };
        match change {
            Change::Tracked { status, head, lines } => {
                summary.status = *status;
                let Some((additions, deletions)) = *lines else {
                    summary.binary = true;
                    return Ok(Some(summary));
                };
                // A line over the limit needs a file over it, so only then is
                // the patch read. An unknown size counts as big.
                let old = head.as_ref().map_or(Some(0), |head| sizes.get(head).copied());
                let new = if *status == Status::Deleted {
                    Some(0)
                } else {
                    workspace::size(repository, &summary.path).ok().flatten()
                };
                let small = |size: Option<u64>| size.is_some_and(|s| s <= MAX_LINE_BYTES as u64);
                if !(small(old) && small(new)) {
                    let args = tracked_patch(base, &summary.path);
                    summary.too_large = self.scan(repository, &args, deadline)?.too_large;
                }
                if !summary.too_large {
                    (summary.additions, summary.deletions) = (additions, deletions);
                }
            }
            Change::Untracked => {
                summary.untracked = true;
                let content = match workspace::open(repository, &summary.path)? {
                    Some(content @ (Content::File(..) | Content::Link(_))) => content,
                    _ => return Ok(None),
                };
                let mut scan = Scan::default();
                untracked::lines(content, deadline, &mut |event| Ok(scan.on(event)))?;
                (summary.binary, summary.too_large) = (scan.binary, scan.too_large);
                if !scan.binary && !scan.too_large {
                    summary.additions = scan.additions;
                }
            }
        }
        Ok(Some(summary))
    }

    /// Reads one file's patch for its counts, stopping at a line too long to show.
    fn scan(&self, repository: BorrowedFd<'_>, args: &[OsString], deadline: Instant) -> Result<Scan, Error> {
        let mut scan = Scan::default();
        let mut patch = Patch::default();
        let mut on = |event: Event<'_>| -> Result<Flow, Error> { Ok(scan.on(event)) };
        let exit = self.patch(repository, args, deadline, &mut patch, &mut on)?;
        if exit.is_some() {
            patch.finish(&mut on)?;
        }
        Ok(scan)
    }

    /// Streams a patch through `patch` into `on`; `None` when `on` stopped it.
    fn patch(
        &self,
        repository: BorrowedFd<'_>,
        args: &[OsString],
        deadline: Instant,
        patch: &mut Patch,
        on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>,
    ) -> Result<Option<Exit>, Error> {
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let mut sink = |chunk: &[u8]| patch.feed(chunk, on);
        let exit = self.git.stream(repository, &args, None, deadline, &mut sink)?;
        if let Some(exit) = &exit
            && exit.code != Some(0)
        {
            return Err(exit.failure("diff"));
        }
        Ok(exit)
    }

    pub fn file(&self, request: &FileRequest, deadline: Instant) -> Result<FileDiff, Error> {
        let fd = self.workspace.open_repository(&request.repository)?;
        let repository = fd.as_fd();
        if segments(&request.path, "file")?.is_empty() {
            return Err(refuse(ErrorCode::InvalidInput, "a file path is required"));
        }
        let status = self.status(repository, Some(&request.path), deadline)?;
        let listed = |tracked: bool| {
            status.records.iter().any(|record| match record {
                Record::Tracked { path, .. } => tracked && *path == request.path,
                Record::Untracked { path } => !tracked && *path == request.path,
            })
        };
        // Taken before the lines are read: if the file changes while they are,
        // the next page's version differs and the caller starts again.
        let head = status.records.iter().find_map(|record| match record {
            Record::Tracked { path, head, .. } if *path == request.path => head.as_deref(),
            _ => None,
        });
        let version = workspace::version(repository, &request.path, head);
        let mut page = Page::new(request.after_line.unwrap_or(0));
        if listed(true) {
            // Git's to read, also below a directory that became a link: git
            // never reads a tracked file through one (it shows it deleted).
            let base = self.base(repository, status.head.as_deref(), deadline)?;
            let mut parser = Patch::default();
            let mut on = |event: Event<'_>| page.on(event);
            if self
                .patch(repository, &tracked_patch(&base, &request.path), deadline, &mut parser, &mut on)?
                .is_some()
            {
                parser.finish(&mut on)?;
            }
        } else {
            // Read here, beneath the repository and through no link.
            check_parents(repository, &request.path)?;
            // Not listed: not changed (any more), nothing to show; nor when it
            // is gone since.
            let content = if listed(false) { workspace::open(repository, &request.path)? } else { None };
            if let Some(content) = content {
                untracked::lines(content, deadline, &mut |event| page.on(event))?;
            }
        }
        Ok(FileDiff { path: request.path.clone(), version, hunks: page.hunks, next_line: page.next_line })
    }
}

fn tracked_patch(base: &str, path: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["diff".into()];
    args.extend(PATCH.iter().map(OsString::from));
    args.extend(["--no-renames", "--ignore-submodules=dirty", base, "--", path].map(OsString::from));
    args
}

/// Provides the endpoint and serves requests until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let service = DiffService::open(&options.root, options.home.as_deref(), &options.repository)?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    // As a project's host, its router forwards nothing before this.
    kernel.announce_ready().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    serve_dispatches(&service, &mut kernel)
}

/// Serves the kernel's dispatches one at a time until told to terminate.
fn serve_dispatches<R: Read, W: Write>(
    service: &DiffService,
    kernel: &mut Connection<R, W>,
) -> Result<(), String> {
    // Dispatches read but not yet served, each with its deadline. The kernel
    // sends a dispatch as soon as it is admitted, with the time left then. One
    // read without waiting may have arrived at any time since the kernel was
    // last found to have nothing more (`quiet_since`, before the request just
    // served began), so its deadline counts from then. A dispatch whose caller
    // gave up is cancelled here before it is served.
    let mut queue: VecDeque<(Dispatch, Instant)> = VecDeque::new();
    let mut quiet_since = Instant::now();
    loop {
        let (event, heard) = match kernel.next_event_within(Duration::ZERO).map_err(|e| e.to_string())? {
            Some(event) => (Some(event), quiet_since),
            None => {
                quiet_since = Instant::now();
                if queue.is_empty() {
                    let event = kernel.next_event().map_err(|e| e.to_string())?;
                    // It woke the service: it came now, and whatever is read
                    // right after it came no earlier.
                    quiet_since = Instant::now();
                    (Some(event), quiet_since)
                } else {
                    (None, quiet_since)
                }
            }
        };
        if let Some(event) = event {
            match event {
                KernelEvent::Dispatch(dispatch) => {
                    let deadline = heard + Duration::from_millis(dispatch.remaining_ms);
                    queue.push_back((dispatch, deadline));
                }
                KernelEvent::CancelDispatch(id) => {
                    if let Some(at) = queue.iter().position(|(d, _)| d.id == id) {
                        queue.remove(at);
                        let cancelled = refuse(ErrorCode::Cancelled, "cancelled before it was served");
                        kernel.reply(id, Err(cancelled)).map_err(|e| e.to_string())?;
                    }
                }
                KernelEvent::ChildExited(_) | KernelEvent::ChildReaped { .. } => {}
                KernelEvent::Terminate => return Ok(()),
            }
            continue;
        }
        let Some((dispatch, deadline)) = queue.pop_front() else { continue };
        let result = match deadline.checked_sub(REPLY_MARGIN) {
            Some(ready_by) if ready_by >= Instant::now() + MIN_BUDGET => {
                service.serve(dispatch.action.as_str(), &dispatch.resource, &dispatch.payload, ready_by)
            }
            _ => Err(refuse(ErrorCode::Timeout, "too little of the caller's deadline is left to run git")),
        };
        kernel.reply(dispatch.id, result).map_err(|e| e.to_string())?;
    }
}
