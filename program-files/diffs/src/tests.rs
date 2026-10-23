#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use nucleus_diffs_interface::{
    ChangesRequest, FileDiff, FileRequest, Hunk, MAX_LINE_BYTES, PAGE_BYTES, PAGE_FILES,
};

use super::*;
use crate::git::GIT;

/// A workspace (`work/`) with the person's home beside it, and the service
/// reading both, showing the workspace itself as the repository.
struct Fixture {
    dir: tempfile::TempDir,
    service: DiffService,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let service =
            DiffService::open(&dir.path().join("work"), Some(&dir.path().join("home")), "").unwrap();
        Self { dir, service }
    }

    /// The service on the same workspace and home, showing `repository`.
    fn showing(&self, repository: &str) -> DiffService {
        DiffService::open(&self.work(""), Some(&self.dir.path().join("home")), repository).unwrap()
    }

    fn work(&self, relative: &str) -> PathBuf {
        self.dir.path().join("work").join(relative)
    }

    fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        let path = self.work(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// The host's git, as a person would run it, in `dir`.
    fn git_in(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        std::fs::create_dir_all(dir).unwrap();
        Command::new(GIT)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn git(&self, repository: &str, args: &[&str]) -> String {
        let out = self.git_in(&self.work(repository), args);
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
    }

    fn commit_all_after_init(&self, repository: &str) {
        self.git(repository, &["init", "-q"]);
        self.git(repository, &["add", "-A"]);
        self.git(repository, &["commit", "-q", "-m", "c"]);
    }

    /// Makes `path` stat-dirty, so git reads it again.
    fn touch(&self, path: &str) {
        let file = std::fs::File::options().write(true).open(self.work(path)).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();
    }

    fn changes(&self, repository: &str, after: Option<&str>) -> Result<Changes, Error> {
        let request = ChangesRequest { repository: repository.into(), after: after.map(str::to_owned) };
        self.showing(repository).changes(&request, deadline())
    }

    /// Every page of `changes`, checking each stays within its bounds. The
    /// versions are left out (`every_changed_file_has_a_version_...` tests them).
    fn all_changes(&self, repository: &str) -> (Vec<FileSummary>, usize) {
        let (mut files, mut pages, mut after) = (Vec::new(), 0, None);
        loop {
            let page = self.changes(repository, after.as_deref()).unwrap();
            pages += 1;
            assert!(page.files.len() <= PAGE_FILES);
            let size = serde_json::to_vec(&page.files).unwrap().len();
            assert!(size <= PAGE_BYTES, "a page of {size} bytes");
            files.extend(page.files.into_iter().map(|file| FileSummary { version: String::new(), ..file }));
            match page.next {
                Some(next) => after = Some(next),
                None => return (files, pages),
            }
        }
    }

    fn file(&self, repository: &str, path: &str, after_line: Option<u64>) -> Result<FileDiff, Error> {
        let request = FileRequest { repository: repository.into(), path: path.into(), after_line };
        self.showing(repository).file(&request, deadline())
    }

    /// Every page of `file`, flattened: each deleted line with the new-file
    /// line it stood before, each added line with its own line.
    fn all_lines(&self, repository: &str, path: &str) -> (Vec<(char, u64, String)>, Vec<FileDiff>) {
        let (mut lines, mut pages, mut after) = (Vec::new(), Vec::new(), None);
        loop {
            let page = self.file(repository, path, after).unwrap();
            let size = serde_json::to_vec(&page.hunks).unwrap().len();
            assert!(size <= PAGE_BYTES + MAX_LINE_BYTES, "a page of {size} bytes");
            for hunk in &page.hunks {
                lines.extend(hunk.deleted.iter().map(|t| ('-', hunk.line, t.clone())));
                lines.extend(hunk.added.iter().zip(hunk.line..).map(|(t, n)| ('+', n, t.clone())));
            }
            after = page.next_line;
            pages.push(page);
            if after.is_none() {
                return (lines, pages);
            }
        }
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

fn summary(path: &str, status: Status, additions: u64, deletions: u64) -> FileSummary {
    FileSummary {
        path: path.into(),
        status,
        additions,
        deletions,
        untracked: false,
        binary: false,
        too_large: false,
        version: String::new(),
    }
}

/// Every file under `dir`, `.git` included, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.insert(entry.path(), std::fs::read(entry.path()).unwrap());
            }
        }
    }
    files
}

