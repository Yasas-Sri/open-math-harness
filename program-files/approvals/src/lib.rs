//! Approvals guard: provider of `nucleus-approvals-interface`.
//!
//! The guard holds the capabilities for protected actions; requesters do not.
//! That is the whole enforcement: a requester that was never given `fs remove`
//! cannot remove a file, whatever it says to anyone. It can only ask the guard,
//! and the guard acts when its policy allows it or a person approves it.
//!
//! It protects only the calls it makes. A requester that also holds `shell
//! exec` over the same files can delete them with `rm`, which no rule here
//! sees, and an irreversible action granted to an agent directly bypasses the
//! guard: nothing checks that a composition granted it to the guard instead.
//!
//! ## Policy
//!
//! A JSON file supplied by the operator: a `version` and an ordered list of
//! rules, first match wins, no match means `deny`.
//!
//! ```json
//! {"version": 3, "rules": [
//!   {"endpoint": "fs", "action": "remove", "resource": ["work", "scratch"], "effect": "allow"},
//!   {"endpoint": "fs", "action": "remove", "resource": ["work"], "effect": "ask"}
//! ]}
//! ```
//!
//! The policy is read once at start. Its version is recorded in every pending
//! request and covered by the request's digest, so a decision can never apply
//! to a request evaluated under another policy: pending requests do not survive
//! a restart, which is the only way the policy changes.
//!
//! ## What an approval is bound to
//!
//! The requester (asserted by the kernel on the `request` dispatch), the target
//! endpoint, action and resource, the exact payload, and the policy version,
//! all through the digest the decider must quote. It is used once: approving
//! makes the guard perform the recorded call, and the pending entry is gone
//! before the call is made. It dies with the requester's call (deadline,
//! cancellation, or termination). The requester cannot decide its own request.
//!
//! ## Record
//!
//! Every request, policy verdict, decision (with who decided), the intent to
//! dispatch (synced **before** the protected call is made), and the outcome are
//! appended to a JSON-lines log with a format version. A `dispatching` line with
//! no `outcome` after it means the outcome is unknown, not that nothing happened.
//! It is an audit record, not crash recovery: at start the guard reads it only
//! for its format version and next sequence number ([`Journal::open`]), and
//! nothing reconciles an unknown outcome or retries a call.
//!
//! ## Forgetting a deleted project
//!
//! `forget` on `[project]`: the requests waiting about the project's files
//! (a target within `["projects", project]`) are refused, and the journal is
//! rewritten, to a new file renamed over it, without every line about them:
//! their requests, verdicts, decisions, expiries, intents and outcomes. A
//! decision, expiry or outcome names its request by id; ids start again with
//! each run of the guard, and a request is decided only in the run that
//! recorded it, so each such line belongs to the latest request of its id
//! before it. An outcome without an id follows the `dispatching` line just
//! before it: the guard makes one call at a time.
//!
//! ## Privileges and compromise
//!
//! The guard holds real authority over every protected action in its policy,
//! so it is part of the trusted computing base for those actions: a compromised
//! guard can perform them without asking. In the VM it has no network, and
//! of the filesystem only its policy, read-only, and its log directory. It
//! serves one call at a time; a protected call in progress delays other
//! requests and decisions by as long as that call takes.
//!
//! An approval proves that a process holding `decide` said yes, not that a
//! person read anything: in the VM the UI service holds `decide` on `[]` for
//! every window the platform labels the owner or a member. A compromised front
//! end can approve whatever pending request its `decide` covers, but cannot
//! change one (a decision carries only the request's id, its digest and yes or
//! no) or make the call itself. A compromised requester can ask with a
//! misleading reason, which a front end shows as the requester's words beside
//! the exact call.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use nucleus_approvals_interface::{
    ACTION_DECIDE, ACTION_FORGET, ACTION_LIST, ACTION_REQUEST, Decision, ForgetRequest, ListRequest,
    ListResponse, Pending, Request, digest, interface,
};
use nucleus_protocol::{
    ACTION_PROVIDE, ActionName, DispatchId, EndpointName, Error, ErrorCode, HandleInfo, KERNEL_ENDPOINT,
    Payload, ProcessId, ResourcePath,
};
use nucleus_sdk::{Connection, Event};
use serde::Deserialize;
use serde_json::{Value, json};

/// Requests that may wait for a decision at once.
pub const MAX_PENDING: usize = 32;
/// Version written into every record of the log.
pub const LOG_FORMAT_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub policy: PathBuf,
    pub log: PathBuf,
    pub endpoint: EndpointName,
}

