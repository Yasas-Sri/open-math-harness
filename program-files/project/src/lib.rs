//! `nucleus-project`: one project's host for its folder, its git repository
//! and its files. It runs as the projects' user, in the project's cgroup,
//! started on demand by the `projects` service, which keeps the only handle
//! on its endpoint and forwards the window's calls to it with the project's
//! prefix removed from their resources. It works in the project's folder
//! (`--root`), which it makes first when started to create the project
//! (`--create`); git, `/usr/bin/git` (in the VM, the image's), runs with
//! `--home` (the projects' shared home) as `HOME`.
//!
//! Interface: `nucleus_projects_interface::host_interface`. Resources are
//! relative to the folder: `[]` for the project (`init`, and `delete`, which
//! removes the folder with everything in it), and `["repo"]`
//! for the repository (the files actions).
//!
//! What it keeps: the uploads in progress, in memory and in the folder's
//! `.nucleus/uploads`; the staged git bundles in `.nucleus/bundles`
//! (`bundles.rs`); removed entries on their way out in `.nucleus/trash`.
//! All are emptied when it starts: fail and restart clean.
//!
//! Calls run on threads of their own, [`Options::max_inflight`] at once, so
//! git work that takes minutes holds up no file transfer; the rest wait in
//! arrival order. A call whose caller gives up has its git process group
//! killed.

mod bundles;
mod files;
mod git;
mod home;
mod repos;
mod uploads;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nucleus_projects_interface::{
    ACTION_BUNDLE_IMPORT, ACTION_BUNDLE_OUT, ACTION_BUNDLE_READ, ACTION_BUNDLE_WRITE, ACTION_DELETE,
    ACTION_DOWNLOAD, ACTION_GIT_INITIALIZE, ACTION_GIT_REMOTE, ACTION_GIT_STATE, ACTION_INIT, ACTION_LIST,
    ACTION_MKDIR, ACTION_REMOVE, ACTION_RENAME, ACTION_UPLOAD, BundleImport, BundleOut, BundleRead,
    BundleWrite, Dir, DownloadRequest, Empty, GIT_TIMEOUT_MS, GitInitialize, GitRemote, ListRequest,
    PathRequest, RenameRequest, UploadRequest, host_interface,
};
use nucleus_protocol::{
    ACTION_PROVIDE, DispatchId, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Dispatch, Event};

pub use repos::check_url;

/// Kept back from the caller's deadline so the reply arrives before it.
const REPLY_MARGIN: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(20);
/// The name this host gives its own working directories (it never shows):
/// the router knows the project's name, the host only its folder.
const SELF: &str = "project";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The project's folder.
    pub root: PathBuf,
    pub endpoint: EndpointName,
    /// The temporary directory the router gives the host (the VM's `/tmp`), the
    /// only other place git may write.
    pub tmp: PathBuf,
    /// Git's `HOME`: the projects' shared home; the folder when not given.
    pub home: Option<PathBuf>,
    /// Make the folder, which must not exist yet, before serving it.
    pub create: bool,
    pub max_inflight: usize,
}