/// Review shows the one repository it was given (in the VM a project's
/// `repo/`): nothing before it exists, never another repository of the
/// home, a worktree or a nested one, and not one reached through a link.
#[test]
fn repositories_lists_the_one_repository_and_nothing_else() {
    let fx = Fixture::new();
    let repo = fx.showing("repo");
    assert_eq!(repo.repositories(deadline()).unwrap().repositories, [], "none before it is made");
    fx.write("repo/one.txt", "1\n");
    fx.commit_all_after_init("repo");
    fx.git("repo/inner", &["init", "-q"]);
    fx.git("repo", &["worktree", "add", "-q", "--detach", "../worktrees/w1"]);
    fx.git("other", &["init", "-q"]);
    let got = repo.repositories(deadline()).unwrap().repositories;
    let got: Vec<_> = got.iter().map(|r| (r.path.as_str(), r.branch.as_deref())).collect();
    assert_eq!(got, [("repo", Some("main"))]);
    for other in ["worktrees/w1", "repo/inner", "other", ""] {
        let request = ChangesRequest { repository: other.into(), after: None };
        assert_eq!(repo.changes(&request, deadline()).unwrap_err().code, ErrorCode::InvalidInput, "{other}");
    }
    // A repository whose `.git` is a file pointing elsewhere is one.
    let store = fx.dir.path().join("store.git");
    fx.git("c", &["init", "-q", "--separate-git-dir", store.to_str().unwrap()]);
    assert_eq!(fx.showing("c").repositories(deadline()).unwrap().repositories.len(), 1);
    // Not through a link, as the repository or as its `.git`.
    std::os::unix::fs::symlink(fx.work("repo"), fx.work("link")).unwrap();
    std::fs::create_dir_all(fx.work("fake")).unwrap();
    std::os::unix::fs::symlink(fx.work("repo/.git"), fx.work("fake/.git")).unwrap();
    for linked in ["link", "fake"] {
        assert!(
            fx.showing(linked)
                .changes(&ChangesRequest { repository: linked.into(), after: None }, deadline())
                .is_err()
        );
    }
    for bad in ["..", "/etc", "repo/../other", "repo/.git"] {
        assert!(DiffService::open(&fx.work(""), None, bad).is_err(), "{bad}");
    }
}

