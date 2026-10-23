//! Git bundles between the repository and the platform, which keeps the
//! GitHub credentials (`git_state` and the `bundle_*` files actions). A
//! bundle is staged in the folder's `.nucleus/bundles`, outside the working
//! tree, named by its id: one `bundle_out` made, until its last chunk is
//! read, or one the platform writes, until `bundle_import` consumes it.
//! Nothing the bundles do touches HEAD, the index, the working tree or a local
//! branch: `bundle_out` points [`OUTGOING_REF`] at the commit only while git
//! bundles it, and `bundle_import` updates one `refs/remotes/omm/<branch>`, or
//! with `review` one `refs/omm/reviews/<branch>`. `git_remote` sets
//! the repository's `omm` remote ([`remote`]), and `git_initialize`
//! ([`initialize`]) checks out a fetched branch in a repository that has no
//! local commits or changes yet, as its tracking branch.
//!
//! Git runs in the repository and names a staged bundle by its path from
//! there (`../.nucleus/bundles/<id>`); this host itself reads and writes it
//! only through the staging directory it opened, with no link followed.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use nucleus_projects_interface::{
    BUNDLE_CHUNK_BYTES, BUNDLE_IDLE_MS, Bundle, BundleChunk, BundleImport, BundleOut, BundleRead,
    BundleWrite, GitRef, GitState, INCOMING_REF, Imported, OUTGOING_REF, TRACKING_PREFIX, valid_branch,
    valid_bundle_id, valid_oid,
};
use nucleus_protocol::{Error, ErrorCode};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use crate::git::{Git, Output};
use crate::home::{Home, STAGING, errno, open_beneath};

/// The staging area of bundles, in [`STAGING`].
pub const AREA: &str = "bundles";
/// The deadline of removing [`OUTGOING_REF`] again, whatever the call's.
const CLEANUP: Duration = Duration::from_secs(10);

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

fn failed(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::ServiceError, message.into())
}

fn os(args: &[String]) -> Vec<&OsStr> {
    args.iter().map(OsStr::new).collect()
}

fn args<const N: usize>(fixed: [&str; N]) -> Vec<String> {
    fixed.iter().map(|a| (*a).to_owned()).collect()
}

/// The first line of what git printed.
fn line(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_owned()
}

/// A staged bundle's path from the repository, where git runs.
fn path(id: &str) -> String {
    format!("../{STAGING}/{AREA}/{id}")
}

struct Staged {
    file: std::fs::File,
    size: u64,
    /// Made by `bundle_out`, to be read; else written by the platform.
    outgoing: bool,
    /// Every chunk is written (always, for an outgoing one).
    complete: bool,
    touched: Instant,
}

/// The bundles staged in this project.
#[derive(Default)]
pub struct Bundles {
    table: Mutex<HashMap<String, Staged>>,
    /// Held while [`OUTGOING_REF`] names a commit.
    outgoing: Mutex<()>,
}

/// Removes a staged bundle's file, and a lock git may have left beside it.
fn discard(home: &Home, id: &str) {
    if let Ok(staging) = home.staging(AREA) {
        for name in [id.to_owned(), format!("{id}.lock")] {
            let _ = rustix::fs::unlinkat(&staging, name.as_str(), AtFlags::empty());
        }
    }
}