impl Options {
    /// Parses `--root <folder> --endpoint <name> [--tmp <dir>] [--home <dir>]
    /// [--create yes] [--max-inflight <n>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut root, mut endpoint) = (None, None);
        let mut options = Options {
            root: PathBuf::new(),
            endpoint: EndpointName::new("project").map_err(|e| e.to_string())?,
            tmp: PathBuf::from("/tmp"),
            home: None,
            create: false,
            max_inflight: 8,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--endpoint" => endpoint = Some(EndpointName::new(value).map_err(|e| e.to_string())?),
                "--tmp" => options.tmp = PathBuf::from(value),
                "--home" => options.home = Some(PathBuf::from(value)),
                "--create" if value == "yes" => options.create = true,
                "--max-inflight" => {
                    options.max_inflight =
                        value.parse().ok().filter(|n| *n > 0).ok_or("--max-inflight must be at least 1")?;
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        options.root = root.ok_or("--root is required")?;
        options.endpoint = endpoint.ok_or("--endpoint is required")?;
        Ok(options)
    }
}

/// What the host holds: the folder, its git, the uploads in progress.
pub struct Host {
    home: home::Home,
    root: PathBuf,
    git: git::Git,
    uploads: Mutex<uploads::Uploads>,
    bundles: bundles::Bundles,
}

fn decode<T: serde::de::DeserializeOwned>(payload: &Payload) -> Result<T, Error> {
    payload.decode()
}

impl Host {
    pub fn open(options: &Options) -> Result<Self, String> {
        if options.create {
            home::make(&options.root)?;
        }
        let home = home::Home::open(&options.root)?;
        let git_home = options.home.clone().unwrap_or_else(|| options.root.clone());
        let git = git::Git::new(git_home, options.tmp.clone());
        Ok(Self {
            home,
            root: options.root.clone(),
            git,
            uploads: Mutex::default(),
            bundles: Default::default(),
        })
    }