#[test]
fn changes_are_what_git_diff_head_and_the_untracked_files_are() {
    let fx = Fixture::new();
    fx.write(".gitignore", "*.log\n");
    fx.write("modified.txt", "a\nb\nc\n");
    fx.write("deleted.txt", "gone\n");
    fx.write("binary.dat", b"x\0y");
    fx.write("touched.txt", "same\n");
    fx.write("typechange.txt", "t1\nt2\n");
    fx.write("linked/f.txt", "f\n");
    fx.write("conflict.txt", "c1\nc2\n");
    fx.write("sub/s.txt", "one\n");
    fx.commit_all_after_init("sub");
    let recorded = fx.git("sub", &["rev-parse", "HEAD"]);
    fx.commit_all_after_init("");
    // Changed on a branch, deleted here: `HEAD` lacks it in the conflict.
    fx.git("", &["checkout", "-q", "-b", "other"]);
    fx.write("conflict.txt", "c1\nC2\nc3\n");
    fx.git("", &["commit", "-q", "-a", "-m", "theirs"]);
    fx.git("", &["checkout", "-q", "main"]);
    fx.git("", &["rm", "-q", "conflict.txt"]);
    fx.git("", &["commit", "-q", "-m", "ours"]);
    assert!(!fx.git_in(&fx.work(""), &["merge", "-q", "other"]).status.success(), "no conflict");
    fx.write("modified.txt", "a\nB\nc\nd\n");
    std::fs::remove_file(fx.work("deleted.txt")).unwrap();
    fx.write("binary.dat", b"x\0z");
    fx.write("staged.txt", "new\n");
    fx.git("", &["add", "staged.txt"]);
    fx.write("dir/untracked.txt", "u1\nu2");
    fx.write("dir/untracked.bin", b"u\0\n");
    fx.write("ignored.log", "noise\n");
    fx.write("wide.txt", "w".repeat(MAX_LINE_BYTES + 1));
    std::fs::remove_file(fx.work("typechange.txt")).unwrap();
    std::os::unix::fs::symlink("modified.txt", fx.work("typechange.txt")).unwrap();
    // A tracked directory became a link to another: its file is deleted.
    std::fs::remove_dir_all(fx.work("linked")).unwrap();
    std::os::unix::fs::symlink("dir", fx.work("linked")).unwrap();
    // A new submodule commit is its two commit lines, whatever format the
    // repository asks for (`diff` starts a git inside the submodule).
    fx.write("sub/s.txt", "two\n");
    fx.git("sub", &["commit", "-q", "-a", "-m", "two"]);
    let checked_out = fx.git("sub", &["rev-parse", "HEAD"]);
    fx.git("", &["config", "diff.submodule", "diff"]);
    // Only its time changed: git's refreshed view does not list it.
    fx.touch("touched.txt");
    fx.git("nested", &["init", "-q"]);

    let (files, _) = fx.all_changes("");
    let untracked = |mut s: FileSummary| {
        s.untracked = true;
        s
    };
    assert_eq!(
        files,
        [
            FileSummary { binary: true, ..summary("binary.dat", Status::Modified, 0, 0) },
            summary("conflict.txt", Status::Added, 3, 0),
            summary("deleted.txt", Status::Deleted, 0, 1),
            untracked(FileSummary { binary: true, ..summary("dir/untracked.bin", Status::Added, 0, 0) }),
            untracked(summary("dir/untracked.txt", Status::Added, 2, 0)),
            untracked(summary("linked", Status::Added, 1, 0)),
            summary("linked/f.txt", Status::Deleted, 0, 1),
            summary("modified.txt", Status::Modified, 2, 1),
            summary("staged.txt", Status::Added, 1, 0),
            summary("sub", Status::Modified, 1, 1),
            summary("typechange.txt", Status::Modified, 1, 2),
            untracked(FileSummary { too_large: true, ..summary("wide.txt", Status::Added, 0, 0) }),
        ]
    );

    let hunks = |path: &str| fx.file("", path, None).unwrap().hunks;
    let hunk = |line, deleted: &[&str], added: &[&str]| Hunk {
        line,
        deleted: deleted.iter().map(|s| (*s).to_owned()).collect(),
        added: added.iter().map(|s| (*s).to_owned()).collect(),
    };
    assert_eq!(hunks("modified.txt"), [hunk(2, &["b"], &["B"]), hunk(4, &[], &["d"])]);
    assert_eq!(hunks("deleted.txt"), [hunk(1, &["gone"], &[])]);
    assert_eq!(hunks("dir/untracked.txt"), [hunk(1, &[], &["u1", "u2"])]);
    assert_eq!(hunks("dir/untracked.bin"), []);
    assert_eq!(hunks("binary.dat"), []);
    assert_eq!(hunks("touched.txt"), [], "an unchanged file has no lines");
    assert_eq!(hunks("conflict.txt"), [hunk(1, &[], &["c1", "C2", "c3"])]);
    assert_eq!(hunks("typechange.txt"), [hunk(1, &["t1", "t2"], &[]), hunk(1, &[], &["modified.txt"])]);
    assert_eq!(hunks("linked"), [hunk(1, &[], &["dir"])], "a link is its target, never followed");
    assert_eq!(hunks("linked/f.txt"), [hunk(1, &["f"], &[])]);
    let commit = |id: &str| format!("Subproject commit {id}");
    assert_eq!(hunks("sub"), [hunk(1, &[&commit(&recorded)], &[&commit(&checked_out)])]);
    let wide = fx.file("", "wide.txt", None).unwrap_err();
    assert!(wide.message.contains("too large"), "{wide:?}");
}

#[test]
fn every_changed_file_has_a_version_that_moves_with_its_content_and_head() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    fx.commit_all_after_init("");
    fx.write("a.txt", "1\n2\n");
    fx.write("new.txt", "n\n");
    let versions = || -> BTreeMap<String, String> {
        fx.changes("", None).unwrap().files.into_iter().map(|f| (f.path, f.version)).collect()
    };
    let first = versions();
    assert_eq!(versions(), first, "nothing changed, nothing moved");
    assert_eq!(fx.file("", "a.txt", None).unwrap().version, first["a.txt"], "a page says what it read");
    std::thread::sleep(Duration::from_millis(20));
    fx.write("a.txt", "1\n3\n"); // the same counts, other lines
    let second = versions();
    assert_ne!(second["a.txt"], first["a.txt"]);
    assert_eq!(second["new.txt"], first["new.txt"]);
    fx.git("", &["add", "a.txt"]);
    fx.git("", &["commit", "-q", "-m", "c2"]);
    fx.write("a.txt", "1\n3\n4\n");
    assert_ne!(versions()["a.txt"], second["a.txt"], "a new HEAD version moves it too");
}

