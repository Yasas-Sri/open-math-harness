#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use nucleus_protocol::{
    ActionName, ChildProgram, CreatedEndpoint, DispatchId, EndpointName, Error, ErrorCode, Generation, Grant,
    HandleId, HandleInfo, IdentityRequest, Payload, ProcessId, RequestId, ResourcePath,
};
use nucleus_sdk::{ChannelError, Dispatch, Reply};
use serde_json::{Value, json};

use super::*;

/// A kernel that records what the router asks and answers at once.
#[derive(Default)]
struct Recorder {
    next: u64,
    spawned: Vec<(ProcessId, ChildProgram, Vec<Grant>, Option<IdentityRequest>)>,
    submitted: Vec<(RequestId, HandleId, String, Vec<String>, Value)>,
    replies: Vec<(DispatchId, Result<Value, ErrorCode>)>,
    refusals: Vec<String>,
    rebound: Vec<HandleId>,
    killed: Vec<ProcessId>,
    stops: Vec<(RequestId, HandleId, String)>,
}

impl Recorder {
    fn fresh(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

fn handle(id: u64, endpoint: &str, action: &str) -> HandleInfo {
    HandleInfo {
        handle: HandleId(u32::try_from(id).unwrap()),
        endpoint: EndpointName::new(endpoint).unwrap(),
        actions: [ActionName::new(action).unwrap()].into(),
        resource: ResourcePath::root(),
        delegable: true,
        expires_in_ms: None,
        max_calls: None,
    }
}

impl Kernel for Recorder {
    fn reply(&mut self, dispatch: DispatchId, result: Result<Payload, Error>) -> Result<(), ChannelError> {
        if let Err(e) = &result {
            self.refusals.push(e.message.clone());
        }
        self.replies.push((dispatch, result.map(|p| p.decode().unwrap()).map_err(|e| e.code)));
        Ok(())
    }
    fn submit(
        &mut self,
        handle: HandleId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        _: Option<u64>,
    ) -> Reply<RequestId> {
        let request = RequestId(self.fresh());
        let resource = resource.segments().to_vec();
        self.submitted.push((request, handle, action.to_owned(), resource, payload.decode().unwrap()));
        Ok(Ok(request))
    }
    fn cancel(&mut self, _: RequestId) -> Result<(), ChannelError> {
        Ok(())
    }
    fn create_endpoint(&mut self, _: HandleId, _: BTreeSet<ActionName>) -> Reply<CreatedEndpoint> {
        let n = self.fresh();
        let endpoint = EndpointName::new(format!("dynamic.{n}")).unwrap();
        Ok(Ok(CreatedEndpoint {
            provide: handle(n * 10, "kernel", "provide"),
            invoke: handle(n * 10 + 1, endpoint.as_str(), "x"),
            endpoint,
        }))
    }
    fn spawn(
        &mut self,
        _: HandleId,
        program: ChildProgram,
        grants: Vec<Grant>,
        identity: Option<IdentityRequest>,
    ) -> Reply<ProcessId> {
        let process = ProcessId(100 + self.fresh());
        self.spawned.push((process, program, grants, identity));
        Ok(Ok(process))
    }
    fn kill(&mut self, process: ProcessId) -> Reply<()> {
        self.killed.push(process);
        Ok(Ok(()))
    }
    fn rebind(&mut self, handle: HandleId) -> Reply<Generation> {
        self.rebound.push(handle);
        Ok(Ok(Generation(2)))
    }
    fn close(&mut self, _: HandleId) -> Reply<()> {
        Ok(Ok(()))
    }
    fn stop_project(&mut self, authority: HandleId, project: &str) -> Reply<RequestId> {
        let request = RequestId(self.fresh());
        self.stops.push((request, authority, project.to_owned()));
        Ok(Ok(request))
    }
}

/// The router's `conversation wake` on every session.
const WAKE: HandleId = HandleId(6);
/// The router's `kernel stop_project`, in a composition with projects'
/// processes.
const STOP: HandleId = HandleId(5);

struct World {
    dir: tempfile::TempDir,
    /// The background jobs' records.
    jobs: tempfile::TempDir,
    router: Router,
    k: Recorder,
    next: u64,
}

fn options() -> HostConfig {
    HostConfig {
        programs: "/programs".into(),
        home: "/work/home".into(),
        shell_description: "a shell".into(),
        prover_shell_args: vec!["--env".into(), "FUNCNEST=32".into()],
        ..HostConfig::default()
    }
}

#[test]
fn an_archive_preserves_data_and_reserves_the_name_until_unarchived() {
    let mut w = World::new(Some(STOP));
    std::fs::create_dir(w.dir.path().join("alpha")).unwrap();
    std::fs::write(w.dir.path().join("alpha/proof.lean"), "theorem kept").unwrap();
    let begin = w.call("projects", "begin_archive", &["projects", "alpha"], json!({}));
    assert_eq!(w.reply(begin), Ok(json!({})));
    assert!(!w.exists("alpha"));
    let list = w.call("projects", "list", &["projects"], json!({}));
    let listed = w.reply(list).unwrap();
    assert_eq!(listed["projects"][0]["name"], "alpha");
    assert_eq!(listed["projects"][0]["archiving"], true);
    assert_ne!(listed["projects"][0]["archived"], true);
    let restore = w.call("projects", "unarchive", &["projects", "alpha"], json!({}));
    assert!(w.reply(restore).is_err(), "unfinished archiving cannot be restored");
    let create = w.call("projects", "create", &["projects", "alpha"], json!({}));
    assert!(w.reply(create).is_err(), "an archived name cannot be reused");
    let shell = w.call("shell", "exec", &["projects", "alpha", "repo"], json!({"command": "true"}));
    assert!(w.reply(shell).is_err(), "archiving prevents new execution");
    let stop = w.call("projects", "stop", &["projects", "alpha"], json!({}));
    assert_eq!(w.stopped(Ok(json!({}))), "alpha");
    assert!(w.reply(stop).is_ok(), "the archiving project's cgroup can still be stopped");
    let archive = w.call("projects", "archive", &["projects", "alpha"], json!({}));
    assert_eq!(w.reply(archive), Ok(json!({})));
    w.router = World::router([&w.dir, &w.jobs], Some(STOP), std::time::Duration::from_secs(600));
    let restore = w.call("projects", "unarchive", &["projects", "alpha"], json!({}));
    assert!(w.reply(restore).is_ok());
    assert_eq!(std::fs::read_to_string(w.dir.path().join("alpha/proof.lean")).unwrap(), "theorem kept");
    assert!(w.k.spawned.is_empty(), "restoring never starts work");
    let begin = w.call("projects", "begin_archive", &["projects", "alpha"], json!({}));
    assert!(w.reply(begin).is_ok());
    let archived = w.call("projects", "archive", &["projects", "alpha"], json!({}));
    assert!(w.reply(archived).is_ok());
    let deleted = w.call("projects", "delete", &["projects", "alpha"], json!({}));
    assert_eq!(w.reply(deleted), Ok(json!({"state": "deleting"})));
    let purge = w.call("projects", "purge", &["projects", "alpha"], json!({}));
    w.removed_by_its_host("alpha");
    assert!(w.reply(purge).is_ok() && !w.exists(".archived-alpha"));
}

impl World {
    /// A router with projects' processes when given [`STOP`], without when not.
    fn new(identities: Option<HandleId>) -> Self {
        Self::idle_after(identities, std::time::Duration::from_secs(600))
    }

    fn idle_after(identities: Option<HandleId>, idle: std::time::Duration) -> Self {
        let (dir, jobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let router = Self::router([&dir, &jobs], identities, idle);
        Self { dir, jobs, router, k: Recorder::default(), next: 0 }
    }

    /// A router over the homes and the jobs' records in `dirs`:
    /// built again over the same ones, the router after a restart.
    fn router(
        dirs: [&tempfile::TempDir; 2],
        identities: Option<HandleId>,
        idle: std::time::Duration,
    ) -> Router {
        let [dir, jobs] = dirs;
        let registry = Registry::open(dir.path(), identities.is_some()).unwrap();
        let authority = Authority {
            create: HandleId(1),
            spawn: HandleId(2),
            spawn_as: identities.map(|_| HandleId(3)),
            stop: identities,
            ready: (EndpointName::new("dynamic.ready").unwrap(), HandleId(4)),
            wake: Some(WAKE),
        };
        Router::new(registry, options(), authority, idle).with_jobs(Some(jobs.path().to_owned())).unwrap()
    }

    fn call(&mut self, endpoint: &str, action: &str, resource: &[&str], payload: Value) -> DispatchId {
        self.call_by(ProcessId(7), endpoint, action, resource, payload)
    }

    /// A call from `caller`, as the kernel asserts it.
    fn call_by(
        &mut self,
        caller: ProcessId,
        endpoint: &str,
        action: &str,
        resource: &[&str],
        payload: Value,
    ) -> DispatchId {
        self.next += 1;
        let d = Dispatch {
            id: DispatchId(self.next),
            endpoint: EndpointName::new(endpoint).unwrap(),
            caller,
            action: ActionName::new(action).unwrap(),
            resource: ResourcePath::new(resource.iter().copied()).unwrap(),
            payload: Payload::encode(&payload).unwrap(),
            remaining_ms: 60_000,
        };
        self.router.dispatch(&mut self.k, &d).unwrap();
        d.id
    }

    fn ready(&mut self, process: ProcessId, key: &str) -> Result<Value, ErrorCode> {
        self.next += 1;
        let d = Dispatch {
            id: DispatchId(self.next),
            endpoint: EndpointName::new("dynamic.ready").unwrap(),
            caller: process,
            action: ActionName::new("ready").unwrap(),
            resource: ResourcePath::new([key]).unwrap(),
            payload: Payload::encode(&json!({})).unwrap(),
            remaining_ms: 1_000,
        };
        self.router.dispatch(&mut self.k, &d).unwrap();
        self.reply(d.id)
    }

    /// The kernel reports the last project stop the router asked for: the
    /// project it was for.
    fn stopped(&mut self, result: Result<Value, Error>) -> String {
        let (request, authority, project) = self.k.stops.last().cloned().expect("a project stop");
        assert_eq!(authority, STOP);
        self.answer(request, result);
        project
    }

    /// A deletion's last step: once every process of `project` ended, its
    /// host, started for the folder's deleting name as the project, removes
    /// the folder (here by hand) and is stopped.
    fn removed_by_its_host(&mut self, project: &str) {
        assert_eq!(self.stopped(Ok(json!({}))), project);
        let (process, program, _, identity) = self.k.spawned.last().cloned().unwrap();
        let folder = format!("{}/.deleting-{project}", self.dir.path().display());
        assert!(program.args.windows(2).any(|a| a == ["--root", folder.as_str()]), "{program:?}");
        assert_eq!(identity.map(|i| i.project).as_deref(), Some(project));
        self.start_last();
        let (request, payload) = self.last("delete");
        assert_eq!(payload, json!({}));
        std::fs::remove_dir_all(&folder).unwrap();
        self.answer(request, Ok(json!({})));
        assert!(self.k.killed.contains(&process), "the host of a folder that is gone stops");
    }

    fn exists(&self, name: &str) -> bool {
        self.dir.path().join(name).exists()
    }

    fn answer(&mut self, request: RequestId, result: Result<Value, Error>) {
        let result = result.map(|v| Payload::encode(&v).unwrap());
        self.router.answer(&mut self.k, request, result).unwrap();
    }

    fn reply(&self, dispatch: DispatchId) -> Result<Value, ErrorCode> {
        self.k.replies.iter().find(|(d, _)| *d == dispatch).map(|(_, r)| r.clone()).expect("answered")
    }

    /// The host the last spawn started says it is ready.
    fn start_last(&mut self) -> ProcessId {
        let (process, _, grants, _) = self.k.spawned.last().unwrap().clone();
        self.ready(process, &Self::ready_key(&grants)).unwrap();
        process
    }

    /// The request of the last call forwarded to a host.
    fn forwarded(&self) -> RequestId {
        self.k.submitted.last().unwrap().0
    }

    /// The last call the router made with `action`: its request and payload.
    fn last(&self, action: &str) -> (RequestId, Value) {
        let found = self.k.submitted.iter().rev().find(|call| call.2 == action).expect("submitted");
        (found.0, found.4.clone())
    }

    fn ready_key(grants: &[Grant]) -> String {
        grants.iter().find(|g| g.actions.iter().any(|a| a.as_str() == "ready")).unwrap().resource.segments()
            [0]
        .clone()
    }
}

/// Creating a project without identities: its home is made, its host is
/// started on it, the call waits until that host says it serves (and only
/// it may say so), and the project is listed and takes calls once its
/// repository exists.
#[test]
fn a_project_is_created_by_its_own_host_and_listed_when_its_repository_exists() {
    let mut w = World::new(None);
    let created = w.call("projects", "create", &["projects", "alpha"], json!({}));
    let (process, program, grants, identity) = w.k.spawned[0].clone();
    assert!(identity.is_none(), "no identities, no identity");
    assert_eq!(program.executable, "/programs/nucleus-project");
    let binds: Vec<_> =
        program.binds.iter().map(|b| (b.path.as_str(), b.mount.as_deref(), b.writable)).collect();
    let home = format!("{}/alpha", w.dir.path().display());
    // Without projects' processes the host is a sandboxed child: it sees its
    // folder and the shared home where this service sees them.
    assert_eq!(binds, [(home.as_str(), None, true), ("/work/home", None, true)]);
    assert!(program.args.windows(2).any(|a| a == ["--root", home.as_str()]));
    assert!(w.k.submitted.is_empty(), "nothing is forwarded before the host is ready");
    let key = World::ready_key(&grants);
    assert_eq!(w.ready(ProcessId(999), &key), Err(ErrorCode::PermissionDenied), "only the host itself");
    w.ready(process, &key).unwrap();
    let (request, _, action, resource, _) = w.k.submitted[0].clone();
    assert_eq!((action.as_str(), resource.len()), ("init", 0));
    assert!(w.k.rebound.len() == 1, "the router's handle follows the host's provide");
    // Until it has a repository it is no project of the listing or of a call.
    assert!(w.exists("alpha") && w.exists(".creating-alpha"));
    let listed = w.call("projects", "list", &["projects"], json!({}));
    assert_eq!(w.reply(listed), Ok(json!({"projects": []})));
    let early = w.call("files", "list", &["projects", "alpha", "repo"], json!({"path": ""}));
    assert_eq!(w.reply(early), Err(ErrorCode::InvalidInput));
    w.answer(request, Ok(json!({})));
    let alpha = json!({"name": "alpha", "dir": "alpha"});
    assert_eq!(w.reply(created), Ok(alpha.clone()));
    let stopped = w.call("projects", "stop", &["projects", "alpha"], json!({}));
    assert_eq!(w.reply(stopped), Err(ErrorCode::Unavailable), "no project's processes to stop");
    assert!(!w.exists(".creating-alpha"));
    let listed = w.call("projects", "list", &["projects"], json!({}));
    assert_eq!(w.reply(listed), Ok(json!({"projects": [alpha]})));
    let again = w.call("projects", "create", &["projects", "alpha"], json!({}));
    assert_eq!(w.reply(again), Err(ErrorCode::InvalidInput));
    // A repository its host could not make takes the project with it.
    let failed = w.call("projects", "create", &["projects", "beta"], json!({}));
    w.start_last();
    let (request, init) = w.last("init");
    assert_eq!(init, json!({}));
    w.answer(request, Err(Error::new(ErrorCode::ServiceError, "git init failed")));
    assert_eq!(w.reply(failed), Err(ErrorCode::ServiceError));
    assert!(!w.exists("beta") && !w.exists(".deleting-beta") && !w.exists(".creating-beta"));
}

/// With projects' processes the project's host, started as the project,
/// makes its folder and its repository; one that cannot leaves nothing: the
/// project is deleted, its processes ended and its folder removed by its
/// host, before the caller hears why. Calls about a project go to its own
/// host only, as the project, with the project's prefix removed, and its
/// Stop ends every process of it.
#[test]
fn a_project_s_host_makes_its_folder_and_calls_reach_only_its_own_host() {
    let mut w = World::new(Some(STOP));
    let failed = w.call("projects", "create", &["projects", "beta"], json!({}));
    let (process, program, grants, identity) = w.k.spawned[0].clone();
    assert_eq!(identity.map(|i| i.project).as_deref(), Some("beta"), "as the project");
    assert!(program.args.windows(2).any(|a| a == ["--create", "yes"]), "it makes the folder: {program:?}");
    std::fs::create_dir(w.dir.path().join("beta")).unwrap(); // the host's
    w.ready(process, &World::ready_key(&grants)).unwrap();
    let (request, _, action, _, payload) = w.k.submitted[0].clone();
    assert_eq!((action.as_str(), payload), ("init", json!({})));
    w.answer(request, Err(Error::new(ErrorCode::ServiceError, "git init failed")));
    assert_eq!(w.reply(failed), Err(ErrorCode::ServiceError));
    assert!(w.k.refusals.last().unwrap().contains("git init failed"), "{:?}", w.k.refusals);
    assert!(w.k.killed.contains(&process), "its host is stopped");
    assert!(w.exists(".deleting-beta") && !w.exists("beta") && !w.exists(".creating-beta"));
    let listed = w.call("projects", "list", &["projects"], json!({}));
    assert_eq!(w.reply(listed).unwrap()["projects"][0]["deleting"], json!(true), "until its folder is gone");
    w.removed_by_its_host("beta");
    assert!(!w.exists(".deleting-beta"));

    std::fs::create_dir(w.dir.path().join("beta")).unwrap();
    std::fs::create_dir(w.dir.path().join("gamma")).unwrap();
    w.call("files", "list", &["projects", "beta", "repo"], json!({"path": ""}));
    let (process, program, grants, identity) = w.k.spawned.last().unwrap().clone();
    // As the projects' user it is in no sandbox: no binds, the folder and
    // the shared home by their own paths, the VM's network.
    let root_of = |program: &ChildProgram| {
        program.args.windows(2).find(|a| a[0] == "--root").map(|a| a[1].clone()).unwrap()
    };
    assert!(program.binds.is_empty() && program.network, "{program:?}");
    assert_eq!(root_of(&program), format!("{}/beta", w.dir.path().display()));
    assert!(program.args.windows(2).any(|a| a == ["--home", "/work/home"]), "git's home is the shared one");
    assert!(!program.args.iter().any(|a| a == "--create"), "{program:?}");
    assert_eq!(identity.map(|i| i.project).as_deref(), Some("beta"));
    w.ready(process, &World::ready_key(&grants)).unwrap();
    let (_, handle, action, resource, _) = w.k.submitted.last().unwrap().clone();
    assert_eq!((action.as_str(), resource), ("list", vec!["repo".to_owned()]));
    // Another project's call starts that project's host, never this one's.
    let spawned = w.k.spawned.len();
    w.call("files", "list", &["projects", "gamma", "repo"], json!({"path": ""}));
    assert_eq!(w.k.spawned.len(), spawned + 1);
    assert!(root_of(&w.k.spawned[spawned].1).ends_with("/gamma"));
    assert!(w.k.submitted.iter().filter(|s| s.1 == handle).count() == 1, "beta's call only");
    // Its Stop ends every process of the project, answered once none is left.
    let stopped = w.call("projects", "stop", &["projects", "beta"], json!({}));
    assert_eq!(w.stopped(Ok(json!({}))), "beta");
    assert_eq!(w.reply(stopped), Ok(json!({})));
    let nobody = w.call("projects", "stop", &["projects", "nobody"], json!({}));
    assert_eq!(w.reply(nobody), Err(ErrorCode::InvalidInput));
    // Nothing is sent for a call that names no project or no directory.
    let before = (w.k.spawned.len(), w.k.submitted.len());
    for (endpoint, action, resource) in [
        ("files", "list", vec!["projects", "beta"]),
        ("files", "list", vec!["projects", "nobody", "repo"]),
        ("diffs", "changes", vec!["projects", "beta", "repo"]),
    ] {
        let d = w.call(endpoint, action, &resource, json!({"path": ""}));
        assert!(w.reply(d).is_err(), "{endpoint} {action} {resource:?}");
    }
    assert_eq!((w.k.spawned.len(), w.k.submitted.len()), before);
}

/// Deleting a project: `delete` gives its folder its deleting name at once,
/// so nothing of it starts any more (no call reaches it, its name is not
/// taken again), forgets its job records first, so that their end wakes no
/// session, and stops its hosts; the project is listed as being deleted.
/// `purge` ends every process of it, then its host removes the folder, and
/// is answered once the folder is gone; a purge that fails is answered with
/// why and leaves the project being deleted, and `purge` again goes on.
#[test]
fn a_deleted_project_starts_nothing_and_is_purged_by_its_host() {
    let mut w = World::new(Some(STOP));
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    let registered = w.call_by(ProcessId(7), "jobs", "register", &["delta", "s1"], json!({}));
    w.reply(registered).unwrap();
    let job = start_job(&mut w, ProcessId(7), "c1");
    let record = w.jobs.path().join(format!("{job}.json"));
    let host = w.k.spawned.last().unwrap().0;
    let wakes = |w: &World| w.k.submitted.iter().filter(|call| call.2 == "wake").count();
    let before = wakes(&w);
    let deleting = json!({"state": "deleting"});
    let deleted = w.call("projects", "delete", &["projects", "delta"], json!({}));
    assert_eq!(w.reply(deleted), Ok(deleting.clone()));
    assert!(w.exists(".deleting-delta") && !w.exists("delta"));
    assert!(!record.exists() && wakes(&w) == before, "its jobs go first and wake no session");
    assert!(w.k.killed.contains(&host), "its hosts are stopped");
    assert!(w.k.stops.is_empty(), "its processes end only at its purge");
    let listed = w.call("projects", "list", &["projects"], json!({}));
    assert_eq!(
        w.reply(listed),
        Ok(json!({"projects": [{"name": "delta", "dir": "delta", "deleting": true}]}))
    );
    let spawned = w.k.spawned.len();
    let call = w.call("files", "list", &["projects", "delta", "repo"], json!({"path": ""}));
    assert_eq!(w.reply(call), Err(ErrorCode::InvalidInput));
    let shell = w.call("shell", "exec", &["projects", "delta", "repo"], json!({"command": "true"}));
    assert_eq!(w.reply(shell), Err(ErrorCode::InvalidInput));
    assert_eq!(w.k.spawned.len(), spawned, "nothing of it starts");
    let taken = w.call("projects", "create", &["projects", "delta"], json!({}));
    assert_eq!(w.reply(taken), Err(ErrorCode::InvalidInput));
    let again = w.call("projects", "delete", &["projects", "delta"], json!({}));
    assert_eq!(w.reply(again), Ok(deleting.clone()));
    // A purge whose processes do not end is answered with why; the project
    // stays being deleted.
    let purged = w.call("projects", "purge", &["projects", "delta"], json!({}));
    assert_eq!(w.k.stops.last().map(|c| c.2.as_str()), Some("delta"), "its processes end first");
    w.stopped(Err(Error::new(ErrorCode::ServiceError, "processes of delta keep running")));
    assert_eq!(w.reply(purged), Err(ErrorCode::ServiceError));
    assert!(w.k.refusals.last().unwrap().contains("keep running"));
    assert!(w.exists(".deleting-delta"));
    // Again: answered, together with a purge asked meanwhile, once its host
    // removed the folder.
    let purged = w.call("projects", "purge", &["projects", "delta"], json!({}));
    let joined = w.call("projects", "purge", &["projects", "delta"], json!({}));
    w.removed_by_its_host("delta");
    assert_eq!((w.reply(purged), w.reply(joined)), (Ok(json!({})), Ok(json!({}))));
    let gone = w.call("projects", "delete", &["projects", "delta"], json!({}));
    assert_eq!(w.reply(gone), Ok(json!({"state": "deleted"})));
    // A project being created is not deleted meanwhile.
    w.call("projects", "create", &["projects", "beta"], json!({}));
    let early = w.call("projects", "delete", &["projects", "beta"], json!({}));
    assert_eq!(w.reply(early), Err(ErrorCode::Unavailable));
}

/// A creation the runtime's restart interrupted is deleted at the next
/// start, its folder removed by its host or, without projects' processes,
/// by this service.
#[test]
fn an_interrupted_creation_is_deleted_at_the_next_start() {
    for identities in [Some(STOP), None] {
        let mut w = World::new(identities);
        std::fs::create_dir(w.dir.path().join("zeta")).unwrap();
        std::fs::write(w.dir.path().join(".creating-zeta"), "").unwrap();
        w.router.delete_interrupted(&mut w.k).unwrap();
        assert!(!w.exists(".creating-zeta") && !w.exists("zeta"));
        if identities.is_some() {
            assert!(w.exists(".deleting-zeta"));
            w.removed_by_its_host("zeta");
        }
        assert!(!w.exists(".deleting-zeta"));
    }
}

/// The projects are listed a page at a time, by name, whatever their
/// number: `next` names the last of a page, which the next one starts after.
#[test]
fn projects_are_listed_page_by_page() {
    let mut w = World::new(Some(STOP));
    for name in ["e", "a", "c", "b", ".deleting-d", ".free-61001", "Upper", ".creating-f", "f"] {
        std::fs::create_dir(w.dir.path().join(name)).unwrap();
    }
    std::fs::write(w.dir.path().join("g"), "").unwrap();
    let names = |page: &Value| -> Vec<String> {
        page["projects"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_owned()).collect()
    };
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let list = w.call("projects", "list", &["projects"], json!({"after": after, "limit": 2}));
        let page = w.reply(list).unwrap();
        assert!(names(&page).len() <= 2);
        seen.extend(names(&page));
        match page.get("next").and_then(Value::as_str) {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen, ["a", "b", "c", "d", "e"], "being created and not projects: not listed");
    let one = w.call("projects", "list", &["projects", "c"], json!({}));
    assert_eq!(names(&w.reply(one).unwrap()), ["c"], "a narrower caller sees its own");
    for limit in [0, 1001] {
        let bad = w.call("projects", "list", &["projects"], json!({"limit": limit}));
        assert_eq!(w.reply(bad), Err(ErrorCode::InvalidInput), "{limit}");
    }
}

/// The shells of a project's discovery workers work in the home's `runs/`
/// only, with the operator's environment and no project environment. There
/// are two, apart: `prover-shell` is started without network, which the
/// kernel then enforces, and `prover-shell-net` with it; each declares which
/// it is, so a run's workers have the network their capability names and are
/// told so.
#[test]
fn a_projects_workers_shells_work_in_its_runs_only_one_without_network_and_one_with() {
    let mut w = World::new(Some(STOP));
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    let command = json!({"command": "ls"});
    for endpoint in ["prover-shell", "prover-shell-net"] {
        let outside = w.call(endpoint, "exec", &["projects", "delta", "repo"], command.clone());
        assert_eq!(w.reply(outside), Err(ErrorCode::InvalidInput), "{endpoint}");
    }
    assert!(w.k.spawned.is_empty(), "refused before a host starts");
    let workspace = ["projects", "delta", "runs", "r1", "p", "t1"];
    for (n, (endpoint, network)) in
        [("prover-shell", "off"), ("prover-shell-net", "on")].into_iter().enumerate()
    {
        w.call(endpoint, "exec", &workspace, command.clone());
        assert_eq!(w.k.spawned.len(), n + 1, "{endpoint} is a host of its own");
        let (process, program, grants, _) = w.k.spawned[n].clone();
        assert_eq!(program.executable, "/programs/nucleus-shell");
        assert_eq!(program.network, network == "on", "{endpoint}: {program:?}");
        assert!(program.binds.is_empty());
        assert!(program.args.windows(2).any(|a| a == ["--network", network]), "{endpoint} is told which");
        assert!(!program.args.iter().any(|a| a.starts_with("HOME=")));
        assert!(program.args.windows(2).any(|a| a == ["--env", "FUNCNEST=32"]), "the operator's environment");
        w.ready(process, &World::ready_key(&grants)).unwrap();
        let (_, _, action, resource, _) = w.k.submitted.last().unwrap().clone();
        assert_eq!(
            (action.as_str(), resource),
            ("exec", vec!["runs".into(), "r1".into(), "p".into(), "t1".into()])
        );
    }
    // What each declares to the workers that hold it.
    let hosts = Options::parse(
        ["--root", "/homes", "--home", "/work/home", "--programs", "/programs"].map(str::to_owned),
    )
    .unwrap()
    .hosts;
    assert!(hosts.prover_shell_description.contains(nucleus_shell_interface::NETWORK_OFF));
    assert!(hosts.prover_shell_net_description.contains(nucleus_shell_interface::NETWORK_ON));
    // The sessions' shell has the projects' environment: the shared HOME,
    // and what is installed there first on PATH.
    w.call("shell", "exec", &["projects", "delta", "repo"], command);
    let program = &w.k.spawned.last().unwrap().1;
    let env: Vec<&str> = program.args.windows(2).filter(|a| a[0] == "--env").map(|a| a[1].as_str()).collect();
    let home = "/work/home";
    assert!(env.contains(&format!("HOME={home}").as_str()), "{env:?}");
    let path = env.iter().find_map(|e| e.strip_prefix("PATH=")).unwrap();
    assert!(path.starts_with(&format!("{home}/.local/bin:{home}/.cargo/bin:{home}/.elan/bin:")), "{path}");
}

/// Idle hosts are stopped, but never one with a call in flight.
#[test]
fn idle_hosts_stop_but_never_with_a_call_in_flight() {
    let mut w = World::idle_after(None, std::time::Duration::ZERO);
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    w.call("files", "list", &["projects", "delta", "repo"], json!({"path": ""}));
    let files = w.start_last();
    let listing = w.forwarded();
    w.router.sweep(&mut w.k).unwrap();
    assert!(w.k.killed.is_empty(), "it has a call in flight");
    w.answer(listing, Ok(json!({"entries": []})));
    w.router.sweep(&mut w.k).unwrap();
    assert_eq!(w.k.killed, [files], "the idle files host stops");
}

/// The agent's `exec` that starts a job in the background, answered by its
/// host: the job's id.
fn start_job(w: &mut World, agent: ProcessId, call: &str) -> String {
    let start = json!({"command": "gap -q scan.g", "background": true, "call": call});
    let hosts = w.k.spawned.len();
    let started = w.call_by(agent, "shell", "exec", &["projects", "delta", "repo"], start);
    if w.k.spawned.len() > hosts {
        w.start_last();
    }
    let (request, forwarded) = w.last("exec");
    let job = forwarded["job"].as_str().unwrap().to_owned();
    assert_eq!((&forwarded["call"], &forwarded["background"]), (&json!(call), &json!(true)));
    let output = format!("/homes/delta/.nucleus/jobs/{job}.log");
    let detached = json!({"id": job, "by": "agent", "output": output, "note": ""});
    let answer = json!({"exit_code": null, "timed_out": false, "output": "", "truncated": false, "elapsed_ms": 4, "job": detached});
    w.answer(request, Ok(answer));
    w.reply(started).unwrap();
    job
}

/// Background jobs: only a session's registered agent starts
/// one, and its call answers at once with the job; every session of the
/// project sees it, but only through its own session is it stopped, so
/// another session's agent is told it is someone else's work. The agent's
/// `stop` names no person and wakes no one, also from an agent that has not
/// registered yet. A host is
/// not stopped as idle while a job runs in it, and is once its jobs ended.
/// At most sixteen jobs of a project run at once, its sessions together, and
/// one more is refused saying whose they are.
#[test]
fn only_a_registered_agent_starts_a_job_and_only_its_own_session_stops_it() {
    let mut w = World::idle_after(None, std::time::Duration::ZERO);
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    let (agent, other, next_agent) = (ProcessId(7), ProcessId(8), ProcessId(10));
    let start = json!({"command": "gap -q scan.g", "background": true, "call": "c1"});
    let refused = w.call_by(agent, "shell", "exec", &["projects", "delta", "repo"], start);
    assert_eq!(w.reply(refused), Err(ErrorCode::InvalidInput), "not a registered agent");
    for (process, session) in [(agent, "s1"), (other, "s2")] {
        let registered = w.call_by(process, "jobs", "register", &["delta", session], json!({}));
        w.reply(registered).unwrap();
    }
    let job = start_job(&mut w, agent, "c1");
    let host = w.k.spawned.last().unwrap().0;
    assert_eq!(w.k.submitted.last().unwrap().2, "wait", "the router follows the job");
    // A wait the kernel refused says nothing of the job: it runs on.
    let (wait, _) = w.last("wait");
    w.answer(wait, Err(Error::new(ErrorCode::LimitExceeded, "too many outstanding requests")));
    let second = start_job(&mut w, agent, "c2");

    let listed = w.call_by(other, "jobs", "list", &["delta", "s2"], json!({}));
    let listed = w.reply(listed).unwrap();
    let seen: Vec<_> =
        listed["jobs"].as_array().unwrap().iter().map(|job| (&job["session"], &job["state"])).collect();
    assert_eq!(seen, [(&json!("s1"), &json!("running")); 2], "the project's jobs are seen");
    let theirs = w.call_by(other, "jobs", "stop", &["delta", "s2"], json!({"job": job}));
    assert_eq!(w.reply(theirs), Err(ErrorCode::InvalidInput));
    w.router.sweep(&mut w.k).unwrap();
    assert!(!w.k.killed.contains(&host), "a host that runs a job is not idle");

    let status = || json!({"state": "stopped", "elapsed_ms": 9_000, "size": 0, "tail": ""});
    let stopped = w.call_by(agent, "jobs", "stop", &["delta", "s1"], json!({"job": job}));
    let (request, _) = w.last("stop");
    w.answer(request, Ok(status()));
    assert_eq!(w.reply(stopped).unwrap()["job"]["state"], "stopped");
    // The session's next agent, which has run no command yet.
    let wakes = w.k.submitted.iter().filter(|call| call.2 == "wake").count();
    let named = json!({"job": second, "author": "Lei"});
    let named = w.call_by(next_agent, "jobs", "stop", &["delta", "s1"], named);
    assert_eq!(w.reply(named), Err(ErrorCode::InvalidInput), "an agent's stop names no person");
    let own = w.call_by(next_agent, "jobs", "stop", &["delta", "s1"], json!({"job": second}));
    let (request, _) = w.last("stop");
    w.answer(request, Ok(status()));
    let ended = w.reply(own).unwrap();
    assert_eq!((&ended["job"]["stopped_by"], ended["job"].get("stopped_author")), (&json!("agent"), None));
    assert_eq!(w.k.submitted.iter().filter(|call| call.2 == "wake").count(), wakes, "its own call told it");

    // The waits that were out are answered after the jobs' ends were recorded.
    let waits: Vec<RequestId> =
        w.k.submitted.iter().filter(|call| call.2 == "wait").map(|call| call.0).collect();
    for request in waits {
        w.answer(request, Ok(status()));
    }
    w.router.sweep(&mut w.k).unwrap();
    assert!(w.k.killed.contains(&host), "a host whose jobs have ended is idle again");

    for n in 0..16 {
        start_job(&mut w, if n < 10 { agent } else { other }, &format!("r{n}"));
    }
    let more = json!({"command": "gap -q scan.g", "background": true, "call": "c7"});
    let more = w.call_by(agent, "shell", "exec", &["projects", "delta", "repo"], more);
    assert_eq!(w.reply(more), Err(ErrorCode::LimitExceeded), "sixteen jobs of a project run at once");
    let refusal = w.k.refusals.last().unwrap();
    assert!(refusal.contains("s1") && refusal.contains("s2"), "whose they are: {refusal}");

    // A command whose answer is lost (its caller withdrew the call as the
    // host made it a job) may have left a job nobody knows: it is stopped.
    let lost = json!({"command": "gap -q scan.g", "call": "c8"});
    let lost = w.call_by(agent, "shell", "exec", &["projects", "delta", "repo"], lost);
    let (request, forwarded) = w.last("exec");
    w.answer(request, Err(Error::new(ErrorCode::UnknownOutcome, "the caller withdrew the call")));
    assert_eq!(
        (w.last("stop").1["job"].clone(), w.reply(lost)),
        (forwarded["job"].clone(), Err(ErrorCode::UnknownOutcome))
    );
}

/// Every job's record is kept, however many and however old (these ended in
/// 1970), and read again
/// at a restart; the list pages them, running before ended, each newest
/// first, every one once, with its last lines read back from its record.
#[test]
fn every_job_is_kept_and_listed_page_by_page() {
    let mut w = World::new(None);
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    for n in 0..30u64 {
        let (session, id) = (if n % 3 == 0 { "s2" } else { "s1" }, format!("j{}", 1000 + n));
        let job = json!({"job": id, "session": session, "command": "gap", "state": "exited",
            "started_at_ms": 1_000 + n, "ended_at_ms": 2_000 + n, "exit_code": 0, "by": "agent",
            "output": format!("/homes/delta/.nucleus/jobs/{id}.log"), "size": 3, "tail": format!("tail {n}\n")});
        let record =
            json!({"v": 1, "session": {"project": "delta", "name": session}, "job": job, "told": true});
        std::fs::write(w.jobs.path().join(format!("{id}.json")), record.to_string()).unwrap();
    }
    w.router = World::router([&w.dir, &w.jobs], None, std::time::Duration::ZERO);
    let registered = w.call_by(ProcessId(7), "jobs", "register", &["delta", "s1"], json!({}));
    w.reply(registered).unwrap();
    let running = start_job(&mut w, ProcessId(7), "c1");
    let list = |w: &mut World, request: Value| {
        let listed = w.call_by(ProcessId(9), "jobs", "list", &["delta", "s1"], request);
        w.reply(listed)
    };
    let (mut seen, mut after) = (Vec::new(), None::<String>);
    loop {
        let page = list(&mut w, json!({"limit": 7, "after": after})).unwrap();
        assert_eq!(page["running"], 1);
        for job in page["jobs"].as_array().unwrap() {
            if job["state"] == "exited" {
                let n = job["job"].as_str().unwrap()[1..].parse::<u64>().unwrap() - 1000;
                assert_eq!(job["tail"], format!("tail {n}\n"), "read back from its record");
            }
            seen.push(job["job"].as_str().unwrap().to_owned());
        }
        match page.get("next").and_then(Value::as_str) {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    let ended: Vec<String> = (0..30).rev().map(|n| format!("j{}", 1000 + n)).collect();
    assert_eq!(seen, [vec![running.clone()], ended].concat(), "running first, then newest first, all once");
    // A page that holds the rest exactly ends the list.
    let whole = list(&mut w, json!({"limit": 31})).unwrap();
    assert_eq!((whole["jobs"].as_array().unwrap().len(), whole.get("next")), (31, None));
    let own = list(&mut w, json!({"scope": "session", "limit": 200})).unwrap();
    assert_eq!(own["jobs"].as_array().unwrap().len(), 21, "s1's own: 20 ended and the running one");
    for bad in
        [json!({"limit": 0}), json!({"limit": 201}), json!({"after": "j1"}), json!({"after": "ended.x/y"})]
    {
        assert_eq!(list(&mut w, bad.clone()), Err(ErrorCode::InvalidInput), "{bad}");
    }
}

/// A restart of the runtime ends every job: the record of
/// one that ran then says so and still names its output, and its session is
/// woken with that.
#[test]
fn a_job_that_ran_when_the_runtime_restarted_is_recorded_interrupted_and_its_session_woken() {
    let mut w = World::new(None);
    std::fs::create_dir(w.dir.path().join("delta")).unwrap();
    let registered = w.call_by(ProcessId(7), "jobs", "register", &["delta", "s1"], json!({}));
    w.reply(registered).unwrap();
    let job = start_job(&mut w, ProcessId(7), "c1");

    w.router = World::router([&w.dir, &w.jobs], None, std::time::Duration::ZERO);
    w.router.wake_untold(&mut w.k).unwrap();
    let (_, handle, action, session, wake) = w.k.submitted.last().unwrap().clone();
    assert_eq!((handle, action.as_str(), session), (WAKE, "wake", vec!["delta".to_owned(), "s1".to_owned()]));
    assert!(wake["text"].as_str().unwrap().contains("was interrupted"), "{wake}");
    let listed = w.call_by(ProcessId(9), "jobs", "list", &["delta", "s1"], json!({}));
    let listed = &w.reply(listed).unwrap()["jobs"][0];
    let output = format!("/homes/delta/.nucleus/jobs/{job}.log");
    assert_eq!(
        (&listed["job"], &listed["state"], &listed["output"]),
        (&json!(job), &json!("interrupted"), &json!(output))
    );

    // The runtime's end takes a job's host first: the wake-up the ending
    // runtime refuses is made at the next start, and says what happened;
    // the one the session took is not made again.
    let (taken, _) = w.last("wake");
    w.answer(taken, Ok(json!({"id": "r1", "due_at_ms": 0})));
    let registered = w.call_by(ProcessId(7), "jobs", "register", &["delta", "s1"], json!({}));
    w.reply(registered).unwrap();
    let second = start_job(&mut w, ProcessId(7), "c2");
    let host = w.k.spawned.last().unwrap().0;
    w.router.child_exited(&mut w.k, host).unwrap();
    let (refused, _) = w.last("wake");
    w.answer(refused, Err(Error::new(ErrorCode::ShuttingDown, "kernel is shutting down")));
    let before = w.k.submitted.len();
    w.router = World::router([&w.dir, &w.jobs], None, std::time::Duration::ZERO);
    w.router.wake_untold(&mut w.k).unwrap();
    let told: Vec<&str> =
        w.k.submitted[before..].iter().map(|call| call.4["text"].as_str().unwrap()).collect();
    assert!(
        matches!(told[..], [text] if text.contains(&second) && text.contains("runtime restarted")),
        "{told:?}"
    );
}