impl Options {
    /// Parses `--policy <file> --log <file> [--endpoint <name>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut policy, mut log) = (None, None);
        let mut endpoint = nucleus_approvals_interface::ENDPOINT.to_owned();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--policy" => policy = Some(PathBuf::from(value)),
                "--log" => log = Some(PathBuf::from(value)),
                "--endpoint" => endpoint = value,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            policy: policy.ok_or("--policy is required")?,
            log: log.ok_or("--log is required")?,
            endpoint: EndpointName::new(endpoint).map_err(|e| e.to_string())?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub endpoint: EndpointName,
    pub action: ActionName,
    /// Matches this resource and everything beneath it.
    pub resource: ResourcePath,
    pub effect: Effect,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u64,
    pub rules: Vec<Rule>,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("policy {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("policy {}: {e}", path.display()))
    }

    /// First matching rule wins; nothing matching means deny. A rule sees the
    /// endpoint, the action and the resource, never the payload: the payload is
    /// what a person is shown and what the digest binds.
    pub fn verdict(&self, endpoint: &EndpointName, action: &ActionName, resource: &ResourcePath) -> Effect {
        self.rules
            .iter()
            .find(|r| &r.endpoint == endpoint && &r.action == action && resource.is_within(&r.resource))
            .map_or(Effect::Deny, |r| r.effect)
    }
}

/// One protected call, exactly as it will be made.
#[derive(Clone, Debug, PartialEq)]
pub struct Protected {
    pub requester: ProcessId,
    pub endpoint: EndpointName,
    pub action: ActionName,
    pub resource: ResourcePath,
    pub payload: Payload,
}

impl Protected {
    /// Whether its target, `[endpoint, ...resource]`, is within `scope`.
    fn within(&self, scope: &ResourcePath) -> bool {
        let target = std::iter::once(self.endpoint.as_str())
            .chain(self.resource.segments().iter().map(String::as_str));
        ResourcePath::new(target).is_ok_and(|target| target.is_within(scope))
    }
}

struct Waiting {
    dispatch: DispatchId,
    call: Protected,
    reason: String,
    digest: String,
    since: Instant,
}

/// What the guard's owner must do next.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Answer a dispatch.
    Reply(DispatchId, Result<Payload, Error>),
    /// Make the protected call, then answer `reply_to` with its result.
    Forward { reply_to: DispatchId, approval: Option<u64>, call: Protected },
}

/// Append-only, synced record.
pub struct Journal {
    path: PathBuf,
    file: File,
    seq: u64,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, String> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut seq = 0;
        for line in existing.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
            if record["v"].as_u64().is_some_and(|v| v > LOG_FORMAT_VERSION) {
                return Err(
                    "the approvals log was written by a newer format version; refusing to touch it".into()
                );
            }
            seq = seq.max(record["seq"].as_u64().map_or(0, |s| s + 1));
        }
        let file =
            OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("open log: {e}"))?;
        Ok(Self { path: path.to_owned(), file, seq })
    }

    /// Rewrites the journal without the lines about `project` (module doc,
    /// "Forgetting a deleted project"), through a new file renamed over it.
    /// Every other line stays as it was, its `seq` with it.
    fn forget(&mut self, project: &str) -> Result<(), Error> {
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            Error::new(ErrorCode::ServiceError, format!("approvals log: {what}: {e}"))
        };
        let about = |call: &Value| {
            let resource = call["resource"].as_array().map(Vec::as_slice).unwrap_or_default();
            matches!(resource, [root, name, ..] if root == "projects" && name == project)
        };
        let text = std::fs::read_to_string(&self.path).map_err(|e| failed("read", &e))?;
        let (mut ours, mut unapproved) = (HashMap::new(), false);
        let mut kept = String::with_capacity(text.len());
        for line in text.lines() {
            let record = serde_json::from_str::<Value>(line).unwrap_or_default();
            let id = record["id"].as_u64().or(record["approval"].as_u64());
            let mine = match record["event"].as_str() {
                Some("requested") => {
                    let mine = about(&record["call"]);
                    ours.insert(id, mine);
                    mine
                }
                Some("dispatching") => {
                    let mine = about(&record["call"]);
                    if id.is_none() {
                        unapproved = mine;
                    }
                    mine
                }
                Some("outcome") if id.is_none() => unapproved,
                Some("decided" | "expired" | "outcome") => ours.get(&id).copied().unwrap_or(false),
                _ => about(&record["call"]),
            };
            if !mine {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        let fresh = self.path.with_extension("jsonl.next");
        let written = File::create(&fresh)
            .and_then(|mut file| file.write_all(kept.as_bytes()).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&fresh, &self.path));
        written.map_err(|e| failed("rewrite", &e))?;
        if let Some(dir) = self.path.parent() {
            File::open(dir).and_then(|dir| dir.sync_all()).map_err(|e| failed("sync", &e))?;
        }
        self.file = OpenOptions::new().append(true).open(&self.path).map_err(|e| failed("open", &e))?;
        Ok(())
    }

    pub fn append(&mut self, mut record: Value) -> Result<(), Error> {
        let at_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        record["v"] = json!(LOG_FORMAT_VERSION);
        record["seq"] = json!(self.seq);
        record["at_ms"] = json!(u64::try_from(at_ms).unwrap_or(u64::MAX));
        let mut line = record.to_string();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| Error::new(ErrorCode::ServiceError, format!("approvals log: {e}")))?;
        self.seq += 1;
        Ok(())
    }
}

