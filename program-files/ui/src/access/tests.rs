#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::{Planned, Request};

fn label(fields: Value) -> String {
    let mut label = json!({"v": 1, "kind": "window", "account": "a-1", "nickname": "Ada", "role": "member"});
    for (key, value) in fields.as_object().unwrap() {
        if value.is_null() {
            label.as_object_mut().unwrap().remove(key);
        } else {
            label[key] = value.clone();
        }
    }
    label.to_string()
}

fn peer(fields: Value) -> Peer {
    let line = label(fields);
    Peer::from_label(&line).unwrap_or_else(|e| panic!("{line}: {e}"))
}

/// Each role's peer, the service link's on `alpha`.
fn roles() -> Vec<(&'static str, Peer)> {
    vec![
        ("owner", peer(json!({"role": "owner"}))),
        ("member", peer(json!({"role": "member"}))),
        ("mirror", peer(json!({"kind": "mirror", "role": "mirror", "nickname": "platform"}))),
        ("service", peer(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-2"}))),
    ]
}

/// The operation of `line`, a request without its id.
fn operation(line: &str) -> crate::Operation {
    let mut request: Value = serde_json::from_str(line).unwrap();
    request["id"] = json!(1);
    let Request { operation, .. } = serde_json::from_value(request).unwrap_or_else(|e| panic!("{line}: {e}"));
    operation
}

/// What `line` comes to for `peer`: as the service decides it.
fn planned(peer: &Peer, line: &str) -> Result<Planned, Error> {
    let mut operation = operation(line);
    crate::fill(&mut operation, peer);
    crate::plan(operation, &peer.access)
}

/// What an operation is, for the label table of module `access`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// No call: `whoami`.
    Who,
    /// A read of a project.
    Read,
    /// A read the mirror makes too.
    Mirrored,
    /// A change in a project.
    Write,
    /// `conversation.say`: a change the platform's service link also makes.
    Say,
    /// Creates exactly the label's named project for a member.
    Create,
    /// `projects.delete`: the platform's service link's alone, for the owner.
    Delete,
    /// The secret scan's findings: the mirror's alone.
    MirrorOnly,
    Git,
}

/// The one call an operation stands for: endpoint, action and resource;
/// `None` for those that are no single call (`whoami`).
type Call = Option<(&'static str, &'static str, &'static [&'static str])>;

/// The resources of session `s`, project `alpha` and its repository.
const S: &[&str] = &["alpha", "s"];
const P: &[&str] = &["projects", "alpha"];
const R: &[&str] = &["projects", "alpha", "repo"];

