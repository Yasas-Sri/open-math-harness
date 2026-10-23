//! Deleting a project: the platform's `projects.delete`, for
//! the team's owner (module `access`), is a workflow of this service, the
//! last the runtime boots, because it calls services that start after the
//! projects service and use its endpoints. In order:
//!
//! 1. the projects service's `delete`: the folder takes its deleting name, so
//!    none of the project starts any more; its jobs go and its hosts stop;
//! 2. `forget` on `[project]` of each service that keeps records of it, in
//!    [`FORGET_ORDER`], each once the one before answered;
//! 3. the projects service's `purge`: every process of the project ends and
//!    its folder is removed.
//!
//! A request is answered with the answer to step 1 (`deleting`, or `deleted`
//! when nothing of the project is left); asked while the deletion goes on,
//! `deleting`. A step that fails stops the deletion: the next request is
//! answered with why, and starts nothing, so a caller that asks until the
//! project is gone (the platform) stops at the failure; the request after it
//! starts the deletion again from step 1, every step being safe to repeat.
//! Nothing here outlives the process: after a restart the next request
//! starts again from step 1, which finds the folder already being deleted.
//!
//! A deletion does not remove what the project's processes wrote outside its
//! folder (the shared home, `/tmp`, another project's folder), nor what the
//! runtime's services said about it on stderr, which stays in the VM's
//! journal until it rotates. What the platform keeps of the project (its
//! mirror, comments, artifacts) is the platform's to delete.
//!
//! Archiving first disables new execution (`begin_archive`),
//! stops each service's work and the project's cgroup, and only then marks
//! the project archived. A failed step leaves archiving unfinished, never
//! reported as archived. No service forgets any project data.

use std::collections::{BTreeMap, VecDeque};

use nucleus_projects_interface as projects;
use nucleus_protocol::{Error, ErrorCode, ResourcePath};
use serde_json::{Value, json};

use crate::Route;
use crate::serve::{Answers, respond};

/// The services that keep records of a project, in the order a deletion has
/// them forget it: what runs for the project first (the sessions' agents,
/// the swarms' members, the discovery runs), so that nothing writes into
/// what is forgotten after it; the conversation last, since the others write
/// into it. Its folder and processes go after the last.
pub const FORGET_ORDER: [&str; 6] = ["sessions", "swarm", "discovery", "memory", "approvals", "conversation"];
/// The action each of them forgets a project with, on `[project]`.
pub const ACTION_FORGET: &str = "forget";
/// The services whose work an archive stops, in order, none of them
/// forgetting any of the project's records.
pub const STOP_ORDER: [&str; 4] = ["sessions", "swarm", "discovery", "memory"];

/// How long the projects service is given to begin a deletion.
const DELETE_MS: u64 = 60_000;
/// How long one service is given to forget a project: removing a large
/// history, or waiting for a run's processes to be gone.
const FORGET_MS: u64 = 300_000;
/// How long the projects service is given to end a project's processes and
/// remove its folder (its host is given ten minutes for the folder).
const PURGE_MS: u64 = 660_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    BeginArchive,
    Archive,
    Stop(usize),
    Halt,
    Delete,
    /// The place in the services this process forgets with.
    Forget(usize),
    Purge,
}

struct Running {
    step: Step,
    /// The requests awaiting the operation's answer.
    waiting: Vec<(u64, Answers)>,
    archive: bool,
}

/// The deletions under way, and why others stopped.
pub(crate) struct Deletions {
    /// The services of [`FORGET_ORDER`] this process holds `forget` for.
    forgets: Vec<&'static str>,
    running: BTreeMap<String, Running>,
    /// Projects whose next call waits for a place.
    ready: VecDeque<String>,
    /// Why a deletion stopped, until a request is answered with it.
    stopped: BTreeMap<String, String>,
}

impl Deletions {
    pub fn new(forgets: Vec<&'static str>) -> Self {
        Self { forgets, running: BTreeMap::new(), ready: VecDeque::new(), stopped: BTreeMap::new() }
    }