#[test]
fn before_the_first_commit_every_file_is_added() {
    let fx = Fixture::new();
    fx.git("r", &["init", "-q"]);
    fx.write("r/staged.txt", "s\n");
    fx.git("r", &["add", "staged.txt"]);
    fx.write("r/loose.txt", "l\n");

    let (files, _) = fx.all_changes("r");
    let loose = FileSummary { untracked: true, ..summary("loose.txt", Status::Added, 1, 0) };
    assert_eq!(files, [loose, summary("staged.txt", Status::Added, 1, 0)]);
    assert_eq!(fx.file("r", "staged.txt", None).unwrap().hunks[0].added, ["s"]);
}

#[test]
fn every_page_together_is_the_whole_diff_with_exact_lines() {
    let fx = Fixture::new();
    // More files than one page holds.
    for i in 0..PAGE_FILES + 20 {
        fx.write(&format!("many/f{i:04}.txt"), "a\n");
    }
    // Names whose JSON escapes make the byte bound end pages first.
    let escaped = "\u{1}".repeat(200);
    for i in 0..400 {
        fx.write(&format!("wide/{escaped}{i:03}"), "a\n");
    }
    // One change bigger than a page, then two small ones.
    let old: Vec<String> = (1..=12_000).map(|i| format!("old {i:05} {}", "o".repeat(30))).collect();
    let new: Vec<String> = (1..=12_000).map(|i| format!("new {i:05} {}", "n".repeat(30))).collect();
    let same: Vec<String> = (1..=10).map(|i| format!("same {i}")).collect();
    let tail: Vec<String> = (1..=9).map(|i| format!("tail {i}")).collect();
    let added: Vec<String> = (1..=3).map(|i| format!("added {i}")).collect();
    let text = |parts: &[&[String]]| parts.concat().iter().map(|l| format!("{l}\n")).collect::<String>();
    fx.write("long/big.txt", text(&[&old, &same, &["gone".to_owned()], &tail]));
    for repository in ["many", "wide", "long"] {
        fx.commit_all_after_init(repository);
    }
    for i in 0..PAGE_FILES + 20 {
        fx.write(&format!("many/f{i:04}.txt"), "b\n");
    }
    for i in 0..400 {
        fx.write(&format!("wide/{escaped}{i:03}"), "b\n");
    }
    fx.write("long/big.txt", text(&[&new, &same, &tail, &added]));

    let (files, pages) = fx.all_changes("many");
    assert_eq!(pages, 2);
    let names: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let expected: Vec<String> = (0..PAGE_FILES + 20).map(|i| format!("f{i:04}.txt")).collect();
    assert_eq!(names, expected);
    assert!(files.iter().all(|f| (f.additions, f.deletions) == (1, 1)));

    let (files, pages) = fx.all_changes("wide");
    assert!(pages >= 3, "{pages} pages");
    assert_eq!(files.len(), 400);
    assert!(files.windows(2).all(|w| w[0].path < w[1].path), "in order, none twice");

    let (files, _) = fx.all_changes("long");
    assert_eq!(files, [summary("big.txt", Status::Modified, 12_003, 12_001)]);
    let (lines, pages) = fx.all_lines("long", "big.txt");
    let mut expected: Vec<(char, u64, String)> = old.iter().map(|t| ('-', 1, t.clone())).collect();
    expected.extend(new.iter().zip(1..).map(|(t, n)| ('+', n, t.clone())));
    // `gone` stood before the new file's line 12 011 (after the 12 010 kept lines).
    expected.push(('-', 12_011, "gone".to_owned()));
    expected.extend(added.iter().zip(12_020..).map(|(t, n)| ('+', n, t.clone())));
    assert_eq!(lines, expected);
    // The case at risk happened: pages that begin inside the deleted run and
    // inside the added run of one hunk.
    let starts: Vec<&Hunk> = pages.iter().skip(1).filter_map(|p| p.hunks.first()).collect();
    assert!(starts.iter().any(|h| h.line == 1 && !h.deleted.is_empty()), "{:?}", starts.len());
    assert!(starts.iter().any(|h| h.line > 1 && h.line <= 12_000 && h.deleted.is_empty()));
}

