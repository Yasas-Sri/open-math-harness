//! The skills service: the provider of the `skills` interface.
//!
//! As a project's host it runs as the project, an ordinary process of the
//! projects' Linux user, with the project's folder as its root; a call's
//! resource is the repository beneath it (`["repo"]`), whose `.skill/` holds
//! the project's skills (module `store`). `list` reads them and the catalogs the operator named (`--catalog`); `load` returns one, and
//! first copies a published one into `.skill/<name>/` (module `sources`).
//!
//! Authority: the folder, as the projects' user may reach it, and the VM's
//! network, which every process of a project has; its fetches go through the
//! `web` service's checks all the same. It holds no capability beyond
//! providing its endpoint. A compromised instance reaches what any process
//! of the project reaches, no more. What a skill says is untrusted input: it
//! reaches a model as a tool result and carries no authority.

mod front_matter;
mod sources;
mod store;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Event};
use nucleus_skills_interface::{
    ACTION_LIST, ACTION_LOAD, DIRECTORY, ENDPOINT, ListRequest, ListResponse, LoadRequest, LoadResponse,
    MAX_DESCRIPTION_CHARS, MAX_SKILLS, Skill, interface, valid_name,
};

pub use sources::Sources;
pub use store::Store;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub root: PathBuf,
    pub endpoint: EndpointName,
    /// The catalogs' `index.json` URLs, in the order they are searched.
    pub catalogs: Vec<String>,
}

impl Options {
    /// Parses `--root <dir> [--endpoint <name>] [--catalog <url>]...`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut root = None;
        let mut endpoint = ENDPOINT.to_owned();
        let mut catalogs = Vec::new();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--endpoint" => endpoint = value,
                "--catalog" if value.starts_with("https://") => catalogs.push(value),
                "--catalog" => return Err(format!("--catalog must be an https URL, not {value:?}")),
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            root: root.ok_or("--root is required")?,
            endpoint: EndpointName::new(&endpoint).map_err(|e| e.to_string())?,
            catalogs,
        })
    }
}

/// Kept back from the caller's deadline so the answer reaches it.
const REPLY_MARGIN: Duration = Duration::from_millis(250);

pub struct Service {
    store: Store,
    sources: Sources,
}

impl Service {
    pub fn new(store: Store, sources: Sources) -> Self {
        Self { store, sources }
    }

    /// The repository's skills, then the published ones it does not have.
    pub fn list(&self, repository: &[String], deadline: Instant) -> Result<ListResponse, Error> {
        let mut skills: Vec<Skill> = self
            .store
            .list(repository)?
            .into_iter()
            .map(|found| Skill {
                name: found.name,
                description: found.description,
                installed: true,
                catalog: None,
            })
            .collect();
        let mut unreachable = Vec::new();
        let mut published = Vec::new();
        for catalog in self.sources.catalogs() {
            match self.sources.catalog(catalog, deadline) {
                Ok(entries) => {
                    for entry in entries {
                        let known = skills.iter().chain(&published).any(|s: &Skill| s.name == entry.name);
                        if !known {
                            published.push(Skill {
                                name: entry.name,
                                description: entry.description.chars().take(MAX_DESCRIPTION_CHARS).collect(),
                                installed: false,
                                catalog: Some(catalog.clone()),
                            });
                        }
                    }
                }
                Err(why) => unreachable.push(format!("{catalog}: {why}")),
            }
        }
        published.sort_by(|a, b| a.name.cmp(&b.name));
        skills.extend(published);
        let truncated = skills.len() > MAX_SKILLS;
        skills.truncate(MAX_SKILLS);
        Ok(ListResponse { skills, unreachable, truncated })
    }

    /// One skill, copied into the repository first when it has none of
    /// that name.
    pub fn load(
        &self,
        repository: &[String],
        request: &LoadRequest,
        deadline: Instant,
    ) -> Result<LoadResponse, Error> {
        let mut note = None;
        let (name, installed, source) = match (&request.name, &request.url) {
            (Some(name), None) => {
                if !valid_name(name) {
                    return Err(Error::new(ErrorCode::InvalidInput, format!("{name:?} is not a skill name")));
                }
                if self.store.has(repository, name)? {
                    (name.clone(), false, None)
                } else {
                    let (catalog, entry) = self.sources.find(name, deadline)?;
                    let fetched = self.sources.fetch_entry(&catalog, &entry, deadline)?;
                    self.store.add(repository, &fetched.name, &fetched.files)?;
                    (fetched.name, true, Some(catalog))
                }
            }
            (None, Some(url)) => {
                let fetched = self.sources.fetch_url(url, deadline)?;
                // A skill the project holds already is loaded as the project holds it (it
                // may have been changed there), not replaced: the same `/skill URL` in a
                // second conversation loads it.
                if self.store.has(repository, &fetched.name)? {
                    note = Some(format!(
                        "The project already has a skill named {}; its own copy in .skill/{} was loaded, \
                         not the one at {url}. Remove or rename that folder to load the other.",
                        fetched.name, fetched.name
                    ));
                    (fetched.name, false, None)
                } else {
                    self.store.add(repository, &fetched.name, &fetched.files)?;
                    (fetched.name, true, Some(url.clone()))
                }
            }
            _ => {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "give a skill's `name` or its `url`, not both",
                ));
            }
        };
        let read = self.store.read(repository, &name)?;
        Ok(LoadResponse {
            directory: format!("{DIRECTORY}/{name}"),
            name,
            installed,
            source,
            instructions: read.instructions,
            truncated: read.truncated,
            files: read.files,
            note,
        })
    }

    fn serve(
        &self,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
        remaining_ms: u64,
    ) -> Result<Payload, Error> {
        let deadline = Instant::now() + Duration::from_millis(remaining_ms).saturating_sub(REPLY_MARGIN);
        let repository = resource.segments();
        match action {
            ACTION_LIST => {
                payload.decode::<ListRequest>()?;
                Payload::encode(&self.list(repository, deadline)?)
            }
            ACTION_LOAD => Payload::encode(&self.load(repository, &payload.decode()?, deadline)?),
            _ => Err(Error::new(ErrorCode::InvalidInput, "unknown action")),
        }
    }
}