fn describe(call: &Protected) -> Value {
    json!({"requester": call.requester, "endpoint": call.endpoint, "action": call.action, "resource": call.resource})
}

/// Policy, pending requests, and the rules for deciding them. No kernel I/O.
pub struct Guard {
    policy: Policy,
    /// What this process can actually do, from its own handles.
    held: Vec<HandleInfo>,
    pending: BTreeMap<u64, Waiting>,
    next_id: u64,
    journal: Journal,
}

fn refuse(code: ErrorCode, why: &str) -> Error {
    Error::new(code, why.to_owned())
}

impl Guard {
    pub fn new(policy: Policy, held: Vec<HandleInfo>, journal: Journal) -> Self {
        Self { policy, held, pending: BTreeMap::new(), next_id: 1, journal }
    }

    fn can_perform(&self, call: &Protected) -> bool {
        self.held.iter().any(|h| {
            h.endpoint == call.endpoint
                && h.actions.contains(&call.action)
                && call.resource.is_within(&h.resource)
        })
    }

    /// Handles one dispatch to this service.
    pub fn handle(
        &mut self,
        dispatch: DispatchId,
        caller: ProcessId,
        action: &str,
        resource: &ResourcePath,
        payload: &Payload,
    ) -> Vec<Step> {
        let outcome = match action {
            ACTION_REQUEST => self.request(dispatch, caller, resource, payload),
            ACTION_LIST => self.list(dispatch, resource, payload),
            ACTION_DECIDE => self.decide(dispatch, caller, resource, payload),
            ACTION_FORGET => self.forget(dispatch, resource, payload),
            _ => Err(refuse(ErrorCode::InvalidInput, "unknown action")),
        };
        outcome.unwrap_or_else(|error| vec![Step::Reply(dispatch, Err(error))])
    }

    fn request(
        &mut self,
        dispatch: DispatchId,
        caller: ProcessId,
        resource: &ResourcePath,
        payload: &Payload,
    ) -> Result<Vec<Step>, Error> {
        let request: Request = payload.decode()?;
        let [endpoint, target @ ..] = resource.segments() else {
            return Err(refuse(
                ErrorCode::InvalidInput,
                "the resource path must start with the target endpoint",
            ));
        };
        let endpoint = EndpointName::new(endpoint.clone())?;
        if endpoint.as_str() == KERNEL_ENDPOINT || endpoint.as_str() == nucleus_approvals_interface::ENDPOINT
        {
            return Err(refuse(
                ErrorCode::PermissionDenied,
                "that endpoint cannot be reached through the guard",
            ));
        }
        let call = Protected {
            requester: caller,
            endpoint,
            action: request.action,
            resource: ResourcePath::new(target.iter().cloned())?,
            payload: request.payload,
        };
        if request.reason.len() > 2_000 {
            return Err(refuse(ErrorCode::InvalidInput, "reason is too long"));
        }
        let verdict = self.policy.verdict(&call.endpoint, &call.action, &call.resource);
        // Asking a person to approve something the guard cannot do would waste their attention.
        if verdict != Effect::Deny && !self.can_perform(&call) {
            self.journal.append(
                json!({"event": "refused", "why": "guard lacks the capability", "call": describe(&call)}),
            )?;
            return Err(refuse(
                ErrorCode::PermissionDenied,
                "the guard holds no capability for that action on that resource",
            ));
        }
        match verdict {
            Effect::Deny => {
                self.journal.append(json!({"event": "denied_by_policy", "policy_version": self.policy.version, "call": describe(&call)}))?;
                Err(refuse(ErrorCode::PermissionDenied, "the approvals policy does not permit this"))
            }
            Effect::Allow => {
                self.journal.append(json!({"event": "allowed_by_policy", "policy_version": self.policy.version, "call": describe(&call)}))?;
                Ok(vec![Step::Forward { reply_to: dispatch, approval: None, call }])
            }
            Effect::Ask => self.wait_for_a_person(dispatch, call, request.reason),
        }
    }