/// Neither the repository's configuration nor the person's global one (in the
/// home the service is given) makes it run the fsmonitor hook; the global
/// excludes still count, as they do for the person's own git.
#[test]
fn a_configured_fsmonitor_hook_never_runs_and_global_excludes_count() {
    let fx = Fixture::new();
    fx.write("f.txt", "a\n");
    fx.commit_all_after_init("");
    fx.write("f.txt", "b\n");
    fx.write("x.tmp", "noise\n");
    let home = fx.dir.path().join("home");
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::write(home.join(".config/git/ignore"), "*.tmp\n").unwrap();
    // A hook that ran leaves the marker or, where it cannot write one,
    // outlasts the deadline: either way the service fails.
    let marker = fx.dir.path().join("marker");
    let hook = fx.dir.path().join("hook.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}' || sleep 30\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for config in [fx.work(".git/config"), home.join(".gitconfig")] {
        let config = config.to_str().unwrap();
        fx.git("", &["config", "--file", config, "core.fsmonitor", hook.to_str().unwrap()]);
        // The person's own git runs it, so the configuration is set up to attack.
        let _ = fx.git_in(&fx.work(""), &["status", "--porcelain"]);
        assert!(marker.exists(), "the hook is live for an ordinary git");
        std::fs::remove_file(&marker).unwrap();

        let soon = Instant::now() + Duration::from_secs(10);
        fx.service.repositories(soon).unwrap();
        let request = ChangesRequest { repository: String::new(), after: None };
        let files = fx.service.changes(&request, soon).unwrap().files;
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["f.txt"]);
        let request = FileRequest { repository: String::new(), path: "f.txt".into(), after_line: None };
        fx.service.file(&request, soon).unwrap();
        assert!(!marker.exists(), "the service ran the fsmonitor hook of {config}");
        fx.git("", &["config", "--file", config, "--unset", "core.fsmonitor"]);
    }
}

/// A request that waited while another was served keeps the deadline it
/// arrived with: its time counts from before that serve, not after it.
#[test]
fn a_request_that_waited_behind_another_keeps_its_deadline() {
    use nucleus_protocol::{
        ActionName, DispatchId, FromKernel, ProcessId, ToKernel, read_frame, write_frame,
    };
    let fx = Fixture::new();
    fx.write("f.txt", "a\n");
    fx.commit_all_after_init("");
    // Each git run that reads this file says it began and takes 300 ms.
    let began = fx.dir.path().join("began");
    fx.write(".gitattributes", "f.txt filter=slow\n");
    fx.git("", &["config", "filter.slow.clean", &format!("touch '{}'; sleep 0.3; cat", began.display())]);
    fx.touch("f.txt");
    let (from_kernel, mut kernel_out) = std::io::pipe().unwrap();
    let (mut kernel_in, to_kernel) = std::io::pipe().unwrap();
    let dispatch = |id, action, payload, remaining_ms| FromKernel::Dispatch {
        dispatch: DispatchId(id),
        endpoint: nucleus_protocol::EndpointName::new(ENDPOINT).unwrap(),
        caller: ProcessId(2),
        action: ActionName::new(action).unwrap(),
        resource: ResourcePath::root(),
        payload: Payload::encode(&payload).unwrap(),
        remaining_ms,
    };
    write_frame(&mut kernel_out, &FromKernel::Hello { process: ProcessId(1), handles: Vec::new() }).unwrap();
    let replies = std::thread::scope(|scope| {
        let served = scope.spawn(|| {
            let mut kernel = Connection::open(from_kernel, to_kernel).unwrap();
            serve_dispatches(&fx.service, &mut kernel)
        });
        let changes = serde_json::json!({"repository": ""});
        write_frame(&mut kernel_out, &dispatch(1, ACTION_CHANGES, changes, 60_000)).unwrap();
        // Sent while the first is served, with 200 ms more than the reply
        // margin and git's least budget: gone by the time that serve ends.
        let until = Instant::now() + Duration::from_secs(10);
        while !began.exists() {
            assert!(Instant::now() < until, "the first request never ran git");
            std::thread::sleep(Duration::from_millis(5));
        }
        let quick = dispatch(2, ACTION_REPOSITORIES, serde_json::json!({}), 1_700);
        write_frame(&mut kernel_out, &quick).unwrap();
        let mut replies = BTreeMap::new();
        for _ in 0..2 {
            match read_frame(&mut kernel_in) {
                Ok(ToKernel::Reply { dispatch, result }) => replies.insert(dispatch.0, result),
                other => panic!("{other:?}"),
            };
        }
        write_frame(&mut kernel_out, &FromKernel::Terminate).unwrap();
        served.join().unwrap().unwrap();
        replies
    });
    assert!(replies[&1].is_ok(), "{:?}", replies[&1]);
    let late = replies[&2].as_ref().expect_err("served past its caller's deadline");
    assert_eq!(late.code, ErrorCode::Timeout, "{late:?}");
}