    /// Serves one call. `deadline` is when the reply must be ready; `cancel`
    /// is set when the caller gives up.
    pub fn handle(
        &self,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Payload, Error> {
        let git_deadline = deadline.min(Instant::now() + Duration::from_millis(GIT_TIMEOUT_MS));
        let project_wide = || {
            if resource.segments().is_empty() {
                Ok(())
            } else {
                Err(Error::new(ErrorCode::InvalidInput, format!("{action} is called on the project, []")))
            }
        };
        let done = || Payload::encode(&serde_json::json!({}));
        match action {
            ACTION_INIT => {
                project_wide()?;
                decode::<Empty>(payload)?;
                repos::init(&self.git, &self.home, git_deadline, cancel)?;
                done()
            }
            ACTION_DELETE => {
                project_wide()?;
                decode::<Empty>(payload)?;
                home::remove(&self.root)?;
                done()
            }
            _ => self.in_directory(action, resource, payload, git_deadline, cancel),
        }
    }

    /// The actions on one working directory; git runs until `git_deadline`.
    fn in_directory(
        &self,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        git_deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Payload, Error> {
        let dir = Dir::from_home_resource(SELF, resource)?;
        let root = &self.home;
        let working = root.working(&dir)?;
        let done = || Payload::encode(&serde_json::json!({}));
        match action {
            ACTION_LIST => {
                let request: ListRequest = decode(payload)?;
                let after = request.after.as_deref();
                Payload::encode(&files::list(&working, &request.path, after)?)
            }
            ACTION_DOWNLOAD => {
                let request: DownloadRequest = decode(payload)?;
                Payload::encode(&files::download(&working, &request.path, request.offset)?)
            }
            ACTION_UPLOAD => {
                let request: UploadRequest = decode(payload)?;
                let mut uploads = self.uploads.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                Payload::encode(&uploads.chunk(root, &dir, &working, request)?)
            }
            ACTION_MKDIR => {
                let request: PathRequest = decode(payload)?;
                files::mkdir(&working, &request.path)?;
                done()
            }
            ACTION_RENAME => {
                let request: RenameRequest = decode(payload)?;
                files::rename(&working, &request.from, &request.to)?;
                done()
            }
            ACTION_REMOVE => {
                let request: PathRequest = decode(payload)?;
                files::remove(root, &working, &request.path)?;
                done()
            }
            ACTION_GIT_INITIALIZE => {
                let request: GitInitialize = decode(payload)?;
                bundles::initialize(
                    &self.git,
                    &working,
                    &request.branch,
                    &request.oid,
                    git_deadline,
                    cancel,
                )?;
                done()
            }
            ACTION_GIT_REMOTE => {
                let request: GitRemote = decode(payload)?;
                bundles::remote(&self.git, &working, &request.url, git_deadline, cancel)?;
                done()
            }
            ACTION_GIT_STATE => {
                decode::<Empty>(payload)?;
                Payload::encode(&bundles::state(&self.git, &working, git_deadline, cancel)?)
            }
            ACTION_BUNDLE_OUT => {
                let request: BundleOut = decode(payload)?;
                let bundle = self.bundles.out(root, &self.git, &working, &request, git_deadline, cancel)?;
                Payload::encode(&bundle)
            }
            ACTION_BUNDLE_READ => {
                let request: BundleRead = decode(payload)?;
                Payload::encode(&self.bundles.read(root, &request)?)
            }
            ACTION_BUNDLE_WRITE => {
                let request: BundleWrite = decode(payload)?;
                Payload::encode(&self.bundles.write(root, &request)?)
            }
            ACTION_BUNDLE_IMPORT => {
                let request: BundleImport = decode(payload)?;
                let imported =
                    self.bundles.import(root, &self.git, &working, &request, git_deadline, cancel)?;
                Payload::encode(&imported)
            }
            _ => Err(Error::new(ErrorCode::InvalidInput, "unknown action")),
        }
    }
}

/// Provides the endpoint, says it is ready, and serves until told to stop.
pub fn run(options: &Options) -> Result<(), String> {
    let host = Arc::new(Host::open(options)?);
    files::clear_staging(&host.home);
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    kernel
        .provide(authority, host_interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    kernel.announce_ready().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    serve(&host, &mut kernel, options.max_inflight)
}

type Finished = (DispatchId, Result<Payload, Error>);

/// Serves dispatches, each on a thread of its own, `max_inflight` at once.
fn serve<R: Read, W: Write>(
    host: &Arc<Host>,
    kernel: &mut Connection<R, W>,
    max_inflight: usize,
) -> Result<(), String> {
    let (report, finished): (Sender<Finished>, Receiver<Finished>) = mpsc::channel();
    let mut running: HashMap<DispatchId, Arc<AtomicBool>> = HashMap::new();
    let mut waiting: VecDeque<(Dispatch, Instant)> = VecDeque::new();
    loop {
        while let Ok((id, result)) = finished.try_recv() {
            running.remove(&id);
            kernel.reply(id, result).map_err(|e| e.to_string())?;
        }
        while running.len() < max_inflight
            && let Some((dispatch, arrived)) = waiting.pop_front()
        {
            let deadline = arrived + Duration::from_millis(dispatch.remaining_ms);
            let Some(ready_by) = deadline.checked_sub(REPLY_MARGIN).filter(|t| *t > Instant::now()) else {
                let late =
                    Error::new(ErrorCode::Timeout, "the caller's deadline passed before the call started");
                kernel.reply(dispatch.id, Err(late)).map_err(|e| e.to_string())?;
                continue;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            running.insert(dispatch.id, Arc::clone(&cancel));
            let (host, report) = (Arc::clone(host), report.clone());
            std::thread::spawn(move || {
                let result = host.handle(
                    dispatch.action.as_str(),
                    &dispatch.resource,
                    &dispatch.payload,
                    ready_by,
                    &cancel,
                );
                let _ = report.send((dispatch.id, result));
            });
        }
        let event = if running.is_empty() {
            Some(kernel.next_event().map_err(|e| e.to_string())?)
        } else {
            kernel.next_event_within(POLL).map_err(|e| e.to_string())?
        };
        match event {
            None => {}
            Some(Event::Dispatch(dispatch)) => waiting.push_back((dispatch, Instant::now())),
            Some(Event::CancelDispatch(id)) => {
                if let Some(cancel) = running.get(&id) {
                    cancel.store(true, Ordering::SeqCst);
                } else if let Some(at) = waiting.iter().position(|(d, _)| d.id == id) {
                    waiting.remove(at);
                    let cancelled = Error::new(ErrorCode::Cancelled, "cancelled before it started");
                    kernel.reply(id, Err(cancelled)).map_err(|e| e.to_string())?;
                }
            }
            Some(Event::ChildExited(_) | Event::ChildReaped { .. }) => {}
            Some(Event::Terminate) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
