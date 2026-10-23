#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;

use nucleus_protocol::Payload;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;
use crate::Request;

/// What the fake kernel does with a call: answer it with this, or never.
type Script = Box<dyn FnMut(&Route) -> Option<Result<Value, Error>>>;

/// A kernel that records every call it is given and answers as its script
/// says; once nothing is left to answer it asks the service to end, so that
/// [`serve_jobs`] returns with everything it did.
struct Fake {
    next: u64,
    calls: Vec<(String, String, Vec<String>, Value)>,
    ready: VecDeque<Answer>,
    script: Script,
    done: bool,
}

impl Fake {
    fn new(script: impl FnMut(&Route) -> Option<Result<Value, Error>> + 'static) -> Self {
        Self { next: 0, calls: Vec::new(), ready: VecDeque::new(), script: Box::new(script), done: false }
    }

    /// The calls made on `endpoint` `action`, by resource.
    fn made(&self, endpoint: &str, action: &str) -> Vec<(Vec<String>, Value)> {
        self.calls
            .iter()
            .filter(|(e, a, _, _)| e == endpoint && a == action)
            .map(|(_, _, resource, payload)| (resource.clone(), payload.clone()))
            .collect()
    }
}

impl Calls for Fake {
    fn whoami(&self, peer: &Peer) -> Value {
        peer.whoami(nucleus_protocol::ProcessId(1), &[])
    }
    fn submit(&mut self, route: &Route, _: Option<u64>) -> Result<Result<RequestId, Error>, ChannelError> {
        self.next += 1;
        let request = RequestId(self.next);
        let (endpoint, action, resource, payload) = route;
        self.calls.push((
            (*endpoint).to_owned(),
            (*action).to_owned(),
            resource.segments().to_vec(),
            payload.clone(),
        ));
        if let Some(result) = (self.script)(route) {
            self.ready.push_back((request, result.map(|value| Payload::encode(&value).unwrap())));
        }
        Ok(Ok(request))
    }
    fn next_answer(&mut self) -> Result<Option<Answer>, ChannelError> {
        let answer = self.ready.pop_front();
        self.done |= answer.is_none();
        Ok(answer)
    }
    fn terminate_requested(&self) -> bool {
        self.done
    }
}

/// A window of `label`'s: its jobs go to `jobs`, its answers come out of the
/// returned receiver.
struct Client {
    peer: Arc<Peer>,
    answer: Answers,
    answers: UnboundedReceiver<Value>,
}

fn client(label: Value) -> Client {
    let mut full = json!({"v": 1, "kind": "window", "account": "a-1", "nickname": "N"});
    for (key, value) in label.as_object().unwrap() {
        full[key] = value.clone();
    }
    let peer = Arc::new(Peer::from_label(&full.to_string()).unwrap());
    let (answer, answers) = tokio::sync::mpsc::unbounded_channel();
    Client { peer, answer, answers }
}

impl Client {
    fn send(&self, jobs: &mpsc::Sender<Job>, id: u64, request: Value) {
        let mut request = request;
        request["id"] = json!(id);
        let Request { id, mut operation } = serde_json::from_value(request).unwrap();
        crate::fill(&mut operation, &self.peer);
        jobs.send(Job { id, operation, peer: self.peer.clone(), answer: self.answer.clone() }).unwrap();
    }

    fn answered(&mut self) -> Vec<Value> {
        std::iter::from_fn(|| self.answers.try_recv().ok()).collect()
    }
}

fn wait(project: &str, after_seq: Option<u64>) -> Value {
    json!({"op": "conversation.wait", "project": project, "session": "s", "after_seq": after_seq})
}

