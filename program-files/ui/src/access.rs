//! Who a connection is and what it may do.
//!
//! Every stream the platform opens for a window begins with its **label
//! line**, the stream's first line (`--socket --labelled`: the connection's
//! first line), which the platform builds from its own records and the window cannot write
//! ([`Peer::from_label`]):
//!
//! ```text
//! {"v":1,"kind":"window"|"mirror"|"service","account":"<opaque id>","nickname":"…",
//!  "role":"owner"|"member"|"mirror"|"service","project":"NAME"?,"as":"<opaque account id>"?}
//! ```
//!
//! | `kind` | `role` | `project` | `as` | May |
//! | --- | --- | --- | --- | --- |
//! | `window` | `owner`, `member` | none | none | everything this process's grants allow but `projects.delete`, `projects.archive` and `projects.unarchive` |
//! | `mirror` | `mirror` | none | none | `conversation.list`, `conversation.history`, `projects.list` on every project, `conversation.changes`, which a window may make as well, and `scan.findings`, which no other label may make ([`mirror_admits`]) |
//! | `service` | `service` | required | required | `conversation.say` in that project, said for `as`; `projects.create`, `projects.delete`, `projects.archive` or `projects.unarchive` of that project; and its `files.git_state`, `files.git_remote` and `files.bundle_*` |
//!
//! Only the team's owner and members have windows here. A person one project
//! is shared with reads the platform's copy of it and never reaches the VM.
//!
//! Only the platform deletes a project: its `service` link, for the owner who
//! asked it with `project_delete` (which marks the project deleting on the
//! platform first, and removes what the platform holds of it after). An
//! owner's window, and a development socket, never delete one. The deletion
//! is this service's own workflow (module `deletion`): its `forget` calls on
//! the services that keep records and the projects service's `purge` are no
//! connection's to make, and `whoami` shows them to none.
//!
//! Any other combination, an unknown field, a version other than 1, an
//! account or `as` that is not an opaque id (1-64 ASCII letters, digits, `-`,
//! `_`) or a nickname that is nothing once the characters that would hide text
//! are dropped is not a label: the connection is told so and closed.
//!
//! Every call is checked here before it is sent ([`Access::admit`]): a team
//! person's goes as it is, and the kernel checks it against this process's
//! grants; the mirror's and a service link's against what their kind may
//! make, a service link's also against its project, a call about another
//! project being answered as for a project that does not exist.
//!
//! `account` and `nickname` are attribution and presentation only: the
//! account (a `service` label's `as`, the person the platform acts for) is
//! the author the records name for what the connection says, moves,
//! withdraws, sends now or puts in the background ([`Peer::author`]); the nickname is shown by `whoami`
//! and kept in no record, since the platform and the apps show each
//! account's current nickname. Neither is authority on its own: the kind
//! and the role are.
//!
//! The label is not signed: whoever controls the platform, or the link's
//! certificate key and the network path to the VM, can label a link
//! anything. The pin and the VM's link key only make sure that the VM talks to
//! the platform it was given and the platform to the VM it expects.

use nucleus_conversation_interface as conversation;
use nucleus_projects_interface as projects;
use nucleus_protocol::{Error, ErrorCode, HandleInfo, ProcessId};
use nucleus_publish_interface as publish;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Route;

/// The projects service's action that deletes a project
/// (`projects.delete`).
pub const ACTION_DELETE_PROJECT: &str = "delete";

/// Longest label line, in bytes.
pub const MAX_LABEL_BYTES: usize = 4 * 1024;
/// Longest nickname, in characters, once what would hide text is dropped.
pub const MAX_NICKNAME_CHARS: usize = 64;

/// Whether `name` may be a nickname: 1 to [`MAX_NICKNAME_CHARS`] characters,
/// not blank, with no control or hidden character in it.
fn valid_nickname(name: &str) -> bool {
    (1..=MAX_NICKNAME_CHARS).contains(&name.chars().count())
        && !name.trim().is_empty()
        && !nucleus_protocol::text::marked(name).any(|(_, hidden)| hidden)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// The team's owner (`owner: true`) or a member: every project,
    /// everything this process's grants allow but deleting or archiving a project, which
    /// is the platform's ([`Access::Service`]). A connection without a label
    /// (a development socket) is the owner's.
    Team { owner: bool },
    /// The platform's mirror client: reads every project's conversations.
    Mirror,
    /// The platform acting for a person in `project`: `say`, `projects.delete`.
    Service { project: String },
}

/// The `kind` of a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Window,
    Mirror,
    Service,
}

/// The `role` of a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Labelled {
    Owner,
    Member,
    Mirror,
    Service,
}

/// The label line as the platform writes it (module doc).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    v: u32,
    kind: Kind,
    account: String,
    nickname: String,
    role: Labelled,
    #[serde(default)]
    project: Option<String>,
    #[serde(default, rename = "as")]
    on_behalf: Option<String>,
}