    /// A request to delete `project`, already admitted for its connection.
    pub fn ask(&mut self, project: String, id: u64, answer: Answers) {
        if let Some(why) = self.stopped.remove(&project) {
            let stopped = format!("{project} stays being deleted: {why}. Delete it again to go on");
            drop(answer.send(respond(Some(id), Err(Error::new(ErrorCode::ServiceError, stopped)))));
            return;
        }
        match self.running.get_mut(&project) {
            Some(running) if running.archive => {
                drop(answer.send(respond(
                    Some(id),
                    Err(Error::new(ErrorCode::Unavailable, "the project is being archived")),
                )));
            }
            Some(running) if running.step == Step::Delete => running.waiting.push((id, answer)),
            Some(_) => drop(answer.send(respond(Some(id), Ok(json!({"state": "deleting"}))))),
            None => {
                self.running.insert(
                    project.clone(),
                    Running { step: Step::Delete, waiting: vec![(id, answer)], archive: false },
                );
                self.ready.push_back(project);
            }
        }
    }

    pub fn archive(&mut self, project: String, id: u64, answer: Answers) {
        match self.running.get_mut(&project) {
            Some(running) if running.archive => running.waiting.push((id, answer)),
            Some(_) => drop(answer.send(respond(
                Some(id),
                Err(Error::new(ErrorCode::Unavailable, "the project is being deleted")),
            ))),
            None => {
                self.running.insert(
                    project.clone(),
                    Running { step: Step::BeginArchive, waiting: vec![(id, answer)], archive: true },
                );
                self.ready.push_back(project);
            }
        }
    }

    /// The next call of a workflow, with its deadline, once there is a place
    /// for it: its project and the call, or why it cannot be made.
    pub fn next(&mut self) -> Option<(String, Result<Route, Error>, u64)> {
        let project = self.ready.pop_front()?;
        let step = self.running.get(&project).map(|running| running.step)?;
        let call = match step {
            Step::BeginArchive => projects::project_resource(&project).map(|resource| {
                (projects::PROJECTS_ENDPOINT, projects::ACTION_BEGIN_ARCHIVE, resource, json!({}))
            }),
            Step::Archive => projects::project_resource(&project)
                .map(|resource| (projects::PROJECTS_ENDPOINT, projects::ACTION_ARCHIVE, resource, json!({}))),
            Step::Stop(at) => ResourcePath::new([project.as_str()])
                .map(|resource| (STOP_ORDER[at], "stop", resource, json!({}))),
            Step::Halt => projects::project_resource(&project)
                .map(|resource| (projects::PROJECTS_ENDPOINT, projects::ACTION_STOP, resource, json!({}))),
            Step::Delete => projects::project_resource(&project)
                .map(|resource| (projects::PROJECTS_ENDPOINT, projects::ACTION_DELETE, resource, json!({}))),
            Step::Forget(at) => ResourcePath::new([project.as_str()])
                .map(|resource| (self.forgets[at], ACTION_FORGET, resource, json!({}))),
            Step::Purge => projects::project_resource(&project)
                .map(|resource| (projects::PROJECTS_ENDPOINT, projects::ACTION_PURGE, resource, json!({}))),
        };
        let deadline = match step {
            Step::BeginArchive | Step::Archive | Step::Halt | Step::Stop(_) => FORGET_MS,
            Step::Delete => DELETE_MS,
            Step::Forget(_) => FORGET_MS,
            Step::Purge => PURGE_MS,
        };
        Some((project, call, deadline))
    }