/// Provides the endpoint and serves calls until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let store = Store::open(&options.root).map_err(|e| format!("open root: {e}"))?;
    let service = Service::new(store, Sources::new(options.catalogs.clone())?);
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
    loop {
        match kernel.next_event().map_err(|e| e.to_string())? {
            Event::Dispatch(d) => {
                let result = service.serve(d.action.as_str(), &d.resource, &d.payload, d.remaining_ms);
                kernel.reply(d.id, result).map_err(|e| e.to_string())?;
            }
            Event::CancelDispatch(_) | Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn service(catalogs: Vec<String>) -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.skill/mine")).unwrap();
        std::fs::write(
            dir.path().join("repo/.skill/mine/SKILL.md"),
            "---\nname: mine\ndescription: Ours.\n---\nDo it.\n",
        )
        .unwrap();
        let service = Service::new(Store::open(dir.path()).unwrap(), Sources::new(catalogs).unwrap());
        (dir, service)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn the_projects_skills_are_listed_and_loaded_and_an_unreadable_catalog_is_named() {
        // A catalog on a local address is refused before anything is sent.
        let (_dir, service) = service(vec!["https://127.0.0.1/index.json".into()]);
        let repo = vec!["repo".to_owned()];
        let listed = service.list(&repo, soon()).unwrap();
        assert_eq!(
            listed.skills,
            vec![Skill { name: "mine".into(), description: "Ours.".into(), installed: true, catalog: None }]
        );
        assert_eq!(listed.unreachable.len(), 1);
        let loaded =
            service.load(&repo, &LoadRequest { name: Some("mine".into()), url: None }, soon()).unwrap();
        assert_eq!((loaded.directory.as_str(), loaded.installed), (".skill/mine", false));
        assert!(loaded.instructions.contains("Do it."));
        let missing = service.load(&repo, &LoadRequest { name: Some("other".into()), url: None }, soon());
        assert!(missing.unwrap_err().message.contains("no catalog lists one"));
        for request in [
            LoadRequest { name: None, url: None },
            LoadRequest { name: Some("a".into()), url: Some("https://x.org/a.zip".into()) },
            LoadRequest { name: None, url: Some("http://x.org/a.zip".into()) },
            LoadRequest { name: Some("../x".into()), url: None },
        ] {
            assert_eq!(
                service.load(&repo, &request, soon()).map_err(|e| e.code),
                Err(ErrorCode::InvalidInput)
            );
        }
    }

    #[test]
    #[ignore = "reaches the public catalog"]
    fn a_published_skill_is_loaded_into_the_project() {
        let catalog = "https://skills.openmathmodel.org/index.json".to_owned();
        let (dir, service) = service(vec![catalog.clone()]);
        let repo = vec!["repo".to_owned()];
        let later = || Instant::now() + Duration::from_secs(60);
        let listed = service.list(&repo, later()).unwrap();
        assert!(listed.unreachable.is_empty(), "{:?}", listed.unreachable);
        assert!(listed.skills.iter().any(|s| s.name == "lean-workflow" && !s.installed));
        let loaded = service
            .load(&repo, &LoadRequest { name: Some("lean-workflow".into()), url: None }, later())
            .unwrap();
        assert!(loaded.installed && loaded.instructions.contains("name: lean-workflow"), "{loaded:?}");
        assert!(dir.path().join("repo/.skill/lean-workflow/SKILL.md").is_file());
        let url = "https://skills.openmathmodel.org/lean-naming-style.zip".to_owned();
        let loaded = service.load(&repo, &LoadRequest { name: None, url: Some(url) }, later()).unwrap();
        assert_eq!(loaded.name, "lean-naming-style");
        assert!(!loaded.files.is_empty());
    }
}
