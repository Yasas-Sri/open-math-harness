#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::Engine;
use serde_json::{Value, json};

use super::*;

struct Fixture {
    dir: tempfile::TempDir,
    host: Host,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["home", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let host = Host::open(&Self::options(&dir, "home", &[])).unwrap();
        Self { dir, host }
    }

    fn options(dir: &tempfile::TempDir, folder: &str, more: &[&str]) -> Options {
        let (root, tmp) = (dir.path().join(folder), dir.path().join("tmp"));
        let args =
            ["--root", root.to_str().unwrap(), "--endpoint", "dynamic.1", "--tmp", tmp.to_str().unwrap()];
        Options::parse(args.iter().chain(more).map(|a| (*a).to_owned())).unwrap()
    }

    fn home(&self) -> std::path::PathBuf {
        self.dir.path().join("home")
    }

    fn call(&self, action: &str, resource: &[&str], payload: Value) -> Result<Value, Error> {
        let resource = ResourcePath::new(resource.iter().copied()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        self.host
            .handle(action, &resource, &Payload::encode(&payload).unwrap(), deadline, &AtomicBool::new(false))
            .and_then(|p| p.decode())
    }
}

fn upload(path: &str, offset: u64, data: &[u8], last: bool) -> Value {
    json!({"path": path, "offset": offset, "data_base64": base64::engine::general_purpose::STANDARD.encode(data), "last": last})
}

/// A project's folder is made by its host, never over anything of the name,
/// and deleted by it with everything in it: a link in it goes, never what
/// it leads to, and a link on the way to the folder is not followed.
#[test]
fn a_project_s_folder_is_made_new_and_deleted_whole_without_following_links() {
    let f = Fixture::new();
    let outside = f.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("kept"), "kept").unwrap();
    std::fs::create_dir(f.dir.path().join("projects")).unwrap();
    let made = Host::open(&Fixture::options(&f.dir, "projects/alpha", &["--create", "yes"])).unwrap();
    assert!(f.dir.path().join("projects/alpha").is_dir());
    assert!(
        Host::open(&Fixture::options(&f.dir, "projects/alpha", &["--create", "yes"])).is_err(),
        "made twice"
    );
    std::os::unix::fs::symlink(&outside, f.dir.path().join("projects/alpha/link")).unwrap();
    std::fs::create_dir(f.dir.path().join("projects/alpha/deep")).unwrap();
    std::os::unix::fs::symlink(&outside, f.dir.path().join("projects/alpha/deep/link")).unwrap();
    let call = |host: &Host, action: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        let payload = Payload::encode(&json!({})).unwrap();
        host.handle(action, &ResourcePath::root(), &payload, deadline, &AtomicBool::new(false))
    };
    call(&made, "delete").unwrap();
    assert!(!f.dir.path().join("projects/alpha").exists(), "the folder is gone");
    assert_eq!(std::fs::read_to_string(outside.join("kept")).unwrap(), "kept", "what a link led to stays");
    call(&made, "delete").unwrap();
    // The folder's own path is resolved with no link followed.
    std::os::unix::fs::symlink(&outside, f.dir.path().join("linked")).unwrap();
    assert!(Host::open(&Fixture::options(&f.dir, "linked/beta", &["--create", "yes"])).is_err());
    assert!(!outside.join("beta").exists());
}