/// One request of every operation this service has, each in project `alpha`
/// where it names one, with what it is and the call it stands for.
const EVERY_OPERATION: &[(&str, Kind, Call)] = &[
    (r#"{"op":"whoami"}"#, Kind::Who, None),
    (r#"{"op":"sessions.list","after":"default/s","limit":10}"#, Kind::Read, Some(("sessions", "list", &[]))),
    (
        r#"{"op":"sessions.create","project":"alpha","session":"s"}"#,
        Kind::Write,
        Some(("sessions", "create", S)),
    ),
    (r#"{"op":"sessions.stop","project":"alpha","session":"s"}"#, Kind::Write, Some(("sessions", "stop", S))),
    (
        r#"{"op":"sessions.interrupt","project":"alpha","session":"s"}"#,
        Kind::Write,
        Some(("sessions", "interrupt", S)),
    ),
    (
        r#"{"op":"conversation.list","project":"alpha"}"#,
        Kind::Mirrored,
        Some(("conversation", "list", &["alpha"])),
    ),
    (
        r#"{"op":"conversation.ensure","project":"alpha","session":"s"}"#,
        Kind::Write,
        Some(("conversation", "ensure", S)),
    ),
    (
        r#"{"op":"conversation.say","project":"alpha","session":"s","message_id":"u","text":"t"}"#,
        Kind::Say,
        Some(("conversation", "say", S)),
    ),
    (
        r#"{"op":"conversation.queue","project":"alpha","session":"s"}"#,
        Kind::Read,
        Some(("conversation", "queue", S)),
    ),
    (
        r#"{"op":"conversation.move","project":"alpha","session":"s","message_id":"u"}"#,
        Kind::Write,
        Some(("conversation", "move", S)),
    ),
    (
        r#"{"op":"conversation.withdraw","project":"alpha","session":"s","message_id":"u"}"#,
        Kind::Write,
        Some(("conversation", "withdraw", S)),
    ),
    (
        r#"{"op":"conversation.send_now","project":"alpha","session":"s","message_id":"u"}"#,
        Kind::Write,
        Some(("conversation", "send_now", S)),
    ),
    (
        r#"{"op":"conversation.history","project":"alpha","session":"s"}"#,
        Kind::Mirrored,
        Some(("conversation", "history", S)),
    ),
    (
        r#"{"op":"conversation.wait","project":"alpha","session":"s"}"#,
        Kind::Read,
        Some(("conversation", "wait", S)),
    ),
    (
        r#"{"op":"jobs.background","project":"alpha","session":"s","call":"c1"}"#,
        Kind::Write,
        Some(("jobs", "background", S)),
    ),
    // A swarm's read-only card: the team's, never the mirror's or a service link's.
    (r#"{"op":"swarm.status","project":"alpha","session":"s"}"#, Kind::Read, Some(("swarm", "status", S))),
    (
        r#"{"op":"swarm.records","project":"alpha","session":"s","swarm":"s1","after":3,"limit":200}"#,
        Kind::Read,
        Some(("swarm", "records", S)),
    ),
    (r#"{"op":"approvals.list","after":3}"#, Kind::Write, Some(("approvals", "list", &[]))),
    (
        r#"{"op":"approvals.decide","approval":1,"digest":"d","allow":true}"#,
        Kind::Write,
        Some(("approvals", "decide", &[])),
    ),
    (
        r#"{"op":"runs.list","project":"alpha","after":"r","limit":20}"#,
        Kind::Read,
        Some(("discovery", "list", &["alpha"])),
    ),
    (
        r#"{"op":"runs.stop","project":"alpha","run":"r"}"#,
        Kind::Write,
        Some(("discovery", "stop", &["alpha", "r"])),
    ),
    (
        r#"{"op":"runs.status","project":"alpha","run":"r"}"#,
        Kind::Read,
        Some(("discovery", "status", &["alpha", "r"])),
    ),
    (
        r#"{"op":"runs.problems","project":"alpha","run":"r"}"#,
        Kind::Read,
        Some(("discovery", "problems", &["alpha", "r"])),
    ),
    (
        r#"{"op":"runs.tree","project":"alpha","run":"r","problem":"p"}"#,
        Kind::Read,
        Some(("discovery", "tree", &["alpha", "r", "p"])),
    ),
    // What is remembered is a project's: a capability scoped to a session cannot reach it.
    (r#"{"op":"memory.contents","project":"alpha"}"#, Kind::Read, Some(("memory", "contents", &["alpha"]))),
    (
        r#"{"op":"suggestions.next","project":"alpha","session":"s","messages":[]}"#,
        Kind::Write,
        Some(("suggestions", "next", S)),
    ),
    (
        r#"{"op":"suggestions.title","project":"alpha","session":"s","messages":[]}"#,
        Kind::Write,
        Some(("suggestions", "title", S)),
    ),
    (r#"{"op":"skills.list","project":"alpha"}"#, Kind::Read, Some(("skills", "list", R))),
    (r#"{"op":"diffs.repositories","project":"alpha"}"#, Kind::Read, Some(("diffs", "repositories", P))),
    (
        r#"{"op":"diffs.changes","project":"alpha","repository":"repo"}"#,
        Kind::Read,
        Some(("diffs", "changes", P)),
    ),
    (
        r#"{"op":"diffs.file","project":"alpha","repository":"repo","path":"a"}"#,
        Kind::Read,
        Some(("diffs", "file", P)),
    ),
    (
        r#"{"op":"projects.list","after":"a","limit":10}"#,
        Kind::Mirrored,
        Some(("projects", "list", &["projects"])),
    ),
    (r#"{"op":"projects.create","name":"alpha"}"#, Kind::Create, Some(("projects", "create", P))),
    (r#"{"op":"projects.stop","project":"alpha"}"#, Kind::Write, Some(("projects", "stop", P))),
    (r#"{"op":"projects.archive","project":"alpha"}"#, Kind::Delete, Some(("projects", "archive", P))),
    (r#"{"op":"projects.unarchive","project":"alpha"}"#, Kind::Delete, Some(("projects", "unarchive", P))),
    (r#"{"op":"projects.delete","project":"alpha"}"#, Kind::Delete, Some(("projects", "delete", P))),
    (r#"{"op":"files.list","dir":"alpha","path":""}"#, Kind::Read, Some(("files", "list", R))),
    (
        r#"{"op":"files.download","dir":"alpha","path":"a","offset":0}"#,
        Kind::Read,
        Some(("files", "download", R)),
    ),
    (
        r#"{"op":"files.upload","dir":"alpha","path":"a","offset":0,"data_base64":"","last":true}"#,
        Kind::Write,
        Some(("files", "upload", R)),
    ),
    (r#"{"op":"files.mkdir","dir":"alpha","path":"a"}"#, Kind::Write, Some(("files", "mkdir", R))),
    (r#"{"op":"files.rename","dir":"alpha","from":"a","to":"b"}"#, Kind::Write, Some(("files", "rename", R))),
    (r#"{"op":"files.remove","dir":"alpha","path":"a"}"#, Kind::Write, Some(("files", "remove", R))),
    (r#"{"op":"files.git_state","dir":"alpha"}"#, Kind::Git, Some(("files", "git_state", R))),
    (
        r#"{"op":"files.git_initialize","dir":"alpha","branch":"main","oid":"a"}"#,
        Kind::Write,
        Some(("files", "git_initialize", R)),
    ),
    (
        r#"{"op":"files.git_remote","dir":"alpha","url":"https://github.com/Lab/paper.git"}"#,
        Kind::Git,
        Some(("files", "git_remote", R)),
    ),
    (
        r#"{"op":"files.bundle_out","dir":"alpha","commit":"c","basis":[]}"#,
        Kind::Git,
        Some(("files", "bundle_out", R)),
    ),
    (
        r#"{"op":"files.bundle_read","dir":"alpha","bundle":"b","offset":0}"#,
        Kind::Git,
        Some(("files", "bundle_read", R)),
    ),
    (
        r#"{"op":"files.bundle_write","dir":"alpha","offset":0,"data_base64":"","last":true}"#,
        Kind::Git,
        Some(("files", "bundle_write", R)),
    ),
    (
        r#"{"op":"files.bundle_import","dir":"alpha","bundle":"b","branch":"main","oid":"o"}"#,
        Kind::Git,
        Some(("files", "bundle_import", R)),
    ),
    (
        r#"{"op":"scan.findings","project":"alpha","session":"s","salt":"00112233445566778899aabbccddeeff"}"#,
        Kind::MirrorOnly,
        Some(("publish", "findings", S)),
    ),
    // The VM-wide feed of changed sessions.
    (
        r#"{"op":"conversation.changes","after":"e.4","limit":100,"wait":true}"#,
        Kind::Mirrored,
        Some(("conversation", "changes", &[])),
    ),
];

/// Whether the label table of module `access` lets `role` make an operation of `kind`.
fn allowed(role: &str, kind: Kind) -> bool {
    match (role, kind) {
        (_, Kind::Who) => true,
        ("mirror", kind) => matches!(kind, Kind::Mirrored | Kind::MirrorOnly),
        (_, Kind::MirrorOnly) => false,
        ("owner" | "member", kind) => kind != Kind::Delete,
        ("service", kind) => matches!(kind, Kind::Say | Kind::Create | Kind::Delete | Kind::Git),
        _ => panic!("no role {role}"),
    }
}

/// The role table, row by row, against every operation this service has:
/// what a role may do is planned (a call or `whoami`), everything else is refused as `permission_denied` before any
/// call. A new operation fails here until it has a row.
#[test]
fn every_role_may_make_exactly_what_contract_i2_allows() {
    let declared: std::collections::BTreeSet<&str> = include_str!("../lib.rs")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("#[serde(rename = \"")?.strip_suffix("\")]"))
        .collect();
    let listed: std::collections::BTreeSet<&str> = EVERY_OPERATION
        .iter()
        .map(|(line, ..)| line.strip_prefix(r#"{"op":""#).unwrap().split('"').next().unwrap())
        .collect();
    assert_eq!(listed, declared, "every operation of the UI service, and only those");
    assert!(declared.len() > 40 && listed.len() == EVERY_OPERATION.len());
    for (role, peer) in roles() {
        for &(line, kind, _) in EVERY_OPERATION {
            match planned(&peer, line) {
                Ok(_) => assert!(allowed(role, kind), "{role} may {line}"),
                Err(e) => {
                    assert!(!allowed(role, kind), "{role} may not {line}: {e:?}");
                    assert_eq!(e.code, ErrorCode::PermissionDenied, "{role}: {line}");
                }
            }
        }
    }
}

/// Each operation is the one call it names, on its resource; without a
/// project, a session's is the default project's and a list is every project's.
#[test]
fn every_operation_is_one_call_on_its_resource() {
    let without_project: &[(&str, Call)] = &[
        (
            r#"{"op":"conversation.say","session":"s","message_id":"u","text":"t"}"#,
            Some(("conversation", "say", &["default", "s"])),
        ),
        (r#"{"op":"memory.contents"}"#, Some(("memory", "contents", &["default"]))),
        (r#"{"op":"conversation.list"}"#, Some(("conversation", "list", &[]))),
        (r#"{"op":"runs.list"}"#, Some(("discovery", "list", &[]))),
    ];
    let every = EVERY_OPERATION.iter().map(|&(line, _, call)| (line, call));
    for (line, call) in every.chain(without_project.iter().copied()) {
        let routed = crate::route(operation(line)).unwrap_or_else(|e| panic!("{line}: {e:?}"));
        match (routed, call) {
            (None, None) => {}
            (Some((endpoint, action, resource, _)), Some(expected)) => {
                assert_eq!((endpoint, action), (expected.0, expected.1), "{line}");
                assert_eq!(resource.segments(), expected.2, "{line}");
            }
            (routed, _) => panic!("{line}: {routed:?}"),
        }
    }
    // A decision names its approval, never the request's own id.
    let decide = operation(r#"{"op":"approvals.decide","approval":7,"digest":"00ff","allow":true}"#);
    let Some((_, _, _, payload)) = crate::route(decide).unwrap() else { panic!("a call") };
    assert_eq!(payload, json!({"id": 7, "digest": "00ff", "allow": true}));
}

/// A service link reaches its project and nothing else: another project is
/// not there. The mirror reads every project as it is named.
#[test]
fn a_service_link_reaches_its_project_only() {
    let service = peer(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-2"}));
    for line in [
        r#"{"op":"conversation.say","project":"beta","session":"s","message_id":"u","text":"t"}"#,
        r#"{"op":"conversation.say","session":"s","message_id":"u","text":"t"}"#,
        r#"{"op":"projects.delete","project":"beta"}"#,
        r#"{"op":"projects.create","name":"beta"}"#,
        r#"{"op":"files.git_state","dir":"beta"}"#,
        r#"{"op":"files.git_remote","dir":"beta","url":"https://github.com/Lab/paper.git"}"#,
        r#"{"op":"files.bundle_write","dir":"beta","offset":0,"data_base64":"","last":true}"#,
    ] {
        let refused =
            planned(&service, line).err().unwrap_or_else(|| panic!("{line} reached another project"));
        assert_eq!(refused.code, ErrorCode::InvalidInput, "{line}: {refused:?}");
    }
    let segments = |segments: &[&str]| segments.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
    let mirror = peer(json!({"kind": "mirror", "role": "mirror", "nickname": "platform"}));
    let member = peer(json!({"role": "member", "nickname": "Grace", "account": "a-9"}));
    // Its own project's deletion is the deletion workflow's (module `deletion`).
    let Ok(Planned::Delete(project)) = planned(&service, r#"{"op":"projects.delete","project":"alpha"}"#)
    else {
        panic!("its own project's deletion")
    };
    assert_eq!(project, "alpha");
    for (peer, line, endpoint, resource) in [
        // A member's project, created through the service link of exactly that name.
        (
            &service,
            r#"{"op":"projects.create","name":"alpha"}"#,
            "projects",
            segments(&["projects", "alpha"]),
        ),
        // The mirror copies every project: nothing is narrowed for it.
        (&mirror, r#"{"op":"conversation.list"}"#, "conversation", segments(&[])),
        (&mirror, r#"{"op":"projects.list"}"#, "projects", segments(&["projects"])),
        // A member's Stop of a project is the projects service's, on that project.
        (&member, r#"{"op":"projects.stop","project":"beta"}"#, "projects", segments(&["projects", "beta"])),
        (
            &mirror,
            r#"{"op":"conversation.history","project":"beta","session":"s"}"#,
            "conversation",
            segments(&["beta", "s"]),
        ),
    ] {
        let Ok(Planned::Call((got, _, resource_got, _))) = planned(peer, line) else { panic!("{line}") };
        assert_eq!((got, resource_got.segments().to_vec()), (endpoint, resource), "{line}");
    }
}

/// What a connection says or moves carries its label's account as the
/// author, never the nickname; a service link's say is the person it acts
/// for's, its `as`. The window never chooses it.
#[test]
fn what_a_connection_says_is_signed_with_its_label() {
    let say = r#"{"op":"conversation.say","project":"alpha","session":"s","message_id":"u","text":"t"}"#;
    let moved = r#"{"op":"conversation.move","project":"alpha","session":"s","message_id":"u"}"#;
    let member = peer(json!({"role": "member", "nickname": "Grace", "account": "a-9"}));
    let service = peer(
        json!({"kind": "service", "role": "service", "project": "alpha", "nickname": "Lei", "as": "a-7"}),
    );
    for (peer, line, author) in [(&member, say, "a-9"), (&member, moved, "a-9"), (&service, say, "a-7")] {
        let Ok(Planned::Call((_, _, _, payload))) = planned(peer, line) else { panic!("{line}") };
        assert_eq!(payload["author"], author, "{line}");
    }
    assert_eq!(service.on_behalf.as_deref(), Some("a-7"), "the service acts for `as`");
}

/// The label is the platform's, and each kind of it has exactly its fields:
/// anything else in its place closes the connection.
#[test]
fn a_label_names_who_the_connection_is_and_nothing_else() {
    let owner = peer(json!({"role": "owner", "nickname": "Ada", "account": "a-1"}));
    assert_eq!(
        (owner.nickname.as_deref(), owner.author(), &owner.access),
        (Some("Ada"), Some("a-1"), &Access::Team { owner: true })
    );
    assert_eq!(peer(json!({"role": "member"})).access, Access::Team { owner: false });
    assert_eq!(peer(json!({"kind": "mirror", "role": "mirror"})).access, Access::Mirror);
    let service = peer(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-2"}));
    assert_eq!(
        (&service.access, service.on_behalf.as_deref()),
        (&Access::Service { project: "alpha".into() }, Some("a-2"))
    );
    // A joiner and a tag character are dropped; a single selector stays.
    let flag = "Alex \u{1F3F3}\u{FE0F}\u{200D}\u{1F308}\u{E0041}";
    assert_eq!(
        peer(json!({ "nickname": flag })).nickname.as_deref(),
        Some("Alex \u{1F3F3}\u{FE0F}\u{1F308}")
    );
    let long = "a".repeat(MAX_NICKNAME_CHARS + 1);
    for bad in [
        json!({"v": 2}),
        json!({"v": null}),
        json!({"kind": null}),
        json!({"account": null}),
        json!({"nickname": null}),
        json!({"role": null}),
        json!({"kind": "desktop"}),
        json!({"role": "admin"}),
        json!({"role": "publisher", "project": "alpha"}),
        json!({"admin": true}),
        json!({"nickname": ""}),
        json!({"nickname": "\u{200B}"}),
        json!({ "nickname": long }),
        json!({"account": "a b"}),
        json!({"account": ""}),
        // A window is a team person's: every project, and no project of its own.
        json!({"role": "owner", "project": "alpha"}),
        json!({"role": "member", "project": "alpha"}),
        json!({"role": "writer", "project": "alpha"}),
        json!({"role": "commenter", "project": "alpha"}),
        json!({"role": "reader", "project": "alpha"}),
        json!({"role": "reader"}),
        // A window is nobody's service, and only a service acts for someone.
        json!({"role": "member", "as": "a-2"}),
        json!({"kind": "window", "role": "mirror"}),
        json!({"kind": "window", "role": "service", "project": "alpha", "as": "a-2"}),
        json!({"kind": "mirror", "role": "member"}),
        json!({"kind": "mirror", "role": "mirror", "project": "alpha"}),
        json!({"kind": "service", "role": "service", "project": "alpha"}),
        json!({"kind": "service", "role": "service", "as": "a-2"}),
        json!({"kind": "service", "role": "service", "project": "alpha", "as": "x y"}),
        json!({"kind": "service", "role": "service", "project": "../x", "as": "a-2"}),
        json!({"kind": "service", "role": "service", "project": "Alpha", "as": "a-2"}),
        json!({"kind": "service", "role": "writer", "project": "alpha", "as": "a-2"}),
    ] {
        assert!(Peer::from_label(&label(bad.clone())).is_err(), "{bad}");
    }
    for bad in ["not json", r#"{"id":1,"op":"whoami"}"#, "", "{}"] {
        assert!(Peer::from_label(bad).is_err(), "{bad}");
    }
    let padded = format!("{}{}", label(json!({})), " ".repeat(MAX_LABEL_BYTES));
    assert!(Peer::from_label(&padded).unwrap_err().contains("4096"), "a label over 4 KiB");
}

/// `whoami` shows a connection the handles as its role may use them, and
/// the label as it was admitted.
#[test]
fn whoami_shows_what_the_role_may_use() {
    let handle = |endpoint: &str, actions: &[&str]| HandleInfo {
        handle: nucleus_protocol::HandleId(1),
        endpoint: nucleus_protocol::EndpointName::new(endpoint).unwrap(),
        actions: actions.iter().map(|a| nucleus_protocol::ActionName::new(*a).unwrap()).collect(),
        resource: nucleus_protocol::ResourcePath::root(),
        delegable: false,
        expires_in_ms: None,
        max_calls: None,
    };
    let handles = [
        handle("conversation", &["history", "say", "wait"]),
        handle("projects", &["list", "create", "delete"]),
        handle("publish", &["status"]),
    ];
    let shown = |peer: &Peer| {
        let me = peer.whoami(ProcessId(3), &handles);
        let actions: Vec<String> = me["handles"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|h| {
                let endpoint = h["endpoint"].as_str().unwrap().to_owned();
                h["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |a| format!("{endpoint} {}", a.as_str().unwrap()))
            })
            .collect();
        (actions, me["access"].clone())
    };
    let (owner, access) = shown(&peer(json!({"role": "owner"})));
    assert_eq!(
        owner,
        ["conversation history", "conversation say", "conversation wait", "projects create", "projects list"]
    );
    assert_eq!(
        access,
        json!({"kind": "window", "role": "owner", "project": null, "account": "a-1", "nickname": "Ada", "as": null})
    );
    assert_eq!(shown(&peer(json!({"role": "member"}))).0, owner);
    let (mirror, _) = shown(&peer(json!({"kind": "mirror", "role": "mirror"})));
    assert_eq!(mirror, ["conversation history", "projects list"]);
    let (service, access) =
        shown(&peer(json!({"kind": "service", "role": "service", "project": "alpha", "as": "a-2"})));
    assert_eq!(service, ["conversation say", "projects create", "projects delete"]);
    assert_eq!((access["kind"].as_str(), access["as"].as_str()), (Some("service"), Some("a-2")));
}