    /// Puts a call in front of a person and returns nothing to the caller,
    /// which waits.
    fn wait_for_a_person(
        &mut self,
        dispatch: DispatchId,
        call: Protected,
        reason: String,
    ) -> Result<Vec<Step>, Error> {
        if self.pending.len() >= MAX_PENDING {
            return Err(refuse(
                ErrorCode::LimitExceeded,
                &format!(
                    "at most {MAX_PENDING} requests wait for a person's decision in this VM at once, and \
                     {MAX_PENDING} do: ask again once some are decided"
                ),
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        let bound = digest(
            call.requester,
            &call.endpoint,
            &call.action,
            &call.resource,
            &call.payload,
            self.policy.version,
        );
        self.journal.append(json!({
            "event": "requested", "id": id, "digest": bound, "policy_version": self.policy.version,
            "call": describe(&call), "reason": reason,
        }))?;
        self.pending.insert(id, Waiting { dispatch, call, reason, digest: bound, since: Instant::now() });
        Ok(Vec::new())
    }

    fn list(
        &mut self,
        dispatch: DispatchId,
        resource: &ResourcePath,
        payload: &Payload,
    ) -> Result<Vec<Step>, Error> {
        let request: ListRequest = payload.decode()?;
        let limit = request.validate()?;
        let mut shown = self
            .pending
            .range(request.after.map_or(0, |after| after.saturating_add(1))..)
            .filter(|(_, w)| w.call.within(resource));
        let pending: Vec<Pending> = shown
            .by_ref()
            .take(limit)
            .map(|(id, w)| Pending {
                id: *id,
                requester: w.call.requester,
                endpoint: w.call.endpoint.clone(),
                action: w.call.action.clone(),
                resource: w.call.resource.clone(),
                payload: w.call.payload.clone(),
                reason: w.reason.clone(),
                policy_version: self.policy.version,
                digest: w.digest.clone(),
                waiting_ms: u64::try_from(w.since.elapsed().as_millis()).unwrap_or(u64::MAX),
            })
            .collect();
        let next = shown.next().and(pending.last().map(|p| p.id));
        let response = ListResponse { pending, next };
        Ok(vec![Step::Reply(dispatch, Payload::encode(&response))])
    }

    fn decide(
        &mut self,
        dispatch: DispatchId,
        caller: ProcessId,
        resource: &ResourcePath,
        payload: &Payload,
    ) -> Result<Vec<Step>, Error> {
        let decision: Decision = payload.decode()?;
        // One outside the resource the decider names is not there for it.
        let waiting =
            self.pending.get(&decision.id).filter(|w| w.call.within(resource)).ok_or_else(|| {
                refuse(
                    ErrorCode::InvalidInput,
                    "no such pending request: it expired, was decided, or never existed",
                )
            })?;
        if waiting.call.requester == caller {
            return Err(refuse(ErrorCode::PermissionDenied, "a requester cannot decide its own request"));
        }
        if waiting.digest != decision.digest {
            return Err(refuse(
                ErrorCode::InvalidInput,
                "the decision does not match the request as it stands",
            ));
        }
        // Single use: the entry is gone before anything is dispatched.
        let Some(waiting) = self.pending.remove(&decision.id) else {
            return Err(refuse(ErrorCode::ServiceError, "pending request vanished"));
        };
        self.journal.append(json!({
            "event": "decided", "id": decision.id, "digest": waiting.digest, "allow": decision.allow, "decided_by": caller,
        }))?;
        let done = Step::Reply(dispatch, Payload::encode(&json!({})));
        Ok(if decision.allow {
            vec![
                done,
                Step::Forward { reply_to: waiting.dispatch, approval: Some(decision.id), call: waiting.call },
            ]
        } else {
            vec![
                done,
                Step::Reply(
                    waiting.dispatch,
                    Err(refuse(ErrorCode::PermissionDenied, "declined by the person asked")),
                ),
            ]
        })
    }

    /// `forget` (module doc): what waits about `[project]`'s files is refused,
    /// and the journal keeps nothing about them.
    fn forget(
        &mut self,
        dispatch: DispatchId,
        resource: &ResourcePath,
        payload: &Payload,
    ) -> Result<Vec<Step>, Error> {
        payload.decode::<ForgetRequest>()?;
        let project = match resource.segments() {
            [project] => project.clone(),
            _ => {
                return Err(refuse(
                    ErrorCode::InvalidInput,
                    "forget is a project's: its resource is [project]",
                ));
            }
        };
        let files = ResourcePath::new(["projects", project.as_str()])?;
        let ours: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, w)| w.call.resource.is_within(&files))
            .map(|(id, _)| *id)
            .collect();
        let mut steps = Vec::new();
        for id in ours {
            if let Some(waiting) = self.pending.remove(&id) {
                let gone = format!("the project {project} was deleted");
                steps.push(Step::Reply(waiting.dispatch, Err(Error::new(ErrorCode::PermissionDenied, gone))));
            }
        }
        self.journal.forget(&project)?;
        steps.push(Step::Reply(dispatch, Payload::encode(&json!({}))));
        Ok(steps)
    }

    /// The kernel gave up on a dispatch: a request waiting on it can no longer be approved.
    /// An approved request is no longer pending, so its call is made whatever its
    /// requester does; a requester that stopped waiting never learns the outcome,
    /// which the log has.
    pub fn cancel(&mut self, dispatch: DispatchId) {
        let expired: Vec<u64> =
            self.pending.iter().filter(|(_, w)| w.dispatch == dispatch).map(|(id, _)| *id).collect();
        for id in expired {
            self.pending.remove(&id);
            let _ = self
                .journal
                .append(json!({"event": "expired", "id": id, "why": "the requester stopped waiting"}));
        }
    }

    /// Intent, synced before the protected call is made.
    pub fn record_dispatching(&mut self, approval: Option<u64>, call: &Protected) -> Result<(), Error> {
        self.journal.append(json!({"event": "dispatching", "approval": approval, "call": describe(call)}))
    }

    pub fn record_outcome(&mut self, approval: Option<u64>, result: &Result<Payload, Error>) {
        let outcome = match result {
            Ok(_) => json!("ok"),
            Err(e) => json!({"error": e.code}),
        };
        let _ = self.journal.append(json!({"event": "outcome", "approval": approval, "outcome": outcome}));
    }
}

/// Provides the endpoint and serves dispatches until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let policy = Policy::load(&options.policy)?;
    let journal = Journal::open(&options.log)?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    let mut guard = Guard::new(policy, kernel.handles().to_vec(), journal);
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    loop {
        match kernel.next_event().map_err(|e| e.to_string())? {
            Event::Dispatch(d) => {
                for step in guard.handle(d.id, d.caller, d.action.as_str(), &d.resource, &d.payload) {
                    match step {
                        Step::Reply(to, result) => kernel.reply(to, result).map_err(|e| e.to_string())?,
                        Step::Forward { reply_to, approval, call } => {
                            let result = match guard.record_dispatching(approval, &call) {
                                // Not on record, not done.
                                Err(e) => Err(e),
                                Ok(()) => {
                                    match kernel.find_handle(call.endpoint.as_str(), call.action.as_str()) {
                                        None => Err(refuse(
                                            ErrorCode::PermissionDenied,
                                            "the guard holds no capability for that",
                                        )),
                                        Some(handle) => kernel
                                            .call(
                                                handle,
                                                call.action.as_str(),
                                                &call.resource,
                                                &call.payload,
                                                None,
                                            )
                                            .map_err(|e| e.to_string())?,
                                    }
                                }
                            };
                            guard.record_outcome(approval, &result);
                            kernel.reply(reply_to, result).map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
            Event::CancelDispatch(id) => guard.cancel(id),
            Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::collections::BTreeSet;

    use nucleus_protocol::HandleId;

    use super::*;

    const AGENT: ProcessId = ProcessId(7);
    const WINDOW: ProcessId = ProcessId(9);

    fn path(segments: &[&str]) -> ResourcePath {
        ResourcePath::new(segments.iter().copied()).unwrap()
    }

    fn guard(dir: &Path) -> Guard {
        let policy: Policy = serde_json::from_value(json!({"version": 3, "rules": [
            {"endpoint": "fs", "action": "remove", "resource": ["work", "scratch"], "effect": "allow"},
            {"endpoint": "fs", "action": "remove", "resource": ["work", "keep"], "effect": "deny"},
            {"endpoint": "fs", "action": "remove", "resource": ["work"], "effect": "ask"},
            {"endpoint": "fs", "action": "remove", "resource": ["elsewhere"], "effect": "ask"},
        ]}))
        .unwrap();
        let held = vec![HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new("fs").unwrap(),
            actions: BTreeSet::from([ActionName::new("remove").unwrap()]),
            resource: path(&["work"]),
            delegable: false,
            expires_in_ms: None,
            max_calls: None,
        }];
        Guard::new(policy, held, Journal::open(&dir.join("approvals.jsonl")).unwrap())
    }

    fn ask(guard: &mut Guard, dispatch: u64, caller: ProcessId, resource: &[&str]) -> Vec<Step> {
        let request = json!({"action": "remove", "payload": {}, "reason": "tidying up"});
        guard.handle(
            DispatchId(dispatch),
            caller,
            ACTION_REQUEST,
            &path(resource),
            &Payload::encode(&request).unwrap(),
        )
    }

    fn pending(guard: &mut Guard) -> Vec<Pending> {
        let steps = guard.handle(
            DispatchId(900),
            WINDOW,
            ACTION_LIST,
            &ResourcePath::root(),
            &Payload::encode(&json!({})).unwrap(),
        );
        let Step::Reply(_, Ok(payload)) = &steps[0] else { panic!("{steps:?}") };
        payload.decode::<ListResponse>().unwrap().pending
    }

    fn decide(guard: &mut Guard, by: ProcessId, id: u64, digest: &str, allow: bool) -> Vec<Step> {
        let decision = Payload::encode(&Decision { id, digest: digest.to_owned(), allow }).unwrap();
        guard.handle(DispatchId(901), by, ACTION_DECIDE, &ResourcePath::root(), &decision)
    }

    fn error_of(steps: &[Step]) -> ErrorCode {
        match &steps[0] {
            Step::Reply(_, Err(e)) => e.code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn events(dir: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(dir.join("approvals.jsonl")).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["event"].as_str().unwrap().to_owned())
            .collect()
    }

    /// `forget` refuses what waits about the project's files, rewrites the
    /// journal without a line about them (a decided request's decision,
    /// intent and outcome, a policy's verdict included), and keeps every
    /// other line, its `seq` with it; the journal goes on from there.
    #[test]
    fn a_forgotten_project_leaves_nothing_in_the_journal_and_nothing_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let policy: Policy = serde_json::from_value(json!({"version": 3, "rules": [
            {"endpoint": "fs", "action": "remove", "resource": ["projects", "p", "repo", "tmp"], "effect": "allow"},
            {"endpoint": "fs", "action": "remove", "resource": ["projects"], "effect": "ask"},
        ]}))
        .unwrap();
        let held = vec![HandleInfo {
            handle: HandleId(1),
            endpoint: EndpointName::new("fs").unwrap(),
            actions: BTreeSet::from([ActionName::new("remove").unwrap()]),
            resource: path(&["projects"]),
            delegable: false,
            expires_in_ms: None,
            max_calls: None,
        }];
        let mut guard = Guard::new(policy, held, Journal::open(&dir.path().join("approvals.jsonl")).unwrap());
        // Decided and done, about p; allowed by policy and done, about p; one of
        // q's; one of p's still waiting.
        assert!(ask(&mut guard, 1, AGENT, &["fs", "projects", "p", "repo", "a"]).is_empty());
        let first = pending(&mut guard)[0].clone();
        let Step::Forward { approval, call, .. } =
            &decide(&mut guard, WINDOW, first.id, &first.digest, true)[1]
        else {
            panic!("approved")
        };
        guard.record_dispatching(*approval, call).unwrap();
        guard.record_outcome(*approval, &Payload::encode(&json!({})));
        let Step::Forward { approval, call, .. } =
            &ask(&mut guard, 2, AGENT, &["fs", "projects", "p", "repo", "tmp", "x"])[0]
        else {
            panic!("allowed")
        };
        guard.record_dispatching(*approval, call).unwrap();
        guard.record_outcome(*approval, &Payload::encode(&json!({})));
        assert!(ask(&mut guard, 3, AGENT, &["fs", "projects", "q", "repo", "b"]).is_empty());
        assert!(ask(&mut guard, 4, AGENT, &["fs", "projects", "p", "repo", "c"]).is_empty());
        let before = std::fs::read_to_string(dir.path().join("approvals.jsonl")).unwrap();

        let forget = Payload::encode(&json!({})).unwrap();
        let steps = guard.handle(DispatchId(5), WINDOW, ACTION_FORGET, &path(&["p"]), &forget);
        assert!(
            matches!(&steps[0], Step::Reply(DispatchId(4), Err(e)) if e.code == ErrorCode::PermissionDenied),
            "{steps:?}"
        );
        assert!(matches!(&steps[1], Step::Reply(DispatchId(5), Ok(_))), "{steps:?}");
        assert_eq!(pending(&mut guard).iter().map(|p| p.id).collect::<Vec<_>>(), [2], "q's waits");
        let after = std::fs::read_to_string(dir.path().join("approvals.jsonl")).unwrap();
        assert!(!after.contains("\"p\""), "nothing about p: {after}");
        let kept: Vec<&str> = before.lines().filter(|line| line.contains("\"q\"")).collect();
        assert_eq!(after.lines().collect::<Vec<_>>(), kept, "q's line, as it was");
        // The journal goes on after the rewrite.
        assert!(ask(&mut guard, 6, AGENT, &["fs", "projects", "q", "repo", "d"]).is_empty());
        assert_eq!(events(dir.path()), ["requested", "requested"]);
        for bad in [path(&[]), path(&["p", "s"])] {
            assert_eq!(
                error_of(&guard.handle(DispatchId(7), WINDOW, ACTION_FORGET, &bad, &forget)),
                ErrorCode::InvalidInput
            );
        }
    }

    #[test]
    fn policy_allows_denies_or_asks_and_no_match_means_deny() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        let allowed = ask(&mut guard, 1, AGENT, &["fs", "work", "scratch", "tmp.txt"]);
        assert!(
            matches!(&allowed[0], Step::Forward { approval: None, call, .. } if call.resource == path(&["work", "scratch", "tmp.txt"]))
        );
        assert_eq!(
            error_of(&ask(&mut guard, 2, AGENT, &["fs", "work", "keep", "ledger.txt"])),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            error_of(&ask(&mut guard, 3, AGENT, &["fs", "other", "x"])),
            ErrorCode::PermissionDenied,
            "no rule"
        );
        assert_eq!(
            error_of(&ask(&mut guard, 4, AGENT, &["shell", "work"])),
            ErrorCode::PermissionDenied,
            "no rule"
        );
        assert!(
            ask(&mut guard, 5, AGENT, &["fs", "work", "old.txt"]).is_empty(),
            "parked: a person is asked"
        );
        assert_eq!(
            error_of(&ask(&mut guard, 6, AGENT, &["fs", "elsewhere", "x"])),
            ErrorCode::PermissionDenied,
            "policy would ask, but the guard cannot do it, so nobody is bothered"
        );
        for reserved in ["kernel", "approvals"] {
            assert_eq!(error_of(&ask(&mut guard, 7, AGENT, &[reserved, "x"])), ErrorCode::PermissionDenied);
        }
        assert_eq!(pending(&mut guard).len(), 1);
    }

    #[test]
    fn approval_runs_exactly_the_recorded_call_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        ask(&mut guard, 11, AGENT, &["fs", "work", "old.txt"]);
        let shown = pending(&mut guard).remove(0);
        assert_eq!((shown.requester, shown.policy_version, shown.reason.as_str()), (AGENT, 3, "tidying up"));

        let steps = decide(&mut guard, WINDOW, shown.id, &shown.digest, true);
        assert!(matches!(&steps[0], Step::Reply(DispatchId(901), Ok(_))));
        let Step::Forward { reply_to, approval, call } = &steps[1] else { panic!("{steps:?}") };
        assert_eq!(
            (*reply_to, *approval),
            (DispatchId(11), Some(shown.id)),
            "the result goes to the requester's own call"
        );
        assert_eq!(
            (call.requester, call.action.as_str(), &call.resource),
            (AGENT, "remove", &path(&["work", "old.txt"]))
        );

        assert_eq!(
            error_of(&decide(&mut guard, WINDOW, shown.id, &shown.digest, true)),
            ErrorCode::InvalidInput,
            "single use"
        );
        assert!(pending(&mut guard).is_empty());
        assert_eq!(events(dir.path()), ["requested", "decided"]);
    }

    #[test]
    fn a_decision_must_match_the_request_and_come_from_someone_else() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        ask(&mut guard, 21, AGENT, &["fs", "work", "a.txt"]);
        ask(&mut guard, 22, AGENT, &["fs", "work", "b.txt"]);
        let shown = pending(&mut guard);
        assert_ne!(shown[0].digest, shown[1].digest);

        assert_eq!(
            error_of(&decide(&mut guard, WINDOW, shown[0].id, &shown[1].digest, true)),
            ErrorCode::InvalidInput,
            "approving A with what was shown for B"
        );
        assert_eq!(
            error_of(&decide(&mut guard, WINDOW, shown[0].id, "0000000000000000", true)),
            ErrorCode::InvalidInput
        );
        assert_eq!(
            error_of(&decide(&mut guard, AGENT, shown[0].id, &shown[0].digest, true)),
            ErrorCode::PermissionDenied,
            "self-approval"
        );
        assert_eq!(
            error_of(&decide(&mut guard, WINDOW, 999, &shown[0].digest, true)),
            ErrorCode::InvalidInput
        );
        assert_eq!(pending(&mut guard).len(), 2, "failed decisions consume nothing");

        let declined = decide(&mut guard, WINDOW, shown[0].id, &shown[0].digest, false);
        assert!(
            matches!(&declined[1], Step::Reply(DispatchId(21), Err(e)) if e.code == ErrorCode::PermissionDenied)
        );
        assert_eq!(pending(&mut guard).len(), 1);
    }

    /// A front end that names a target prefix sees and decides the requests
    /// within it only; another is not there for it.
    #[test]
    fn a_decider_scoped_to_a_target_reaches_only_the_requests_within_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        ask(&mut guard, 41, AGENT, &["fs", "work", "a", "x.txt"]);
        ask(&mut guard, 42, AGENT, &["fs", "work", "b", "y.txt"]);
        let all = pending(&mut guard);
        let scope = path(&["fs", "work", "a"]);
        let list =
            guard.handle(DispatchId(902), WINDOW, ACTION_LIST, &scope, &Payload::encode(&json!({})).unwrap());
        let Step::Reply(_, Ok(listed)) = &list[0] else { panic!("{list:?}") };
        let listed = listed.decode::<ListResponse>().unwrap().pending;
        assert_eq!(listed.iter().map(|p| p.id).collect::<Vec<_>>(), [all[0].id]);
        let decide_in = |guard: &mut Guard, shown: &Pending| {
            let decision = Decision { id: shown.id, digest: shown.digest.clone(), allow: true };
            guard.handle(DispatchId(903), WINDOW, ACTION_DECIDE, &scope, &Payload::encode(&decision).unwrap())
        };
        assert_eq!(error_of(&decide_in(&mut guard, &all[1])), ErrorCode::InvalidInput, "outside: not there");
        assert!(matches!(&decide_in(&mut guard, &all[0])[1], Step::Forward { reply_to: DispatchId(41), .. }));
        assert_eq!(pending(&mut guard).len(), 1);
    }

    #[test]
    fn a_request_dies_with_the_requesters_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        ask(&mut guard, 31, AGENT, &["fs", "work", "a.txt"]);
        let shown = pending(&mut guard).remove(0);
        guard.cancel(DispatchId(31));
        assert!(pending(&mut guard).is_empty());
        assert_eq!(
            error_of(&decide(&mut guard, WINDOW, shown.id, &shown.digest, true)),
            ErrorCode::InvalidInput,
            "a late approval approves nothing"
        );
        assert_eq!(events(dir.path()), ["requested", "expired"]);
    }