#[test]
fn reading_leaves_the_repository_byte_identical() {
    let fx = Fixture::new();
    fx.write("a.txt", "a\n");
    fx.write("b.txt", "b\n");
    fx.commit_all_after_init("");
    fx.write("a.txt", "A\n");
    fx.write("c.txt", "c\n");
    // Stale stat information: an ordinary `git diff` rewrites the index.
    fx.touch("b.txt");
    let before = snapshot(&fx.work(""));

    fx.service.repositories(deadline()).unwrap();
    fx.all_changes("");
    for path in ["a.txt", "b.txt", "c.txt"] {
        fx.file("", path, None).unwrap();
    }
    assert_eq!(snapshot(&fx.work("")), before, "the service changed the repository");

    // What would have changed: the index, under the person's own git.
    fx.git("", &["diff", "HEAD", "--numstat"]);
    let index = fx.work(".git/index");
    assert_ne!(snapshot(&fx.work(""))[&index], before[&index], "the setup never made git write");
}

#[test]
fn a_request_cannot_leave_the_workspace_or_its_repositories() {
    let fx = Fixture::new();
    fx.write("r/sub/f.txt", "a\n");
    fx.write("r/node_modules/m.txt", "m\n");
    fx.commit_all_after_init("r");
    fx.write("outside/secret.txt", "s\n");
    fx.commit_all_after_init("outside");
    std::os::unix::fs::symlink(fx.work("outside"), fx.work("r/link")).unwrap();
    std::os::unix::fs::symlink(fx.work("r"), fx.work("r-link")).unwrap();
    fx.git("r/node_modules/pkg", &["init", "-q"]);

    let r = fx.showing("r");
    let refused = |repository: &str, path: &str| {
        let changes = r.changes(&ChangesRequest { repository: repository.into(), after: None }, deadline());
        let request = FileRequest { repository: repository.into(), path: path.into(), after_line: None };
        let file = r.file(&request, deadline()).map(|_| ());
        let changes = changes.map(|_| ());
        for got in [changes, file] {
            let code = got.expect_err(&format!("{repository:?} {path:?} was served")).code;
            assert!(matches!(code, ErrorCode::InvalidInput | ErrorCode::PermissionDenied), "{code:?}");
        }
    };
    for repository in
        ["..", "/", "/etc", "r/..", "r/../outside/..", "r/./sub", "r//sub", "r/.git", "r-link", "r/sub"]
    {
        refused(repository, "f.txt");
    }
    refused("r/node_modules/pkg", "x");
    for path in [
        "../outside/secret.txt",
        "/etc/passwd",
        "sub/../../outside/secret.txt",
        ".git/config",
        "link/secret.txt",
    ] {
        let code = fx.file("r", path, None).expect_err(path).code;
        assert!(matches!(code, ErrorCode::InvalidInput | ErrorCode::PermissionDenied), "{path}: {code:?}");
    }
    let whole = Payload::encode(&serde_json::json!({"repository": "r"})).unwrap();
    let scoped = ResourcePath::new(["r"]).unwrap();
    assert!(r.serve(ACTION_CHANGES, &scoped, &whole, deadline()).is_err());
    assert!(r.serve(ACTION_CHANGES, &ResourcePath::root(), &whole, deadline()).is_ok());
}

#[test]
fn a_name_that_is_not_utf8_fails_clearly_instead_of_vanishing() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new();
    fx.git("r", &["init", "-q"]);
    std::fs::write(fx.work("r").join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")), "x\n").unwrap();
    let error = fx.changes("r", None).unwrap_err();
    assert!(error.message.contains("not UTF-8"), "{error:?}");
}

#[test]
fn json_sizes_match_what_serde_json_writes() {
    for text in ["", "plain", "quote\" back\\slash", "\n\r\t\u{8}\u{c}", "\u{1}\u{1f}\u{7f}", "é€🙂"] {
        assert_eq!(diff::json_len(text), serde_json::to_string(text).unwrap().len(), "{text:?}");
    }
}