/// A fresh id: `b-` and 32 hex digits from the kernel's random source.
fn fresh_id() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| failed(format!("no random id: {e}")))?;
    Ok(format!("b-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

fn unknown(id: &str) -> Error {
    invalid(format!("no bundle {id} is staged here"))
}

impl Bundles {
    /// The table, with the bundles nobody touched for [`BUNDLE_IDLE_MS`] gone.
    fn table(&self, home: &Home) -> MutexGuard<'_, HashMap<String, Staged>> {
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let idle = Duration::from_millis(BUNDLE_IDLE_MS);
        table.retain(|id, staged| {
            let keep = staged.touched.elapsed() < idle;
            if !keep {
                discard(home, id);
            }
            keep
        });
        table
    }

    /// Stages a bundle of `commit` carrying exactly [`OUTGOING_REF`], without
    /// the commits reachable from the basis commits the repository has.
    pub fn out(
        &self,
        home: &Home,
        git: &Git,
        working: &OwnedFd,
        request: &BundleOut,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Bundle, Error> {
        if !valid_oid(&request.commit) {
            return Err(invalid(format!("commit {:?} is not a full object id", request.commit)));
        }
        if let Some(bad) = request.basis.iter().find(|b| !valid_oid(b)) {
            return Err(invalid(format!("basis {bad:?} is not a full object id")));
        }
        let run = |args: &[String]| git.run_complete(working.as_fd(), &os(args), deadline, cancel);
        let _ref_held = self.outgoing.lock().unwrap_or_else(PoisonError::into_inner);
        let commit = &request.commit;
        let peeled = run(&args(["rev-parse", "-q", "--verify", &format!("{commit}^{{commit}}")]))?;
        if !peeled.ok() || line(&peeled) != *commit {
            return Err(invalid(format!("{commit} is no commit of the repository")));
        }
        // The basis commits the repository has; the others are skipped.
        let mut known = Vec::new();
        if !request.basis.is_empty() {
            let mut list = args(["rev-list", "--no-walk", "--ignore-missing"]);
            list.extend(request.basis.iter().map(|b| format!("{b}^{{commit}}")));
            let listed = run(&list)?;
            if !listed.ok() {
                return Err(listed.failure("rev-list"));
            }
            known = String::from_utf8_lossy(&listed.stdout)
                .lines()
                .filter(|l| valid_oid(l))
                .map(|l| format!("^{l}"))
                .collect();
        }
        let id = fresh_id()?;
        home.staging(AREA)?;
        let set = run(&args(["update-ref", "--no-deref", OUTGOING_REF, commit]))?;
        if !set.ok() {
            return Err(set.failure("update-ref"));
        }
        let mut create = args(["bundle", "create", &path(&id), OUTGOING_REF]);
        create.extend(known);
        let made = run(&create).and_then(|out| match out {
            out if out.ok() => Ok(()),
            out if out.stderr.contains("empty bundle") => {
                Err(invalid(format!("nothing to send: {commit} is reachable from the basis")))
            }
            out => Err(out.failure("bundle create")),
        });
        let until = Instant::now() + CLEANUP;
        let delete = args(["update-ref", "--no-deref", "-d", OUTGOING_REF]);
        let cleared = git.run_complete(working.as_fd(), &os(&delete), until, &AtomicBool::new(false));
        let cleared =
            cleared.and_then(|out| if out.ok() { Ok(()) } else { Err(out.failure("update-ref -d")) });
        let staged = made.and(cleared).and_then(|()| self.adopt(home, &id));
        if staged.is_err() {
            discard(home, &id);
        }
        staged
    }

    /// Registers the bundle git made as `id`, opened with no link followed.
    fn adopt(&self, home: &Home, id: &str) -> Result<Bundle, Error> {
        let staging = home.staging(AREA)?;
        let file = open_beneath(&staging, id, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())
            .map_err(|e| errno("the staged bundle", e))?;
        let stat = rustix::fs::fstat(&file).map_err(|e| errno("the staged bundle", e))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(failed("the staged bundle is not a file"));
        }
        let size = u64::try_from(stat.st_size).unwrap_or(u64::MAX);
        let mut table = self.table(home);
        let staged =
            Staged { file: file.into(), size, outgoing: true, complete: true, touched: Instant::now() };
        table.insert(id.to_owned(), staged);
        Ok(Bundle { bundle: id.to_owned(), size })
    }

    /// One chunk of a bundle `bundle_out` made; the last removes it.
    pub fn read(&self, home: &Home, request: &BundleRead) -> Result<BundleChunk, Error> {
        if !valid_bundle_id(&request.bundle) {
            return Err(unknown(&request.bundle));
        }
        let mut table = self.table(home);
        let Some(staged) = table.get_mut(&request.bundle).filter(|s| s.outgoing) else {
            return Err(unknown(&request.bundle));
        };
        if request.offset > staged.size {
            return Err(invalid(format!("bundle {} is {} bytes", request.bundle, staged.size)));
        }
        let left = staged.size - request.offset;
        let mut data = vec![0u8; usize::try_from(left).unwrap_or(usize::MAX).min(BUNDLE_CHUNK_BYTES)];
        staged.file.read_exact_at(&mut data, request.offset).map_err(|e| failed(format!("bundle: {e}")))?;
        staged.touched = Instant::now();
        let size = staged.size;
        let next = request.offset + data.len() as u64;
        let next_offset = if next < size {
            Some(next)
        } else {
            table.remove(&request.bundle);
            discard(home, &request.bundle);
            None
        };
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        Ok(BundleChunk { data_base64, size, next_offset })
    }

    /// One chunk of a bundle the platform sends.
    pub fn write(&self, home: &Home, request: &BundleWrite) -> Result<Bundle, Error> {
        // Base64 of a full chunk, padded; a longer text cannot decode to one.
        if request.data_base64.len() > BUNDLE_CHUNK_BYTES.div_ceil(3) * 4 {
            return Err(invalid(format!("a chunk is at most {BUNDLE_CHUNK_BYTES} bytes")));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(&request.data_base64)
            .map_err(|e| invalid(format!("data_base64: {e}")))?;
        if data.len() > BUNDLE_CHUNK_BYTES {
            return Err(invalid(format!("a chunk is at most {BUNDLE_CHUNK_BYTES} bytes")));
        }
        let mut table = self.table(home);
        let id = match &request.bundle {
            None if request.offset == 0 => {
                let id = fresh_id()?;
                let file = open_beneath(
                    home.staging(AREA)?,
                    &id,
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                    Mode::from_raw_mode(0o600),
                )
                .map_err(|e| errno("the staged bundle", e))?;
                let staged = Staged {
                    file: file.into(),
                    size: 0,
                    outgoing: false,
                    complete: false,
                    touched: Instant::now(),
                };
                table.insert(id.clone(), staged);
                id
            }
            None => {
                return Err(invalid("a bundle starts at offset 0 without `bundle`; later chunks name it"));
            }
            Some(id) if valid_bundle_id(id) => id.clone(),
            Some(id) => return Err(unknown(id)),
        };
        let Some(staged) = table.get_mut(&id).filter(|s| !s.outgoing) else { return Err(unknown(&id)) };
        if staged.complete {
            return Err(invalid(format!("bundle {id} is complete")));
        }
        if request.offset != staged.size {
            return Err(invalid(format!("the next chunk of bundle {id} starts at offset {}", staged.size)));
        }
        let size =
            staged.size.checked_add(data.len() as u64).ok_or_else(|| invalid("bundle offset overflow"))?;
        let written = staged.file.write_all(&data).map_err(|e| failed(format!("bundle: {e}")));
        match written {
            Ok(()) => {
                staged.size = size;
                staged.complete = request.last;
                staged.touched = Instant::now();
                Ok(Bundle { bundle: id, size })
            }
            Err(e) => {
                table.remove(&id);
                discard(home, &id);
                Err(e)
            }
        }
    }

    /// Fetches a complete bundle the platform wrote into
    /// `refs/remotes/omm/<branch>` (or, with `review`,
    /// `refs/omm/reviews/<branch>`), and checks it then names `oid`.
    /// The bundle is consumed whatever happens.
    pub fn import(
        &self,
        home: &Home,
        git: &Git,
        working: &OwnedFd,
        request: &BundleImport,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Imported, Error> {
        if !valid_bundle_id(&request.bundle) {
            return Err(unknown(&request.bundle));
        }
        let Some(staged) = self.table(home).remove(&request.bundle) else {
            return Err(unknown(&request.bundle));
        };
        let imported = (|| {
            if staged.outgoing || !staged.complete {
                return Err(invalid(format!("bundle {} is no complete bundle written here", request.bundle)));
            }
            if !valid_branch(&request.branch) {
                return Err(invalid(format!("{:?} is not a branch name", request.branch)));
            }
            if !valid_oid(&request.oid) {
                return Err(invalid(format!("oid {:?} is not a full object id", request.oid)));
            }
            let local = |args: &[String]| git.run_local(working.as_fd(), &os(args), deadline, cancel);
            let bundle = path(&request.bundle);
            let verified = local(&args(["bundle", "verify", "--quiet", &bundle]))?;
            if !verified.ok() {
                return Err(invalid(format!("the bundle does not apply here: {}", verified.stderr.trim())));
            }
            let prefix = if request.review { "refs/omm/reviews/" } else { TRACKING_PREFIX };
            let tracking = format!("{prefix}{}", request.branch);
            let refspec = format!("+{INCOMING_REF}:{tracking}");
            let fetch = args([
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--no-auto-gc",
                "--end-of-options",
                &bundle,
                &refspec,
            ]);
            let fetched = local(&fetch)?;
            if !fetched.ok() {
                return Err(fetched.failure("fetch"));
            }
            let now = git.run_complete(
                working.as_fd(),
                &os(&args(["rev-parse", "-q", "--verify", &tracking])),
                deadline,
                cancel,
            )?;
            let got = line(&now);
            if !now.ok() || got != request.oid {
                return Err(failed(format!("the bundle brought {tracking} to {got:?}, not {}", request.oid)));
            }
            Ok(Imported { oid: got })
        })();
        drop(staged);
        discard(home, &request.bundle);
        imported
    }
}

/// One URL as `remote` writes it: `https://github.com/<owner>/<repository>.git`.
fn written_url(config: &[u8]) -> bool {
    let Some(path) = std::str::from_utf8(config)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .and_then(|url| url.strip_prefix("https://github.com/"))
        .and_then(|rest| rest.strip_suffix(".git"))
    else {
        return false;
    };
    let mut parts = path.split('/');
    let name = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    name(parts.next()) && name(parts.next()) && parts.next().is_none()
}

/// Materialize a fetched branch only in a repository with no local commits or changes.
pub fn initialize(
    git: &Git,
    working: &OwnedFd,
    branch: &str,
    oid: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !valid_branch(branch) || !valid_oid(oid) {
        return Err(invalid("invalid initial branch"));
    }
    let current = state(git, working, deadline, cancel)?;
    if current.head_oid.is_some() || !current.branches.is_empty() {
        return Err(invalid("the project already has local commits"));
    }
    let run = |args: &[String]| git.run_local(working.as_fd(), &os(args), deadline, cancel);
    let clean = run(&args(["status", "--porcelain", "--untracked-files=all"]))?;
    if !clean.ok() {
        return Err(clean.failure("status"));
    }
    if !clean.stdout.is_empty() {
        return Err(invalid("the project already has local changes"));
    }
    if !current.tracking.iter().any(|r| r.name == branch && r.oid == oid) {
        return Err(invalid("the fetched branch changed"));
    }
    let tracking = format!("{TRACKING_PREFIX}{branch}");
    let checked = run(&args(["checkout", "--no-overwrite-ignore", "-b", branch, "--track", &tracking]))?;
    if !checked.ok() {
        return Err(checked.failure("checkout"));
    }
    Ok(())
}

/// Give the tracking refs their standard Git remote semantics. Missing settings
/// are repaired, and a URL this operation wrote for another repository (the project
/// connected a different one, or the repository was renamed) is replaced; a person's
/// other remote configuration is never replaced. No credential is stored and no
/// network command is run.
pub fn remote(
    git: &Git,
    working: &OwnedFd,
    url: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    crate::check_url(url)?;
    let run = |args: &[String]| git.run_local(working.as_fd(), &os(args), deadline, cancel);
    let settings = [("remote.omm.url", url), ("remote.omm.fetch", "+refs/heads/*:refs/remotes/omm/*")];
    let mut missing = Vec::new();
    // Validate both settings before changing either, including multi-valued config.
    for (key, value) in settings {
        let got = run(&args(["config", "--get-all", key]))?;
        match got.code {
            Some(1) => missing.push((key, value)),
            Some(0) if got.stdout == format!("{value}\n").as_bytes() => {}
            Some(0) if key == "remote.omm.url" && written_url(&got.stdout) => missing.push((key, value)),
            Some(0) => {
                return Err(invalid(
                    "the omm remote has different configuration; rename it before fetching the connected repository",
                ));
            }
            _ => return Err(got.failure("config")),
        }
    }
    for (key, value) in missing {
        let saved = run(&args(["config", "--local", "--replace-all", key, value]))?;
        if !saved.ok() {
            return Err(saved.failure("config"));
        }
    }
    Ok(())
}

/// HEAD, the branches and the tracking refs of the repository.
pub fn state(
    git: &Git,
    working: &OwnedFd,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<GitState, Error> {
    let run = |args: &[String]| git.run_complete(working.as_fd(), &os(args), deadline, cancel);
    let head = run(&args(["symbolic-ref", "-q", "HEAD"]))?;
    let head = match head.code {
        Some(0) => line(&head).strip_prefix("refs/heads/").map(str::to_owned),
        Some(1) => None,
        _ => return Err(head.failure("symbolic-ref")),
    };
    let oid = run(&args(["rev-parse", "-q", "--verify", "HEAD^{commit}"]))?;
    let head_oid = match oid.code {
        Some(0) => Some(line(&oid)).filter(|o| valid_oid(o)),
        Some(1) => None,
        _ => return Err(oid.failure("rev-parse")),
    };
    let refs = |prefix: &str| -> Result<Vec<GitRef>, Error> {
        let out = run(&args(["for-each-ref", "--format=%(objectname) %(refname)", prefix]))?;
        if !out.ok() {
            return Err(out.failure("for-each-ref"));
        }
        let refs: Vec<GitRef> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter_map(|(oid, name)| {
                Some(GitRef { name: name.strip_prefix(prefix)?.to_owned(), oid: oid.to_owned() })
            })
            .collect();
        Ok(refs)
    };
    let branches = refs("refs/heads/")?;
    let tracking = refs(TRACKING_PREFIX)?;
    Ok(GitState { head, head_oid, branches, tracking })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn staging_and_chunk_io_work_past_256_mib() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let bundles = Bundles::default();
        home.staging(AREA).unwrap();
        let boundary = 256 * 1024 * 1024;
        let id = fresh_id().unwrap();
        let file = std::fs::File::create(dir.path().join(STAGING).join(AREA).join(&id)).unwrap();
        file.set_len(boundary + 3).unwrap();
        file.write_all_at(b"end", boundary).unwrap();
        assert_eq!(bundles.adopt(&home, &id).unwrap().size, boundary + 3);
        let chunk = bundles.read(&home, &BundleRead { bundle: id, offset: boundary }).unwrap();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(chunk.data_base64).unwrap(), b"end");
        assert_eq!(chunk.next_offset, None);
        let started = bundles
            .write(&home, &BundleWrite { bundle: None, offset: 0, data_base64: String::new(), last: false })
            .unwrap();
        {
            let mut table = bundles.table(&home);
            let staged = table.get_mut(&started.bundle).unwrap();
            staged.file.set_len(boundary).unwrap();
            std::io::Seek::seek(&mut staged.file, std::io::SeekFrom::Start(boundary)).unwrap();
            staged.size = boundary;
        }
        let done = bundles
            .write(
                &home,
                &BundleWrite {
                    bundle: Some(started.bundle.clone()),
                    offset: boundary,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(b"end"),
                    last: true,
                },
            )
            .unwrap();
        assert_eq!(done.size, boundary + 3);
        let mut tail = [0; 3];
        std::fs::File::open(dir.path().join(STAGING).join(AREA).join(started.bundle))
            .unwrap()
            .read_exact_at(&mut tail, boundary)
            .unwrap();
        assert_eq!(&tail, b"end");
    }
}