/// Fan-out: the windows waiting on one session share one call
/// of the conversation service, whoever they are, and each is answered by
/// its own request's id; a window whose label does not admit the wait is
/// refused before any call and hears nothing of it, and a session's answer
/// reaches only the windows waiting on that session.
#[test]
fn one_call_per_session_fanned_out_only_to_the_windows_waiting_on_it() {
    let (jobs, requests) = mpsc::channel();
    let mut member = client(json!({"role": "member"}));
    let mut owner = client(json!({"role": "owner"}));
    let mut colleague = client(json!({"role": "member", "account": "a-2"}));
    let mut mirror = client(json!({"kind": "mirror", "role": "mirror"}));
    let mut service = client(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-3"}));
    let mut other = client(json!({"role": "member", "account": "a-4"}));
    member.send(&jobs, 1, wait("alpha", None));
    owner.send(&jobs, 2, wait("alpha", Some(3)));
    colleague.send(&jobs, 3, wait("alpha", Some(1)));
    mirror.send(&jobs, 4, wait("alpha", None));
    service.send(&jobs, 5, wait("alpha", None));
    other.send(&jobs, 6, wait("beta", None));
    // alpha's session has records up to 3, beta's up to 1; nothing new comes.
    let mut kernel = Fake::new(|(_, _, resource, payload)| {
        let last = if resource.segments()[0] == "alpha" { 3 } else { 1 };
        match payload["after_seq"].as_u64() {
            Some(after) if after >= last => None,
            _ => Some(Ok(json!({"changed": true, "last_seq": last}))),
        }
    });
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    let waits = kernel.made("conversation", "wait");
    let on = |project: &str| waits.iter().filter(|(resource, _)| resource[0] == project).count();
    // One call to learn where the session is, one to wait past it: never one per window.
    assert_eq!((on("alpha"), on("beta")), (2, 1), "{waits:?}");
    assert_eq!(waits[2].1["after_seq"], 3, "the next call waits past what the watch knows");
    let changed = |id: u64, last: u64| json!({"id": id, "ok": {"changed": true, "last_seq": last}});
    assert_eq!(member.answered(), [changed(1, 3)]);
    assert_eq!(owner.answered(), Vec::<Value>::new(), "it has seen 3: it waits for more");
    assert_eq!(colleague.answered(), [changed(3, 3)]);
    assert_eq!(other.answered(), [changed(6, 1)]);
    for (client, id) in [(&mut mirror, 4), (&mut service, 5)] {
        let refused = client.answered();
        assert_eq!((refused.len(), refused[0]["id"].as_u64()), (1, Some(id)));
        assert_eq!(refused[0]["error"]["code"], "permission_denied", "{refused:?}");
    }
}

/// A window that joins a watch is answered at once when the watch already
/// knows of something it has not seen, and the service's wait ending with
/// nothing new tells every window waiting so.
#[test]
fn a_watch_answers_late_joiners_from_what_it_knows_and_ends_with_the_service_s_wait() {
    let (jobs, requests) = mpsc::channel();
    let mut early = client(json!({"role": "member"}));
    let mut late = client(json!({"role": "owner"}));
    early.send(&jobs, 1, wait("alpha", Some(7)));
    late.send(&jobs, 2, wait("alpha", Some(2)));
    // Records up to 7; the wait past them ends with nothing new.
    let mut kernel = Fake::new(|(_, _, _, payload)| {
        Some(Ok(match payload["after_seq"].as_u64() {
            Some(7) => json!({"changed": false, "last_seq": 7}),
            _ => json!({"changed": true, "last_seq": 7}),
        }))
    });
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    assert_eq!(early.answered(), [json!({"id": 1, "ok": {"changed": false, "last_seq": 7}})]);
    assert_eq!(late.answered(), [json!({"id": 2, "ok": {"changed": true, "last_seq": 7}})]);
    let waits = kernel.made("conversation", "wait");
    assert_eq!(waits.len(), 2, "one call learning where the session is, one waiting past it: {waits:?}");
}

/// More sessions watched than the conversation service holds waits: as many
/// calls as it holds start, the rest wait for a place, and no window is
/// refused or dropped: windows are not limited.
#[test]
fn watches_beyond_the_service_s_waits_queue_and_no_window_is_refused() {
    let (jobs, requests) = mpsc::channel();
    let mut windows: Vec<Client> = (0..MAX_WAITS + 36).map(|_| client(json!({"role": "member"}))).collect();
    for (n, window) in windows.iter().enumerate() {
        let id = u64::try_from(n).unwrap();
        window.send(
            &jobs,
            id,
            json!({"op": "conversation.wait", "project": "alpha", "session": format!("s{n}")}),
        );
    }
    let mut kernel = Fake::new(|_| None);
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    assert_eq!(kernel.made("conversation", "wait").len(), MAX_WAITS);
    assert!(windows.iter_mut().all(|window| window.answered().is_empty()), "nobody is refused");
}

/// Every kind of connection makes requests at once: each hears exactly the
/// answers to its own requests, a call its label refuses is answered as
/// refused before anything is sent, and no call reaches a project the
/// label does not admit.
#[test]
fn no_answer_crosses_windows_roles_or_projects() {
    let (jobs, requests) = mpsc::channel();
    let labels = [
        ("owner", json!({"role": "owner"})),
        ("member", json!({"role": "member"})),
        ("mirror", json!({"kind": "mirror", "role": "mirror"})),
        ("service", json!({"kind": "service", "role": "service", "project": "beta", "as": "a-2"})),
    ];
    let requests_of = |project: &str| {
        vec![
            json!({"op": "conversation.history", "project": project, "session": "s"}),
            json!({"op": "conversation.say", "project": project, "session": "s", "message_id": "u", "text": "t"}),
            json!({"op": "files.list", "dir": project, "path": ""}),
            json!({"op": "projects.delete", "project": project}),
            json!({"op": "sessions.list"}),
        ]
    };
    let mut windows = Vec::new();
    let mut id = 0;
    for (role, label) in labels {
        let window = client(label);
        let mut sent = Vec::new();
        for project in ["alpha", "beta"] {
            for request in requests_of(project) {
                id += 1;
                window.send(&jobs, id, request.clone());
                sent.push((id, request));
            }
        }
        windows.push((role, window, sent));
    }
    // Every call is answered with the project and action it was made on.
    let mut kernel = Fake::new(|(endpoint, action, resource, _)| {
        // (`state`: a deletion is answered with the projects service's `delete`.)
        Some(Ok(
            json!({"call": format!("{endpoint} {action}"), "resource": resource.segments(), "state": "deleted"}),
        ))
    });
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    let mut granted = 0;
    for (role, window, sent) in &mut windows {
        let answers = window.answered();
        granted += answers.iter().filter(|answer| answer.get("ok").is_some()).count();
        let ids: Vec<u64> = answers.iter().map(|a| a["id"].as_u64().unwrap()).collect();
        let mut expected: Vec<u64> = sent.iter().map(|(id, _)| *id).collect();
        let mut got = ids.clone();
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected, "{role} hears its own requests' answers, each once");
        for answer in answers {
            let Some(ok) = answer.get("ok") else { continue };
            let resource: Vec<String> = serde_json::from_value(ok["resource"].clone()).unwrap();
            let project = resource.iter().find(|s| s.as_str() != "projects").cloned().unwrap_or_default();
            let admitted = match *role {
                "service" => vec!["beta"],
                _ => vec!["", "alpha", "beta"],
            };
            assert!(admitted.contains(&project.as_str()), "{role} reached {resource:?}");
            let call = ok["call"].as_str().unwrap();
            let allowed: &[&str] = match *role {
                "owner" | "member" => {
                    &["conversation history", "conversation say", "files list", "sessions list"]
                }
                "mirror" => &["conversation history"],
                "service" => &["conversation say", "projects delete"],
                _ => &[],
            };
            assert!(allowed.contains(&call), "{role} made {call}");
        }
    }
    // What a label may not do never reached the kernel: one call for each answer that is not a refusal.
    assert_eq!(kernel.calls.len(), granted, "refusals made no call");
}