    #[test]
    fn waiting_requests_are_bounded_and_the_log_refuses_a_newer_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut guard = guard(dir.path());
        for n in 0..MAX_PENDING {
            assert!(ask(&mut guard, 100 + n as u64, AGENT, &["fs", "work", "f.txt"]).is_empty());
        }
        assert_eq!(
            error_of(&ask(&mut guard, 999, AGENT, &["fs", "work", "f.txt"])),
            ErrorCode::LimitExceeded
        );
        // The list is paged, oldest first, every one once.
        let (mut seen, mut after) = (Vec::new(), None);
        loop {
            let request = Payload::encode(&ListRequest { after, limit: Some(10) }).unwrap();
            let steps = guard.handle(DispatchId(900), WINDOW, ACTION_LIST, &ResourcePath::root(), &request);
            let Step::Reply(_, Ok(payload)) = &steps[0] else { panic!("{steps:?}") };
            let page = payload.decode::<ListResponse>().unwrap();
            seen.extend(page.pending.iter().map(|p| p.id));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, (1..=MAX_PENDING as u64).collect::<Vec<_>>());
        let too_big = Payload::encode(&ListRequest { after: None, limit: Some(201) }).unwrap();
        let steps = guard.handle(DispatchId(902), WINDOW, ACTION_LIST, &ResourcePath::root(), &too_big);
        assert_eq!(error_of(&steps), ErrorCode::InvalidInput);

        let future = dir.path().join("future.jsonl");
        std::fs::write(&future, "{\"v\":2,\"seq\":0,\"event\":\"requested\"}\n").unwrap();
        assert!(Journal::open(&future).is_err());
    }
}