/// Who is at the other end of a connection, as the platform's label says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The nickname, for `whoami` only: no record keeps it. `None` only for
    /// a connection without a label.
    pub nickname: Option<String>,
    /// The platform's opaque id of the account behind the connection.
    pub account: Option<String>,
    /// For a `service` label: the account the platform acts for.
    pub on_behalf: Option<String>,
    pub access: Access,
}

impl Peer {
    /// A connection without labels: a development socket.
    pub fn unlabelled() -> Self {
        Self { nickname: None, account: None, on_behalf: None, access: Access::Team { owner: true } }
    }

    /// Who the records name as the author of what this connection says,
    /// moves, withdraws, sends now or puts in the background: the account the platform acts for on a
    /// `service` link, else the connection's own. `None` only without a
    /// label.
    pub fn author(&self) -> Option<&str> {
        self.on_behalf.as_deref().or(self.account.as_deref())
    }

    /// Reads a connection's label line (module doc). The nickname loses the
    /// characters that would hide text (`nucleus_protocol::text`), since people
    /// read it; a label that is not one is refused with why.
    pub fn from_label(line: &str) -> Result<Self, String> {
        if line.len() > MAX_LABEL_BYTES {
            return Err(format!("the connection's label is longer than {MAX_LABEL_BYTES} bytes"));
        }
        let Label { v, kind, account, nickname, role, project, on_behalf } =
            serde_json::from_str(line.trim_end_matches(['\r', '\n']))
                .map_err(|e| format!("the connection's label: {e}"))?;
        if v != 1 {
            return Err(format!("the connection's label is version {v}; this runtime reads version 1"));
        }
        let shown: String = nucleus_protocol::text::marked(&nickname)
            .filter(|&(_, hidden)| !hidden)
            .map(|(c, _)| c)
            .collect();
        if !valid_nickname(&shown) {
            return Err(format!("{nickname:?} is not a nickname"));
        }
        for id in std::iter::once(&account).chain(&on_behalf) {
            if !conversation::valid_person(id) {
                return Err(format!("{id:?} is not an account id"));
            }
        }
        let project = match project {
            Some(name) if projects::valid_name(&name) => Some(name),
            Some(name) => return Err(format!("{name:?} is not a project name")),
            None => None,
        };
        let access = match (kind, role, project.is_some(), on_behalf.is_some()) {
            (Kind::Window, Labelled::Owner, false, false) => Access::Team { owner: true },
            (Kind::Window, Labelled::Member, false, false) => Access::Team { owner: false },
            (Kind::Mirror, Labelled::Mirror, false, false) => Access::Mirror,
            (Kind::Service, Labelled::Service, true, true) => {
                Access::Service { project: project.clone().unwrap_or_default() }
            }
            _ => {
                return Err(format!(
                    "a {kind:?} label with role {role:?}{}{} is not one",
                    if project.is_some() { ", a project" } else { "" },
                    if on_behalf.is_some() { " and `as`" } else { "" }
                ));
            }
        };
        Ok(Self { nickname: Some(shown), account: Some(account), on_behalf, access })
    }

    /// `whoami`: this process, its handles as this connection may use them
    /// (the actions its label allows), and the label as admitted.
    pub fn whoami(&self, process: ProcessId, handles: &[HandleInfo]) -> Value {
        let usable: Vec<HandleInfo> = handles
            .iter()
            .filter_map(|handle| {
                let mut handle = handle.clone();
                let endpoint = handle.endpoint.as_str().to_owned();
                handle.actions.retain(|action| self.access.may(&endpoint, action.as_str()));
                (!handle.actions.is_empty()).then_some(handle)
            })
            .collect();
        let (kind, role, project) = match &self.access {
            Access::Team { owner: true } => ("window", "owner", None),
            Access::Team { owner: false } => ("window", "member", None),
            Access::Mirror => ("mirror", "mirror", None),
            Access::Service { project } => ("service", "service", Some(project)),
        };
        let access = json!({"kind": kind, "role": role, "project": project, "account": self.account,
                            "nickname": self.nickname, "as": self.on_behalf});
        json!({"process": process, "handles": usable, "access": access})
    }
}

/// What the platform's mirror client may call: the reads that copy every
/// project's conversations to the platform, on every project, and nothing
/// else.
pub fn mirror_admits(endpoint: &str, action: &str) -> bool {
    matches!(
        (endpoint, action),
        (conversation::ENDPOINT, conversation::ACTION_LIST | conversation::ACTION_HISTORY)
            | (projects::PROJECTS_ENDPOINT, projects::ACTION_LIST)
    ) || mirror_scan_findings(endpoint, action)
        || mirror_changes(endpoint, action)
}

/// The secret scan's findings of the records the mirror copies (`scan.findings`,
/// the publish service's `findings`): the mirror's, and no other label's.
fn mirror_scan_findings(endpoint: &str, action: &str) -> bool {
    endpoint == publish::ENDPOINT && action == publish::ACTION_FINDINGS
}