    /// The answer to the call [`Self::next`] gave for `project`.
    pub fn answered(&mut self, project: &str, result: Result<Value, Error>) {
        let Some(running) = self.running.get_mut(project) else { return };
        let next = match (running.step, result) {
            (Step::BeginArchive, Ok(_)) => Some(Step::Stop(0)),
            (Step::Stop(at), Ok(_)) => {
                Some(if at + 1 < STOP_ORDER.len() { Step::Stop(at + 1) } else { Step::Halt })
            }
            (Step::Halt, Ok(_)) => Some(Step::Archive),
            (Step::Archive, Ok(_)) => {
                for (id, answer) in running.waiting.drain(..) {
                    drop(answer.send(respond(Some(id), Ok(json!({"state": "archived"})))));
                }
                None
            }
            (Step::BeginArchive | Step::Archive | Step::Stop(_) | Step::Halt, Err(e)) => {
                for (id, answer) in running.waiting.drain(..) {
                    drop(answer.send(respond(Some(id), Err(e.clone()))));
                }
                None
            }
            (Step::Delete, result) => {
                let result = result.and_then(|value| {
                    serde_json::from_value::<projects::Deletion>(value.clone())
                        .map(|deletion| (deletion.state, value))
                        .map_err(|e| {
                            Error::new(ErrorCode::ServiceError, format!("the projects service's answer: {e}"))
                        })
                });
                for (id, answer) in running.waiting.drain(..) {
                    drop(answer.send(respond(Some(id), result.clone().map(|(_, value)| value))));
                }
                match result {
                    Ok((projects::DeletionState::Deleting, _)) => Some(self.after(None)),
                    // Nothing of it is left, or its error was this request's.
                    _ => None,
                }
            }
            (Step::Forget(at), Ok(_)) => Some(self.after(Some(at))),
            (Step::Forget(at), Err(e)) => {
                let service = self.forgets[at];
                self.stop(
                    project,
                    format!("the {service} service did not forget the project's records: {}", e.message),
                );
                None
            }
            (Step::Purge, Ok(_)) => None,
            (Step::Purge, Err(e)) => {
                self.stop(project, e.message);
                None
            }
        };
        match next {
            Some(step) => {
                if let Some(running) = self.running.get_mut(project) {
                    running.step = step;
                }
                self.ready.push_back(project.to_owned());
            }
            None => drop(self.running.remove(project)),
        }
    }

    /// The step after forgetting at `at` (`None`: after `delete`).
    fn after(&self, at: Option<usize>) -> Step {
        let next = at.map_or(0, |at| at + 1);
        if next < self.forgets.len() { Step::Forget(next) } else { Step::Purge }
    }

    fn stop(&mut self, project: &str, why: String) {
        eprintln!("nucleus-ui: deleting {project} stopped: {why}");
        self.stopped.insert(project.to_owned(), why);
    }
}

pub(crate) fn authority(kernel: &crate::Kernel) -> Result<Vec<&'static str>, String> {
    if kernel.find_handle("projects", projects::ACTION_ARCHIVE).is_some() {
        for service in STOP_ORDER {
            if kernel.find_handle(service, "stop").is_none() {
                return Err(format!("projects archive needs {service} stop"));
            }
        }
        for action in [projects::ACTION_BEGIN_ARCHIVE, projects::ACTION_STOP, projects::ACTION_UNARCHIVE] {
            if kernel.find_handle("projects", action).is_none() {
                return Err(format!("projects archive needs projects {action}"));
            }
        }
    }

    if let Some(other) = kernel.handles().iter().find(|h| {
        h.actions.iter().any(|a| a.as_str() == ACTION_FORGET) && !FORGET_ORDER.contains(&h.endpoint.as_str())
    }) {
        return Err(format!("`forget` on {}, which a deletion does not call", other.endpoint));
    }
    let held = |action: &str| kernel.find_handle(projects::PROJECTS_ENDPOINT, action).is_some();
    if held(projects::ACTION_DELETE) && !held(projects::ACTION_PURGE) {
        return Err("projects delete needs projects purge: a deletion ends with it".into());
    }
    Ok(FORGET_ORDER
        .into_iter()
        .filter(|service| kernel.find_handle(service, ACTION_FORGET).is_some())
        .collect())
}
