//! `forget`: the UI service's, while it deletes a project. Every run of the
//! project is stopped; the call waits until the kernel confirms each run's
//! process and its workers gone (`child_reaped`), so that nothing writes the
//! runs' records any more; then the records, `<out>/<project>/`, are removed,
//! the registry forgets the runs, and the `lean` service drops the statements
//! registered for each run's key. Only then is the call answered. Meanwhile
//! `start` and `resume` in the project are refused.
//!
//! `stop` on `[project]` takes the same path up to the runs' processes being
//! gone, and then answers, keeping the runs, their records and statements.

use super::*;

/// How long the `lean` service is given to drop one run's statements.
const LEAN_FORGET_MS: u64 = 30_000;

/// A project being forgotten.
#[derive(Default)]
pub(super) struct Forgetting {
    /// The `forget` calls to answer.
    dispatches: Vec<DispatchId>,
    /// The `lean forget` calls out; `None` until the runs' processes are gone.
    lean: Option<BTreeSet<RequestId>>,
    /// What failed, which every caller is told.
    failure: Option<Error>,
    /// Stop for archive, preserving runs, statements and journals.
    preserve: bool,
}

fn deleting(project: &str) -> Error {
    Error::new(ErrorCode::ServiceError, format!("the project {project} is being deleted"))
}

impl Service {
    /// Refused while `project` is being forgotten.
    pub(super) fn not_forgetting(&self, project: &str) -> Result<(), Error> {
        if self.forgetting.contains_key(project) { Err(deleting(project)) } else { Ok(()) }
    }

    /// `discovery forget` on `[project]` (module doc).
    pub(super) fn forget(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        self.end_project(k, d, false)
    }

    pub(super) fn stop_project(&mut self, k: &mut impl Kernel, d: &Dispatch) -> Result<(), ChannelError> {
        self.end_project(k, d, true)
    }

    fn end_project(&mut self, k: &mut impl Kernel, d: &Dispatch, preserve: bool) -> Result<(), ChannelError> {
        let project = match project_of(&d.resource) {
            Ok(project) => project,
            Err(e) => return k.reply(d.id, Err(e)),
        };
        if let Err(e) = d.payload.decode::<discovery::ForgetRequest>() {
            return k.reply(d.id, Err(e));
        }
        let fresh = !self.forgetting.contains_key(&project);
        if !fresh && self.forgetting[&project].preserve != preserve {
            return k.reply(d.id, Err(deleting(&project)));
        }
        self.forgetting.entry(project.clone()).or_default().dispatches.push(d.id);
        if fresh {
            if let Some(ending) = self.forgetting.get_mut(&project) {
                ending.preserve = preserve;
            }
            for (run, phase) in self.runs.of_project(&project) {
                match phase {
                    Phase::Admitting { process, .. } => self.abandon(k, process, deleting(&project))?,
                    Phase::Queued { process, .. } | Phase::Running { process } => {
                        if let Some(entry) = self.runs.get_mut(&project, &run) {
                            entry.end(RunState::Stopped, None, Some(process));
                        }
                        self.kill(k, process)?;
                    }
                    Phase::Ended { .. } | Phase::Cancelled { .. } => {}
                }
            }
        }
        self.settle_forgetting(k, &project)
    }

    /// Goes on with forgetting `project` as far as it can: once no process
    /// of its runs is left, its runs and records go and `lean` is asked; once
    /// `lean` has answered, the callers are.
    pub(super) fn settle_forgetting(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
    ) -> Result<(), ChannelError> {
        if !self.forgetting.contains_key(project) || self.procs.values().any(|p| p.project == project) {
            return Ok(());
        }
        if self.forgetting.get(project).is_some_and(|f| f.preserve) {
            if let Some(done) = self.forgetting.remove(project) {
                for dispatch in done.dispatches {
                    k.reply(dispatch, Payload::encode(&serde_json::json!({})))?;
                }
            }
            return Ok(());
        }
        if self.forgetting.get(project).is_some_and(|f| f.lean.is_none()) {
            let names: Vec<String> =
                self.runs.of_project(project).into_iter().map(|(name, _)| name).collect();
            for name in &names {
                self.runs.remove(project, name);
            }
            let mut failure = remove_records(&self.options.out.join(project)).err();
            let mut asked = BTreeSet::new();
            if let (None, Some(handle)) = (&failure, self.authority.forget) {
                for name in &names {
                    let call = ResourcePath::new([wire::key_of(project, name)])
                        .and_then(|key| Ok((key, Payload::encode(&lean::ForgetRequest {})?)));
                    let submitted = match call {
                        Ok((key, payload)) => {
                            self.submit(k, handle, lean::ACTION_FORGET, &key, &payload, LEAN_FORGET_MS)?
                        }
                        Err(e) => Err(e),
                    };
                    match submitted {
                        Ok(request) => {
                            asked.insert(request);
                            self.calls.insert(request, Call::Forget { project: project.to_owned() });
                        }
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
            }
            if let Some(forgetting) = self.forgetting.get_mut(project) {
                forgetting.lean = Some(asked);
                forgetting.failure = forgetting.failure.take().or(failure);
            }
        }
        if self.forgetting.get(project).is_some_and(|f| f.lean.as_ref().is_some_and(|l| !l.is_empty())) {
            return Ok(());
        }
        let Some(done) = self.forgetting.remove(project) else { return Ok(()) };
        let answer = match done.failure {
            Some(e) => Err(e),
            None => Payload::encode(&serde_json::json!({})),
        };
        for dispatch in done.dispatches {
            k.reply(dispatch, answer.clone())?;
        }
        Ok(())
    }

    /// The `lean` service's answer to dropping one run's statements.
    pub(super) fn lean_forgot(
        &mut self,
        k: &mut impl Kernel,
        project: &str,
        request: RequestId,
        result: Result<Payload, Error>,
    ) -> Result<(), ChannelError> {
        if let Some(forgetting) = self.forgetting.get_mut(project) {
            if let Some(lean) = &mut forgetting.lean {
                lean.remove(&request);
            }
            if let Err(e) = result {
                forgetting.failure.get_or_insert(e);
            }
        }
        self.settle_forgetting(k, project)
    }

    /// The caller of a `forget` gave up: it is answered now, and the
    /// forgetting goes on for any other caller (the next `forget` finds what
    /// is left).
    pub(super) fn forget_cancelled(
        &mut self,
        k: &mut impl Kernel,
        dispatch: DispatchId,
    ) -> Result<(), ChannelError> {
        for forgetting in self.forgetting.values_mut() {
            if let Some(at) = forgetting.dispatches.iter().position(|d| *d == dispatch) {
                forgetting.dispatches.remove(at);
                k.reply(dispatch, Err(cancelled("the call was cancelled")))?;
            }
        }
        Ok(())
    }
}

/// Removes a project's records directory with everything in it, and a link
/// in its place as a link.
fn remove_records(dir: &Path) -> Result<(), Error> {
    let removed = match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dir),
        Ok(_) => std::fs::remove_file(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };
    removed.map_err(|e| Error::new(ErrorCode::ServiceError, format!("{}: {e}", dir.display())))
}