/// An upload is invisible at its destination until its last chunk, then
/// appears whole; chunks must follow on, nothing is replaced unasked, a
/// directory never, and nothing lands outside the working directory.
#[test]
fn an_upload_appears_whole_with_its_last_chunk_and_not_before() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    assert!(f.call("init", &[], json!({})).is_err(), "one repository per project");
    let repo = f.home().join("repo");
    assert_eq!(f.call("upload", &["repo"], upload("data.bin", 0, &[0, 1, 2], false)).unwrap()["size"], 3);
    assert!(!repo.join("data.bin").exists(), "a partial upload is invisible");
    let listed = f.call("list", &["repo"], json!({"path": ""})).unwrap();
    assert!(!listed.to_string().contains("data.bin"), "{listed}");
    let wrong = f.call("upload", &["repo"], upload("data.bin", 7, b"x", true));
    assert_eq!(wrong.unwrap_err().code, ErrorCode::InvalidInput, "a chunk continues where the last ended");
    assert_eq!(f.call("upload", &["repo"], upload("data.bin", 3, &[255, 254], true)).unwrap()["size"], 5);
    assert_eq!(std::fs::read(repo.join("data.bin")).unwrap(), [0, 1, 2, 255, 254]);
    let again = f.call("upload", &["repo"], upload("data.bin", 0, b"new", true));
    assert_eq!(again.unwrap_err().code, ErrorCode::InvalidInput, "not replaced unasked");
    let mut replace = upload("data.bin", 0, b"new", true);
    replace["replace"] = json!(true);
    f.call("upload", &["repo"], replace).unwrap();
    assert_eq!(std::fs::read(repo.join("data.bin")).unwrap(), b"new");
    std::fs::create_dir(repo.join("folder")).unwrap();
    let mut over_dir = upload("folder", 0, b"x", true);
    over_dir["replace"] = json!(true);
    assert!(f.call("upload", &["repo"], over_dir).is_err(), "a directory is never replaced");
    let chunk = vec![7u8; nucleus_projects_interface::CHUNK_BYTES + 1];
    assert!(f.call("upload", &["repo"], upload("big", 0, &chunk, true)).is_err(), "chunks are bounded");
    let names: Vec<_> = std::fs::read_dir(f.home().join(".nucleus/uploads")).unwrap().collect();
    assert!(names.is_empty(), "nothing is left staged: {names:?}");
    let chunk = f.call("download", &["repo"], json!({"path": "data.bin", "offset": 1})).unwrap();
    assert_eq!(chunk["data_base64"], base64::engine::general_purpose::STANDARD.encode(b"ew"));
    assert_eq!((chunk["size"].as_u64(), chunk["next_offset"].as_u64()), (Some(3), None));
}