/// Slow runs calls take all but one of the calls' room and slow
/// suggestions all of theirs; sessions.list still goes at once.
#[test]
fn a_service_that_does_not_answer_holds_up_only_its_own_requests() {
    let (jobs, requests) = mpsc::channel();
    let mut window = client(json!({"role": "member"}));
    let suggest = json!({"op": "suggestions.next", "session": "default", "messages": []});
    let lines = std::iter::repeat_n(json!({"op": "runs.list"}), MAX_CALLS - 1)
        .chain(std::iter::repeat_n(suggest, MAX_SUGGESTIONS + 1))
        .chain([json!({"op": "sessions.list"})]);
    for (n, line) in lines.enumerate() {
        window.send(&jobs, u64::try_from(n).unwrap() + 1, line);
    }
    let mut kernel = Fake::new(|(endpoint, _, _, _)| {
        (!matches!(*endpoint, "discovery" | "suggestions")).then(|| Ok(json!({"sessions": []})))
    });
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    let answered = window.answered();
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["id"], u64::try_from(MAX_CALLS + MAX_SUGGESTIONS + 1).unwrap());
    assert_eq!(kernel.calls.len(), MAX_CALLS + MAX_SUGGESTIONS, "one suggestion waits its turn");
}

/// A project's deletion, the platform's: the projects service begins it,
/// every service that keeps records forgets the project, one after another
/// in their order, and the projects service purges it; the request is
/// answered as soon as it began. A service that does not forget stops it:
/// the next request is told why and starts nothing, the one after starts it
/// again from the beginning.
#[test]
fn a_deletion_has_every_service_forget_the_project_before_its_purge() {
    let (jobs, requests) = mpsc::channel();
    let mut service = client(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-3"}));
    let delete = json!({"op": "projects.delete", "project": "alpha"});
    service.send(&jobs, 1, delete.clone());
    let (later, label) = (jobs.clone(), service.peer.clone());
    let answer = service.answer.clone();
    // A service in the middle of the order fails once.
    let failing = crate::FORGET_ORDER[crate::FORGET_ORDER.len() / 2];
    let mut failed = false;
    let mut kernel = Fake::new(move |(endpoint, action, _, _)| {
        if (*endpoint, *action) == (failing, "forget") && !failed {
            failed = true;
            // Asked twice while the failure comes back.
            for id in [2, 3] {
                let Request { id, operation } =
                    serde_json::from_value(json!({"id": id, "op": "projects.delete", "project": "alpha"}))
                        .unwrap();
                later.send(Job { id, operation, peer: label.clone(), answer: answer.clone() }).unwrap();
            }
            return Some(Err(Error::new(ErrorCode::ServiceError, "a member keeps running")));
        }
        Some(Ok(if *action == "delete" { json!({"state": "deleting"}) } else { json!({}) }))
    });
    serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
    let calls: Vec<String> =
        kernel.calls.iter().map(|(e, a, r, _)| format!("{e} {a} {}", r.join("/"))).collect();
    let forgets = crate::FORGET_ORDER.map(|service| format!("{service} forget alpha"));
    let up_to_failure = crate::FORGET_ORDER.iter().position(|service| *service == failing).unwrap();
    let delete = "projects delete projects/alpha".to_owned();
    let expected: Vec<String> = std::iter::once(delete.clone())
        .chain(forgets[..=up_to_failure].iter().cloned())
        .chain(std::iter::once(delete))
        .chain(forgets.iter().cloned())
        .chain(std::iter::once("projects purge projects/alpha".to_owned()))
        .collect();
    assert_eq!(calls, expected);
    let answers = service.answered();
    assert_eq!(answers.len(), 3, "{answers:?}");
    assert_eq!(answers[0], json!({"id": 1, "ok": {"state": "deleting"}}));
    let told = &answers[1];
    assert_eq!((told["id"].as_u64(), told["error"]["code"].as_str()), (Some(2), Some("service_error")));
    assert!(told["error"]["message"].as_str().unwrap().contains(failing), "{told}");
    assert_eq!(answers[2], json!({"id": 3, "ok": {"state": "deleting"}}));
}

#[test]
fn archiving_confirms_all_work_stopped_without_forgetting_any_data() {
    for failure in [None, Some("discovery"), Some("projects")] {
        let (jobs, requests) = mpsc::channel();
        let mut service =
            client(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-3"}));
        service.send(&jobs, 1, json!({"op": "projects.archive", "project": "alpha"}));
        let mut kernel = Fake::new(move |(endpoint, action, _, _)| {
            Some(if Some(*endpoint) == failure && *action == "stop" {
                Err(Error::new(ErrorCode::ServiceError, "work has not stopped"))
            } else {
                Ok(json!({}))
            })
        });
        serve_jobs(&mut kernel, &requests, crate::FORGET_ORDER.to_vec()).unwrap();
        let answers = service.answered();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0]["ok"]["state"] == "archived", failure.is_none(), "{answers:?}");
        assert_eq!(
            kernel.calls.iter().any(|(_, action, _, _)| action == "archive"),
            failure.is_none(),
            "only completed stops may mark the project archived"
        );
        assert!(
            kernel.calls.iter().all(|(_, action, scope, _)| {
                matches!(action.as_str(), "begin_archive" | "archive" | "stop")
                    && scope.last().map(String::as_str) == Some("alpha")
            }),
            "no purge or forget, and every stop stays within this project"
        );
        if failure.is_none() {
            for endpoint in ["sessions", "swarm", "discovery", "memory", "projects"] {
                assert!(kernel.calls.iter().any(|(e, a, _, _)| e == endpoint && a == "stop"));
            }
        }
    }
}