/// The VM-wide change feed (`conversation.changes`): which sessions of every
/// project changed and whether a turn is open on them. The mirror follows
/// every session with it; a window reads its running and finished indicators
/// from it.
fn mirror_changes(endpoint: &str, action: &str) -> bool {
    endpoint == conversation::ENDPOINT && action == conversation::ACTION_CHANGES
}

/// Whose endpoints are the mirror's alone, whatever the grants of a team.
fn mirror_only(endpoint: &str) -> bool {
    endpoint == publish::ENDPOINT
}

/// What a `service` label may call, in its project: what the platform does
/// for a person.
fn service_admits(endpoint: &str, action: &str) -> bool {
    matches!(
        (endpoint, action),
        (conversation::ENDPOINT, conversation::ACTION_SAY)
            | (projects::PROJECTS_ENDPOINT, projects::ACTION_CREATE)
            | (
                projects::PROJECTS_ENDPOINT,
                ACTION_DELETE_PROJECT | projects::ACTION_ARCHIVE | projects::ACTION_UNARCHIVE
            )
            | (
                projects::FILES_ENDPOINT,
                projects::ACTION_GIT_STATE
                    | projects::ACTION_GIT_REMOTE
                    | projects::ACTION_BUNDLE_OUT
                    | projects::ACTION_BUNDLE_READ
                    | projects::ACTION_BUNDLE_WRITE
                    | projects::ACTION_BUNDLE_IMPORT
            )
    )
}

/// The answer for anything of a project other than a service link's: the
/// answer for a project that does not exist.
fn no_project(name: &str) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("there is no project {name}"))
}

fn denied(who: &str, endpoint: &str, action: &str) -> Error {
    Error::new(ErrorCode::PermissionDenied, format!("{who}: {endpoint} {action} is not allowed"))
}

impl Access {
    /// Whether this connection's label allows `action` of `endpoint` at all,
    /// whatever its resource (what `whoami` shows).
    pub fn may(&self, endpoint: &str, action: &str) -> bool {
        // The secret scan's findings are the mirror's alone, whatever the
        // grants of a team.
        if mirror_only(endpoint) && !matches!(self, Self::Mirror) {
            return false;
        }
        match self {
            Self::Team { .. } => !deletes_a_project(endpoint, action) && !deletion_step(endpoint, action),
            Self::Mirror => mirror_admits(endpoint, action),
            Self::Service { .. } => service_admits(endpoint, action),
        }
    }

    /// Checks the one call an operation stands for against the label (module
    /// doc). A team's call passes unchanged; the kernel then checks it against
    /// this process's grants.
    pub fn admit(&self, route: &Route) -> Result<(), Error> {
        let (endpoint, action) = (route.0, route.1);
        match self {
            Self::Team { .. } | Self::Service { .. } if mirror_only(endpoint) => {
                Err(denied("the platform's mirror's alone", endpoint, action))
            }
            Self::Team { .. } => {
                if deletes_a_project(endpoint, action) || deletion_step(endpoint, action) {
                    return Err(Error::new(
                        ErrorCode::PermissionDenied,
                        "project deletion and archiving go through the platform for an authorized team manager",
                    ));
                }
                Ok(())
            }
            Self::Mirror => {
                if self.may(endpoint, action) {
                    Ok(())
                } else {
                    Err(denied("the platform's mirror reads conversations", endpoint, action))
                }
            }
            Self::Service { project } => {
                if !self.may(endpoint, action) {
                    return Err(denied(
                        &format!("the platform's service link for {project}"),
                        endpoint,
                        action,
                    ));
                }
                in_project(project, route)
            }
        }
    }
}

fn deletes_a_project(endpoint: &str, action: &str) -> bool {
    endpoint == projects::PROJECTS_ENDPOINT
        && matches!(action, ACTION_DELETE_PROJECT | projects::ACTION_ARCHIVE | projects::ACTION_UNARCHIVE)
}

/// A call of the deletion workflow itself, which no connection makes.
fn deletion_step(endpoint: &str, action: &str) -> bool {
    action == crate::ACTION_FORGET
        || (endpoint == projects::PROJECTS_ENDPOINT
            && matches!(action, projects::ACTION_PURGE | projects::ACTION_BEGIN_ARCHIVE))
}

/// Holds a service link's call, a `say` or a project's deletion, to its
/// project (module doc).
fn in_project(project: &str, route: &Route) -> Result<(), Error> {
    let (endpoint, _, resource, _) = route;
    let named = match (*endpoint, resource.segments()) {
        (conversation::ENDPOINT, [named, ..]) => named,
        (projects::PROJECTS_ENDPOINT | projects::FILES_ENDPOINT, [root, named, ..])
            if root == projects::PROJECTS =>
        {
            named
        }
        _ => return Err(Error::new(ErrorCode::InvalidInput, "the call names no project")),
    };
    if named.as_str() == project { Ok(()) } else { Err(no_project(named)) }
}

#[cfg(test)]
mod tests;