#[test]
fn editor_saves_refuse_changed_files_and_keep_concurrent_uploads_separate() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let path = f.home().join("repo/proof.lean");
    let read = || f.call("download", &["repo"], json!({"path": "proof.lean", "offset": 0})).unwrap();
    let save = |version: &Value, id: &str, offset, data: &[u8], last| {
        let mut request = upload("proof.lean", offset, data, last);
        request["replace"] = json!(true);
        request["expected_revision"] = version.clone();
        request["upload_id"] = json!(id);
        f.call("upload", &["repo"], request)
    };
    for change in ["same-size", "replaced", "deleted", "symlink"] {
        std::fs::write(&path, "theorem old").unwrap();
        let version = read()["revision"].clone();
        save(&version, "first", 0, b"new ", false).unwrap();
        match change {
            "same-size" => std::fs::write(&path, "theorem new").unwrap(),
            "replaced" => {
                let other = path.with_extension("tmp");
                std::fs::write(&other, "replacement").unwrap();
                std::fs::rename(other, &path).unwrap();
            }
            "deleted" => std::fs::remove_file(&path).unwrap(),
            _ => {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(f.dir.path().join("outside"), &path).unwrap();
            }
        }
        let result = save(&version, "first", 4, b"proof", true).unwrap();
        assert_eq!(result["conflict"], true, "{change}");
        assert_ne!(std::fs::read(&path).ok().as_deref(), Some(b"new proof".as_slice()));
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path).unwrap();
        }
    }
    std::fs::write(&path, "original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let version = read()["revision"].clone();
    save(&version, "first", 0, b"first ", false).unwrap();
    save(&version, "second", 0, b"other ", false).unwrap();
    let saved = save(&version, "first", 6, "∀ n : ℕ, n = n\n".as_bytes(), true).unwrap();
    assert_eq!(saved["revision"], read()["revision"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first ∀ n : ℕ, n = n\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(save(&version, "second", 6, b"end", true).unwrap()["conflict"], true);
    assert_eq!(save(&saved["revision"], "empty", 0, b"", true).unwrap()["size"], 0);
    assert_eq!(std::fs::read_dir(f.home().join(".nucleus/uploads")).unwrap().count(), 0);
}

/// No file operation leaves the working directory: not through `..`, not
/// through a link planted anywhere on the path, and the repository's own
/// `.git` is neither renamed nor removed; a directory beside the repository
/// is no working directory.
#[test]
fn file_operations_cannot_leave_the_working_directory() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let repo = f.home().join("repo");
    std::fs::write(f.dir.path().join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(f.dir.path(), repo.join("up")).unwrap();
    std::os::unix::fs::symlink(f.dir.path().join("outside.txt"), repo.join("link")).unwrap();
    for (action, payload) in [
        ("list", json!({"path": "up"})),
        ("download", json!({"path": "up/outside.txt", "offset": 0})),
        ("download", json!({"path": "link", "offset": 0})),
        ("download", json!({"path": "../outside.txt", "offset": 0})),
        ("upload", upload("up/planted.txt", 0, b"x", true)),
        ("mkdir", json!({"path": "up/made"})),
        ("rename", json!({"from": "up/outside.txt", "to": "taken.txt"})),
        ("remove", json!({"path": "up/outside.txt"})),
        ("rename", json!({"from": ".git", "to": "elsewhere"})),
        ("remove", json!({"path": ".git"})),
        ("remove", json!({"path": ""})),
    ] {
        let got = f.call(action, &["repo"], payload.clone());
        assert!(got.is_err(), "{action} {payload} escaped: {got:?}");
    }
    assert_eq!(std::fs::read_to_string(f.dir.path().join("outside.txt")).unwrap(), "outside");
    assert!(!f.dir.path().join("planted.txt").exists() && !f.dir.path().join("made").exists());
    assert!(repo.join(".git").is_dir());
    // The link itself goes, never what it points at.
    f.call("remove", &["repo"], json!({"path": "link"})).unwrap();
    assert!(f.dir.path().join("outside.txt").exists());
    for resource in [vec!["repo", "up"], vec![]] {
        assert!(f.call("list", &resource, json!({"path": ""})).is_err(), "{resource:?}");
    }
}

/// A folder has no bound on its entries: one of more than a page is listed
/// whole, page by page, directories first and then by name, each entry once,
/// and a cursor that is no listing's `next` is refused.
#[test]
fn a_folder_of_any_size_is_listed_page_by_page() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let big = f.home().join("repo/big");
    std::fs::create_dir(&big).unwrap();
    for n in 0..2_100 {
        std::fs::write(big.join(format!("f{n:05}")), "").unwrap();
    }
    for n in 0..3 {
        std::fs::create_dir(big.join(format!("z{n}"))).unwrap();
    }
    let (mut names, mut after, mut pages) = (Vec::new(), None::<String>, 0);
    loop {
        let page = f.call("list", &["repo"], json!({"path": "big", "after": after})).unwrap();
        pages += 1;
        let entries = page["entries"].as_array().unwrap();
        assert!(entries.len() <= nucleus_projects_interface::PAGE_ENTRIES);
        names.extend(
            entries
                .iter()
                .map(|e| (e["kind"].as_str().unwrap().to_owned(), e["name"].as_str().unwrap().to_owned())),
        );
        match page["next"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let dirs = (0..3).map(|n| ("dir".to_owned(), format!("z{n}")));
    let files = (0..2_100).map(|n| ("file".to_owned(), format!("f{n:05}")));
    assert_eq!(names, dirs.chain(files).collect::<Vec<_>>());
    let refused = f.call("list", &["repo"], json!({"path": "big", "after": "f00001"}));
    assert_eq!(refused.unwrap_err().code, ErrorCode::InvalidInput);
}

#[test]
fn file_listing_reports_inode_creation_time_without_following_links() {
    use nucleus_projects_interface::Listing;
    use std::time::UNIX_EPOCH;

    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let repo = f.home().join("repo");
    std::fs::write(repo.join("proof.lean"), "proof").unwrap();
    std::fs::File::open(repo.join("proof.lean"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(123)))
        .unwrap();
    let outside = f.dir.path().join("outside.txt");
    std::fs::write(&outside, "outside target").unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();

    let listing: Listing =
        serde_json::from_value(f.call("list", &["repo"], json!({"path": ""})).unwrap()).unwrap();
    for name in ["proof.lean", "link"] {
        let entry = listing.entries.iter().find(|entry| entry.name == name).unwrap();
        let metadata = std::fs::symlink_metadata(repo.join(name)).unwrap();
        let expected = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| u64::try_from(time.as_millis()).unwrap());
        assert_eq!(entry.created, expected, "{name}: only the inode's reported birth time is returned");
        assert_eq!(entry.size, metadata.len(), "{name}: the link's own metadata, not its target's");
    }
    let file = listing.entries.iter().find(|entry| entry.name == "proof.lean").unwrap();
    assert_eq!(file.modified, 123_000);
    assert_eq!(
        listing.entries.iter().find(|entry| entry.name == "link").unwrap().kind,
        nucleus_projects_interface::Kind::Link
    );
}

#[test]
fn repository_urls_are_public_https_only() {
    assert!(check_url("https://github.com/leanprover-community/mathlib4.git").is_ok());
    for bad in [
        "http://github.com/a/b",
        "ssh://git@github.com/a/b",
        "git@github.com:a/b",
        "file:///etc",
        "https://user:pass@github.com/a/b",
        "https://token@github.com/a/b",
        "https:///a/b",
        "https://github.com/a/b?x=1",
        "https://github.com/a/b#x",
        "https://github.com/a b",
        "--upload-pack=x",
    ] {
        assert!(check_url(bad).is_err(), "{bad}");
    }
}

/// Runs git in `repo` as a person would, and answers what it printed.
fn person_git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new(git::GIT)
        .current_dir(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Commits travel as git bundles: one made here carries the commit without
/// what the basis has (unknown basis skipped) and is read back once; one
/// the platform writes, chunk by chunk, lands in the tracking ref alone,
/// with HEAD, the branches and the working tree as they were.
#[test]
fn bundles_carry_commits_into_a_tracking_ref_alone() {
    let (a, b) = (Fixture::new(), Fixture::new());
    a.call("init", &[], json!({})).unwrap();
    let repo = a.home().join("repo");
    std::fs::write(repo.join("one"), "1").unwrap();
    person_git(&repo, &["add", "-A"]);
    person_git(&repo, &["commit", "-qm", "one"]);
    let first = person_git(&repo, &["rev-parse", "HEAD"]);
    person_git(b.home().as_path(), &["clone", "-q", repo.to_str().unwrap(), "repo"]);
    std::fs::write(repo.join("two"), "2").unwrap();
    person_git(&repo, &["add", "-A"]);
    person_git(&repo, &["commit", "-qm", "two"]);
    let second = person_git(&repo, &["rev-parse", "HEAD"]);
    let state = a.call("git_state", &["repo"], json!({})).unwrap();
    assert_eq!(
        state,
        json!({"head": "main", "head_oid": second, "branches": [{"name": "main", "oid": second}], "tracking": []})
    );

    let out = |basis: Value| a.call("bundle_out", &["repo"], json!({"commit": second, "basis": basis}));
    let empty = out(json!([second])).unwrap_err();
    assert!(empty.code == ErrorCode::InvalidInput && empty.message.contains("nothing to send"), "{empty:?}");
    let made = out(json!([first, "f".repeat(40)])).unwrap();
    let read = json!({"bundle": made["bundle"], "offset": 0});
    let chunk = a.call("bundle_read", &["repo"], read.clone()).unwrap();
    let data =
        base64::engine::general_purpose::STANDARD.decode(chunk["data_base64"].as_str().unwrap()).unwrap();
    assert_eq!((chunk["size"].clone(), chunk["next_offset"].clone()), (made["size"].clone(), Value::Null));
    let text = String::from_utf8_lossy(&data);
    assert!(
        text.contains(&format!("-{first}")) && text.contains(&format!("{second} refs/omm/outgoing")),
        "{text}"
    );
    assert!(a.call("bundle_read", &["repo"], read).is_err(), "read once");
    assert_eq!(person_git(&repo, &["for-each-ref", "refs/omm/"]), "", "the outgoing ref is gone again");

    person_git(&repo, &["update-ref", "refs/omm/incoming", &second]);
    person_git(&repo, &["bundle", "create", "-q", "../in.bundle", "refs/omm/incoming", &format!("^{first}")]);
    let incoming = std::fs::read(a.home().join("in.bundle")).unwrap();
    let write = |bundle: &Value, offset: usize, data: &[u8], last: bool| {
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(data);
        b.call(
            "bundle_write",
            &["repo"],
            json!({"bundle": bundle, "offset": offset, "data_base64": data_base64, "last": last}),
        )
    };
    let import = |bundle: &Value, branch: &str| {
        b.call("bundle_import", &["repo"], json!({"bundle": bundle, "branch": branch, "oid": second}))
    };
    let bad = write(&Value::Null, 0, &incoming, true).unwrap()["bundle"].clone();
    assert_eq!(import(&bad, "../x").unwrap_err().code, ErrorCode::InvalidInput);
    assert!(import(&bad, "x").is_err(), "an import consumes its bundle");
    let (head, tail) = incoming.split_at(incoming.len() / 2);
    let id = write(&Value::Null, 0, head, false).unwrap()["bundle"].clone();
    assert_eq!(write(&id, 1, tail, true).unwrap_err().code, ErrorCode::InvalidInput, "chunks follow on");
    assert_eq!(write(&id, head.len(), tail, true).unwrap()["size"], incoming.len());
    std::fs::write(b.home().join("repo/local"), "mine").unwrap();
    assert_eq!(import(&id, "feature/x").unwrap(), json!({"oid": second}));
    let theirs = b.home().join("repo");
    assert_eq!(
        person_git(&theirs, &["rev-parse", "HEAD", "refs/remotes/omm/feature/x"]),
        format!("{first}\n{second}")
    );
    assert_eq!(person_git(&theirs, &["status", "--porcelain"]), "?? local");
    // Review imports live in their own namespace, even when a branch has the same name.
    let review = write(&Value::Null, 0, &incoming, true).unwrap()["bundle"].clone();
    b.call(
        "bundle_import",
        &["repo"],
        json!({"bundle":review,"branch":"feature/x","oid":second,"review":true}),
    )
    .unwrap();
    assert_eq!(person_git(&theirs, &["rev-parse", "refs/omm/reviews/feature/x"]), second);
    assert_eq!(person_git(&theirs, &["rev-parse", "HEAD"]), first);
    assert_eq!(person_git(&theirs, &["status", "--porcelain"]), "?? local");
    let state = b.call("git_state", &["repo"], json!({})).unwrap();
    assert_eq!(
        (state["branches"].clone(), state["tracking"].clone()),
        (json!([{"name": "main", "oid": first}]), json!([{"name": "feature/x", "oid": second}]))
    );
    for f in [&a, &b] {
        assert_eq!(
            std::fs::read_dir(f.home().join(".nucleus/bundles")).unwrap().count(),
            0,
            "nothing stays staged"
        );
    }
}

/// A workspace may already hold tracking refs. Configuring the remote repairs
/// upstream semantics without touching local work, and refuses conflicting config.
#[test]
fn tracking_remote_repairs_upstream_without_changing_local_work() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let repo = f.home().join("repo");
    person_git(&repo, &["commit", "--allow-empty", "-qm", "first"]);
    let first = person_git(&repo, &["rev-parse", "HEAD"]);
    person_git(&repo, &["update-ref", "refs/remotes/omm/main", &first]);
    person_git(&repo, &["commit", "--allow-empty", "-qm", "second"]);
    let head = person_git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("staged"), "keep staged").unwrap();
    person_git(&repo, &["add", "staged"]);
    std::fs::write(repo.join("staged"), "keep unstaged too").unwrap();
    let before = person_git(&repo, &["status", "--porcelain"]);
    let config = || f.call("git_remote", &["repo"], json!({"url":"https://github.com/Lab/paper.git"}));
    config().unwrap();
    config().unwrap();
    person_git(&repo, &["branch", "--set-upstream-to=omm/main", "main"]);
    assert_eq!(person_git(&repo, &["config", "branch.main.remote"]), "omm");
    assert_eq!(person_git(&repo, &["config", "branch.main.merge"]), "refs/heads/main");
    assert_eq!(person_git(&repo, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"]), "1\t0");
    assert!(person_git(&repo, &["status", "--short", "--branch"]).contains("[ahead 1]"));
    assert_eq!(person_git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(person_git(&repo, &["status", "--porcelain"]), before);
    assert_eq!(person_git(&repo, &["show", ":staged"]), "keep staged");
    assert_eq!(std::fs::read_to_string(repo.join("staged")).unwrap(), "keep unstaged too");
    person_git(&repo, &["config", "--unset-all", "remote.omm.fetch"]);
    config().unwrap(); // interrupted initial configuration is repairable
    person_git(
        &repo,
        &["config", "--add", "remote.omm.fetch", "+refs/heads/custom:refs/remotes/other/custom"],
    );
    let before_config = std::fs::read(repo.join(".git/config")).unwrap();
    assert_eq!(config().unwrap_err().code, ErrorCode::InvalidInput);
    assert_eq!(std::fs::read(repo.join(".git/config")).unwrap(), before_config);
    person_git(&repo, &["config", "--replace-all", "remote.omm.fetch", "+refs/heads/*:refs/remotes/omm/*"]);
    person_git(&repo, &["config", "--add", "remote.omm.url", "https://github.com/Lab/other.git"]);
    assert_eq!(config().unwrap_err().code, ErrorCode::InvalidInput);
    person_git(&repo, &["config", "--replace-all", "remote.omm.url", "https://example.org/Lab/other.git"]);
    assert_eq!(config().unwrap_err().code, ErrorCode::InvalidInput);
    assert_eq!(person_git(&repo, &["config", "remote.omm.url"]), "https://example.org/Lab/other.git");
    // Another connected repository, or a rename, replaces the URL the platform wrote.
    person_git(&repo, &["config", "--replace-all", "remote.omm.url", "https://github.com/Lab/other.git"]);
    config().unwrap();
    assert_eq!(person_git(&repo, &["config", "remote.omm.url"]), "https://github.com/Lab/paper.git");
    assert!(
        f.call("git_remote", &["repo"], json!({"url":"https://token@github.com/Lab/paper.git"})).is_err()
    );
}

/// Repositories with many refs and large incremental bases remain usable.
#[test]
fn github_metadata_and_bundles_have_no_count_quota() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let repo = f.home().join("repo");
    person_git(&repo, &["commit", "--allow-empty", "-qm", "first"]);
    for _ in 0..104 {
        person_git(&repo, &["commit", "--allow-empty", "-qm", "basis"]);
    }
    let first = person_git(&repo, &["rev-parse", "HEAD"]);
    let ancestors = person_git(&repo, &["rev-list", "HEAD"]);
    let long = format!("{}€∑", "segment/".repeat(35));
    let names: Vec<_> = (0..205).map(|n| format!("branch-{n:03}")).chain([long, "@".into()]).collect();
    for name in &names {
        person_git(&repo, &["branch", name]);
    }
    let state = f.call("git_state", &["repo"], json!({})).unwrap();
    assert_eq!(state["branches"].as_array().unwrap().len(), names.len() + 1);
    for name in &names {
        assert!(state["branches"].as_array().unwrap().iter().any(|r| r["name"] == *name));
    }
    person_git(&repo, &["commit", "--allow-empty", "-qm", "second"]);
    let second = person_git(&repo, &["rev-parse", "HEAD"]);
    let mut basis: Vec<String> = ancestors.lines().map(str::to_owned).collect();
    basis.push("f".repeat(40));
    let mut ids = Vec::new();
    for _ in 0..6 {
        ids.push(
            f.call("bundle_out", &["repo"], json!({"commit": second, "basis": basis})).unwrap()["bundle"]
                .clone(),
        );
    }
    for id in ids {
        let chunk = f.call("bundle_read", &["repo"], json!({"bundle": id, "offset": 0})).unwrap();
        let data =
            base64::engine::general_purpose::STANDARD.decode(chunk["data_base64"].as_str().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&data).contains(&format!("-{first}")));
    }
    assert_eq!(std::fs::read_dir(f.home().join(".nucleus/bundles")).unwrap().count(), 0);
}

#[test]
fn initialization_populates_only_an_untouched_repository() {
    let f = Fixture::new();
    f.call("init", &[], json!({})).unwrap();
    let repo = f.home().join("repo");
    std::fs::write(repo.join("README.md"), "private code").unwrap();
    person_git(&repo, &["add", "README.md"]);
    person_git(&repo, &["commit", "-qm", "source"]);
    let oid = person_git(&repo, &["rev-parse", "HEAD"]);
    person_git(&repo, &["update-ref", "refs/remotes/omm/main", &oid]);
    let initialize = || f.call("git_initialize", &["repo"], json!({"branch": "main", "oid": oid}));
    assert!(initialize().is_err(), "existing commits cannot be replaced");
    person_git(&repo, &["update-ref", "-d", "refs/heads/main"]);
    person_git(&repo, &["rm", "-q", "--cached", "README.md"]);
    assert!(initialize().is_err(), "untracked files cannot be replaced");
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "private code");
    f.call("git_remote", &["repo"], json!({"url": "https://github.com/Lab/paper.git"})).unwrap();
    std::fs::create_dir_all(repo.join(".git/info")).unwrap();
    std::fs::write(repo.join(".git/info/exclude"), "README.md\n").unwrap();
    std::fs::write(repo.join("README.md"), "local ignored work").unwrap();
    assert!(initialize().is_err(), "ignored files cannot be overwritten either");
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "local ignored work");
    std::fs::remove_file(repo.join("README.md")).unwrap();
    initialize().unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "private code");
    assert_eq!(person_git(&repo, &["rev-parse", "HEAD"]), oid);
    assert_eq!(person_git(&repo, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "omm/main");
    assert!(initialize().is_err(), "initialization is never a reset");
}
