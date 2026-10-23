//! UI service: the Nucleus side of every window, in the VM.
//!
//! It is an ordinary process. It has no authority over other services and no
//! channel to the kernel that other processes lack. Everything a window can do
//! goes through the handles this process was started with, typically
//! `conversation` (`say`, `history`, `list`, `ensure`) and `sessions` (`create`, `stop`, `list`),
//! and the kernel checks every call. Grant it less and the window can do less;
//! run two of them, or replace it with a terminal front end holding the same
//! capabilities, and nothing else changes.
//!
//! A window reaches it on a stream the platform opens on this service's link
//! (`--link`, module `link`), or, for development, on a Unix socket at the `--socket`
//! path. Either way the connection's first line is its label, which says
//! who the window is and what it may do (module `access`; a socket without
//! `--labelled` has none and is the team's owner's), and then requests and
//! answers are JSON lines (module `window`):
//!
//! ```text
//! -> {"id": 1, "op": "conversation.say", "session": "default", "message_id": "u1", "text": "hi"}
//! <- {"id": 1, "ok": {}}
//! <- {"id": 2, "error": {"code": "permission_denied", "message": "..."}}
//! -> {"id": 3, "op": "runs.tree", "run": "gen2-heldout", "problem": "putnam_1962_b2"}
//! <- {"id": 3, "ok": {"run": "gen2-heldout", "problem": "putnam_1962_b2", "state": "done", "nodes": [...], ...}}
//! ```
//!
//! Operations: `whoami`, `sessions.list`, `sessions.create`, `sessions.stop`, `sessions.interrupt`,
//! `conversation.say`, `conversation.history`, `conversation.wait`, `conversation.list`,
//! `conversation.ensure`, `conversation.queue`, `conversation.move`, `conversation.withdraw`,
//! `conversation.send_now`, `jobs.background`, `swarm.status`,
//! `swarm.records`, `approvals.list`, `approvals.decide`, `runs.list`, `runs.status`, `runs.problems`,
//! `runs.tree`, `runs.stop`, `memory.contents`, `skills.list`, `diffs.repositories`, `diffs.changes`,
//! `diffs.file`, `suggestions.next`, `suggestions.title`, the projects operations below,
//! `conversation.changes`
//! (the platform's mirror and the windows) and `scan.findings` (the platform's
//! mirror alone). The set is fixed; the service is not a generic kernel
//! proxy. Every operation but `whoami`, `projects.archive` and
//! `projects.delete` is exactly one call on one endpoint, made only if this
//! process holds a handle for that endpoint and action; otherwise it is
//! refused as `permission_denied` before anything is sent, the answer the
//! kernel would give.
//!
//! Lists come a page at a time: `sessions.list {after?, limit?}`,
//! `runs.list {project?, after?, limit?}` and `approvals.list {after?,
//! limit?}` pass the cursor and size to their services and answer their
//! pages, with `next` when there is more.
//!
//! `sessions.create` takes the `model` its agent runs and an optional
//! `reasoning_effort` (`sessions.list` gives each session's, `null` for
//! `default`);
//! `conversation.say` takes optional `files` (`[{name, media_type, path,
//! bytes}]`, files the window uploaded first into `attachments/<session>/` of
//! the session's working directory with `files.upload`); limits in the
//! conversation interface. They are checked by the service that receives them;
//! this service reads no file.
//!
//! `conversation.queue {project?, session}`, `conversation.move {project?,
//! session, message_id, before?}` and `conversation.withdraw {project?,
//! session, message_id}` list the messages waiting for a session's agent,
//! move one, and withdraw one: each one call of `queue`, `move` or
//! `withdraw` on `[project, session]`.
//!
//! `jobs.background {project?, session, call}` moves the running shell
//! command of the session's agent with tool call `call` to the background:
//! one call of `background` on `[project, session]`, with the connection's
//! label as the `author` the job names, as for `say`.
//!
//! `swarm.status {project?, session}` and `swarm.records {project?, session,
//! swarm?, after?, limit?}` read a session's swarms for the
//! window's read-only card: one call of the swarm service's `status` or
//! `records` on `[project, session]`, with the interface's payload, answered
//! verbatim. They are the team's (module `access`); the window has no other
//! swarm operation: only the session's agent starts, judges or stops one.
//!
//! `runs.*` are the window's run views, calls of the resident discovery
//! service on a project's runs (an absent `project` is the default
//! project): `runs.list` is `discovery list [project]`, or every project's
//! runs without a project,
//! `runs.status {project?, run}` is `status [project, run]`,
//! `runs.problems {project?, run}` is `problems [project, run]`,
//! `runs.tree {project?, run, problem}` is `tree [project, run, problem]`
//! and `runs.stop {project?, run}` is `stop [project, run]`, each with its
//! own required grant. Each result is the interface's JSON verbatim; the
//! window holds no filesystem capability for any of it.
//!
//! `diffs.*` are the window's Review, observations of the `diffs` service on
//! a project's repository: `diffs.repositories {}`, `diffs.changes
//! {repository, after?}` and `diffs.file {repository, path, after_line?}`
//! carry the interface's payloads and return its pages verbatim. The window
//! pages through them with `after` / `after_line`.
//!
//! The person's projects are calls of the `projects`
//! service: `projects.list {after?, limit?}` (a page, by name, with `next`
//! while there are more), `projects.create {name}`,
//! `projects.stop {project}` (the project's Stop: every process of it ends,
//! answered once none is left), `projects.archive {project}`,
//! `projects.unarchive {project}` and `projects.delete {project}`, then, on one
//! directory named by its `dir`
//! id, `files.list {dir, path, after?}`, `files.download {dir, path,
//! offset}`, `files.upload {dir, path, offset, data_base64, last,
//! replace?}`, `files.mkdir {dir, path}`, `files.rename {dir, from, to}`,
//! `files.remove {dir, path}`, and the git bundles that carry commits between
//! the repository and the platform: `files.git_state {dir}`, `files.git_initialize {dir,
//! branch, oid}`, `files.git_remote {dir, url}`,
//! `files.bundle_out {dir, commit, basis}`, `files.bundle_read {dir, bundle,
//! offset}`, `files.bundle_write {dir, bundle?, offset, data_base64, last}`
//! and `files.bundle_import {dir, bundle, branch, oid}`. Each carries the
//! interface's payload and returns its result verbatim. Project creation may take the
//! interface's git timeout and has a room of its own. `projects.delete` deletes a project and its folder,
//! which only the platform's `service` link for that project
//! may ask, for the owner's `project_delete` (module `access`): this
//! service's own workflow (module `deletion`), the projects service's
//! `delete` and `purge` on `["projects", project]` with every service that
//! keeps records of the project forgetting it between them. This service is
//! booted last, after every service it calls.
//!
//! `suggestions.next {session, messages}` asks the `suggestions` service, a
//! service beside the sessions, for the message the person might send next:
//! `next` on `[project, session]` with the interface's payload (`{messages}`, the
//! window's own transcript, bounded by the interface). `suggestions.title
//! {session, messages}` asks it for a short title in the same way, which the
//! window saves when the session has none. The service holds no capability
//! of the session; a newer request for the same session and action
//! supersedes an older one there.
//!
//! `scan.findings {project, session, salt, after?}` is the secret scan of a
//! session's records, the publish service's `findings` on `[project,
//! session]` with `{salt, after?}` (the salt 16-128 lowercase hexadecimal
//! characters, the platform's), answered `{records: [{seq, findings: [{rule,
//! preview, fingerprint}], more?}], through?, next?, rules}`. Only the
//! platform's mirror may make it.
//!
//! `conversation.changes {after?, limit?, wait?}` is the conversation
//! service's `changes` on `[]` (its interface's "The change feed"):
//! which sessions of the VM have records newer than the cursor `after` and
//! whether a turn is open on each,
//! answered `{changes: [{project, session, seq, turn_open?}], next, more?, reset?}`; with
//! `wait`, held until a change or the service's wait. It is how the mirror
//! follows every session with one call instead of a `conversation.wait`
//! each, and how a window shows which of its other chats run or just
//! finished. Its calls have a room of their own
//! (`serve::MAX_CHANGES_CALLS`), so a held one never takes the room of a
//! window's other calls.
//!
//! Every connection's first line is the platform's label for it (module
//! `access`): who the connection is (`kind`, `account`,
//! `nickname`), its `role` and, for the platform's service link, its project. What
//! the connection says (`conversation.say`), moves, withdraws, sends now or
//! puts in the background is recorded with its account (a service link's `as`) as its author, never
//! its nickname, which the platform and the apps show for the account; an
//! operation that names an author (or a person) itself is refused, so a page
//! never chooses who it acts as. Every call is checked against the label
//! before it is sent. A character of the nickname that would
//! hide text (`nucleus_protocol::text`) is dropped, since people read the
//! name; a connection whose label is not one is answered with an error and
//! closed.
//!
//! `whoami` answers with this process's id, its handles (endpoint, actions,
//! resource, delegability, expiry, budget) as the connection's role may use
//! them, and the connection's `access`, its label as admitted, so a front end
//! decides what to show from what it holds: the run views when a
//! `discovery` handle allows `list` and `status`, Stop only when one allows
//! `stop`.
//!
//! Trust: whoever reaches a connection acts with this process's
//! capabilities as its label allows, no more. A link is reached only through
//! the platform, which writes the label (module `link`). A development
//! socket's file is created with mode 0600 in the bound directory; other
//! software running as the same host user can still connect. Input is
//! untrusted and size-limited; what comes back from services is passed on as
//! data for the window to render as text.
//!
//! A compromised UI service can act as any label within its grants, delete
//! any project and have the services that keep a project's records forget
//! them (module `deletion`: the workflow's `forget` and `purge`, which no
//! label reaches), and take the windows the platform hands this VM. The
//! team's model key is not in its sandbox; on the network it reaches what the
//! VM reaches, its link key in hand.
//!
//! There is no limit on windows: a window is a task, not a thread, each with
//! up to `MAX_IN_FLIGHT` requests outstanding; answers carry the request `id`
//! and may come out of order. One thread owns the kernel
//! channel (module `serve`) and submits each call without waiting for it;
//! the windows waiting on one session share one `conversation.wait` (module
//! `fanout`). The process holds nothing that needs orderly release beyond a
//! development socket's file, which the next start replaces.

use std::path::PathBuf;
use std::sync::mpsc;

use access::ACTION_DELETE_PROJECT;
use nucleus_approvals_interface as approvals;
use nucleus_conversation_interface as conversation;
use nucleus_diffs_interface as diffs;
use nucleus_discovery_interface as discovery;
use nucleus_memory_interface as memory;
use nucleus_projects_interface as projects;
use nucleus_protocol::{Error, ErrorCode, HandleId, ResourcePath};
use nucleus_publish_interface as publish;
use nucleus_sdk::{Connection, Event};
use nucleus_sessions_interface as sessions;
use nucleus_shell_interface::jobs;
use nucleus_skills_interface as skills;
use nucleus_suggestions_interface as suggestions;
use nucleus_swarm_interface as swarm;
use serde::Deserialize;
use serde_json::{Value, json};

mod access;
mod deletion;
mod fanout;
pub mod github;
pub mod link;
mod platform;
mod serve;
mod window;
pub use access::{Access, Peer};
pub use deletion::{ACTION_FORGET, FORGET_ORDER};
pub use serve::MAX_PENDING;

/// Where the windows come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Front {
    /// A connection to the platform (module `link`): its configuration file.
    Link { config: PathBuf },
    /// A development socket: every connection's first line is its label
    /// when `labelled`, else every connection is the team's owner's.
    Socket { path: PathBuf, labelled: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub front: Front,
}

impl Options {
    /// Parses `--link <file>` or `--socket <path> [--labelled]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut link, mut socket, mut labelled) = (None, None, false);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if flag == "--labelled" {
                labelled = true;
                continue;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--link" => link = Some(PathBuf::from(value)),
                "--socket" => socket = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let front = match (link, socket) {
            (Some(config), None) if !labelled => Front::Link { config },
            (None, Some(path)) => Front::Socket { path, labelled },
            _ => {
                return Err("either --link <file> or --socket <path> [--labelled]".into());
            }
        };
        Ok(Self { front })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub enum Operation {
    #[serde(rename = "whoami")]
    WhoAmI,
    /// One page of the saved sessions, every project's (sessions interface:
    /// `{after?, limit?}` → `{…, next?}`).
    #[serde(rename = "sessions.list")]
    SessionsList {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// A session is named by its project and its name; a
    /// request that names no `project` is about the default project's.
    /// `model`: the provider/model id its agent runs. `reasoning_effort`:
    /// absent means `default`.
    #[serde(rename = "sessions.create")]
    SessionsCreate {
        #[serde(default)]
        project: Option<String>,
        session: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        reasoning_effort: Option<sessions::EffortSetting>,
    },
    #[serde(rename = "sessions.stop")]
    SessionsStop {
        #[serde(default)]
        project: Option<String>,
        session: String,
    },
    #[serde(rename = "sessions.interrupt")]
    SessionsInterrupt {
        #[serde(default)]
        project: Option<String>,
        session: String,
    },
    /// Every project's saved sessions, or one project's.
    #[serde(rename = "conversation.list")]
    ConversationsList {
        #[serde(default)]
        project: Option<String>,
        after: Option<String>,
    },
    /// The session exists, in its project's repository.
    #[serde(rename = "conversation.ensure")]
    ConversationEnsure {
        #[serde(default)]
        project: Option<String>,
        session: String,
    },
    /// `files`: references to uploaded files; `images`: small images carried
    /// inline; all validated by the
    /// conversation service. `author` is the
    /// connection's label (module doc), set here, never read from the
    /// request: a request that names one is refused.
    #[serde(rename = "conversation.say")]
    Say {
        #[serde(default)]
        project: Option<String>,
        session: String,
        message_id: String,
        text: String,
        #[serde(default)]
        files: Vec<conversation::FileRef>,
        #[serde(default)]
        images: Vec<conversation::InlineImage>,
        /// The sender's own model and reasoning for this message (conversation interface 23).
        #[serde(default)]
        settings: Option<conversation::MessageSettings>,
        #[serde(skip)]
        author: Option<String>,
    },
    /// The messages waiting for the session's agent, in the order it will
    /// receive them.
    #[serde(rename = "conversation.queue")]
    Queue {
        #[serde(default)]
        project: Option<String>,
        session: String,
    },
    /// Moves the waiting message `message_id` before the waiting message
    /// `before` (a message id), or to the end. `author` as for `Say`.
    #[serde(rename = "conversation.move")]
    Move {
        #[serde(default)]
        project: Option<String>,
        session: String,
        message_id: String,
        #[serde(default)]
        before: Option<String>,
        #[serde(skip)]
        author: Option<String>,
    },
    /// Withdraws the waiting message `message_id`: the agent never receives
    /// it. `author` as for `Say`.
    #[serde(rename = "conversation.withdraw")]
    Withdraw {
        #[serde(default)]
        project: Option<String>,
        session: String,
        message_id: String,
        #[serde(skip)]
        author: Option<String>,
    },
    /// Makes the original queued message available at the running task's next step.
    #[serde(rename = "conversation.send_now")]
    SendNow {
        #[serde(default)]
        project: Option<String>,
        session: String,
        message_id: String,
        #[serde(skip)]
        author: Option<String>,
    },
    /// Moves the running shell command of the session's agent with tool
    /// call `call` to the background. `author` as for `Say`.
    #[serde(rename = "jobs.background")]
    JobsBackground {
        #[serde(default)]
        project: Option<String>,
        session: String,
        call: String,
        #[serde(skip)]
        author: Option<String>,
    },
    /// The session's latest swarm as its service sees it now.
    #[serde(rename = "swarm.status")]
    SwarmStatus {
        #[serde(default)]
        project: Option<String>,
        session: String,
    },
    /// One page of a swarm's records: `swarm` one of the session's swarms
    /// (its latest when absent), from `after`, at most `limit`.
    #[serde(rename = "swarm.records")]
    SwarmRecords {
        #[serde(default)]
        project: Option<String>,
        session: String,
        #[serde(default)]
        swarm: Option<String>,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "conversation.history")]
    History {
        #[serde(default)]
        project: Option<String>,
        session: String,
        after_seq: Option<u64>,
    },
    /// Long-poll: answered once `history` has a record after `after_seq`, or
    /// `changed: false` after the service's wait (`conversation::WAIT_MS`).
    #[serde(rename = "conversation.wait")]
    Wait {
        #[serde(default)]
        project: Option<String>,
        session: String,
        after_seq: Option<u64>,
    },
    /// Which sessions of the VM changed after a cursor, for the platform's
    /// mirror and for windows: the conversation service's `changes` on `[]`.
    #[serde(rename = "conversation.changes")]
    Changes {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        wait: bool,
    },
    /// One page of the requests waiting for a decision.
    #[serde(rename = "approvals.list")]
    ApprovalsList {
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// `approval` names the pending request (`id` is this envelope's own field);
    /// `digest` is what the window showed the person, and the guard refuses a mismatch.
    #[serde(rename = "approvals.decide")]
    ApprovalsDecide { approval: u64, digest: String, allow: bool },
    /// One page of the runs of a project, or of every project.
    #[serde(rename = "runs.list")]
    RunsList {
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "runs.stop")]
    RunsStop {
        #[serde(default)]
        project: Option<String>,
        run: String,
    },
    #[serde(rename = "runs.status")]
    RunsStatus {
        #[serde(default)]
        project: Option<String>,
        run: String,
    },
    #[serde(rename = "runs.problems")]
    RunsProblems {
        #[serde(default)]
        project: Option<String>,
        run: String,
    },
    #[serde(rename = "runs.tree")]
    RunsTree {
        #[serde(default)]
        project: Option<String>,
        run: String,
        problem: String,
    },
    /// What a project remembers, for a person to read: the consolidated file
    /// and what the memory service is doing with it. An observation, on the
    /// project's path; there is no operation that writes a memory, and the
    /// window holds none that could.
    #[serde(rename = "memory.contents")]
    MemoryContents {
        #[serde(default)]
        project: Option<String>,
    },
    /// The message the person might send next in `session`, from the
    /// messages the window sends (the interface's `NextRequest`).
    #[serde(rename = "suggestions.next")]
    SuggestionsNext {
        #[serde(default)]
        project: Option<String>,
        session: String,
        messages: Vec<suggestions::Message>,
        #[serde(default)]
        skills: Vec<suggestions::Skill>,
    },
    /// A short title for `session`, from the messages the window sends (the
    /// interface's `TitleRequest`); the window saves it when the session has
    /// none.
    #[serde(rename = "suggestions.title")]
    SuggestionsTitle {
        #[serde(default)]
        project: Option<String>,
        session: String,
        messages: Vec<suggestions::Message>,
    },
    /// The skills of a project's repository and the published ones it can
    /// load, for the `/skill` command: an observation of the project's
    /// `skills` host on `["projects", project, "repo"]`.
    #[serde(rename = "skills.list")]
    SkillsList {
        #[serde(default)]
        project: Option<String>,
    },
    /// The Review of a project (the default one when none is named): its git
    /// repositories, one page of a repository's changed files, one page of a
    /// file's changed lines. Observations of the project's `diffs` host on
    /// `["projects", project]`, with the interface's own payloads; a
    /// repository is a path in the project's home (`repo`).
    #[serde(rename = "diffs.repositories")]
    DiffsRepositories {
        #[serde(default)]
        project: Option<String>,
    },
    #[serde(rename = "diffs.changes")]
    DiffsChanges {
        #[serde(default)]
        project: Option<String>,
        repository: String,
        #[serde(default)]
        after: Option<String>,
    },
    #[serde(rename = "diffs.file")]
    DiffsFile {
        #[serde(default)]
        project: Option<String>,
        repository: String,
        path: String,
        #[serde(default)]
        after_line: Option<u64>,
    },
    /// One page of the person's projects, at the `projects`
    /// service: `{after?, limit?}` as its `list` takes them.
    #[serde(rename = "projects.list")]
    ProjectsList {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// A new project, with an empty repository.
    #[serde(rename = "projects.create")]
    ProjectsCreate { name: String },
    /// Ends every process of a project (its Stop), whoever started it;
    /// answered once none is left. Its files and records stay.
    #[serde(rename = "projects.stop")]
    ProjectsStop { project: String },
    #[serde(rename = "projects.archive")]
    ProjectsArchive { project: String },
    #[serde(rename = "projects.unarchive")]
    ProjectsUnarchive { project: String },
    /// Deletes a project and its files: the platform's, for the owner
    /// (module `access`).
    #[serde(rename = "projects.delete")]
    ProjectsDelete { project: String },
    #[serde(rename = "files.list")]
    FilesList {
        dir: String,
        path: String,
        #[serde(default)]
        after: Option<String>,
    },
    #[serde(rename = "files.download")]
    FilesDownload { dir: String, path: String, offset: u64 },
    #[serde(rename = "files.upload")]
    FilesUpload {
        dir: String,
        path: String,
        offset: u64,
        data_base64: String,
        last: bool,
        #[serde(default)]
        replace: bool,
        #[serde(default)]
        expected_revision: Option<String>,
        #[serde(default)]
        upload_id: Option<String>,
    },
    #[serde(rename = "files.mkdir")]
    FilesMkdir { dir: String, path: String },
    #[serde(rename = "files.rename")]
    FilesRename { dir: String, from: String, to: String },
    #[serde(rename = "files.remove")]
    FilesRemove { dir: String, path: String },
    #[serde(rename = "files.git_state")]
    FilesGitState { dir: String },
    #[serde(rename = "files.git_initialize")]
    FilesGitInitialize { dir: String, branch: String, oid: String },
    #[serde(rename = "files.git_remote")]
    FilesGitRemote { dir: String, url: String },
    #[serde(rename = "files.bundle_out")]
    FilesBundleOut { dir: String, commit: String, basis: Vec<String> },
    #[serde(rename = "files.bundle_read")]
    FilesBundleRead { dir: String, bundle: String, offset: u64 },
    #[serde(rename = "files.bundle_write")]
    FilesBundleWrite {
        dir: String,
        #[serde(default)]
        bundle: Option<String>,
        offset: u64,
        data_base64: String,
        last: bool,
    },
    #[serde(rename = "files.bundle_import")]
    FilesBundleImport {
        dir: String,
        bundle: String,
        branch: String,
        oid: String,
        #[serde(default)]
        review: bool,
    },
    /// The secret scan's findings of one page of a session's records, for
    /// the platform's mirror alone (module `access`): the publish service's
    /// `findings` on `[project, session]`.
    #[serde(rename = "scan.findings")]
    ScanFindings {
        project: String,
        session: String,
        salt: String,
        #[serde(default)]
        after: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub operation: Operation,
}

/// Where an operation goes: endpoint, action, resource, payload.
pub type Route = (&'static str, &'static str, ResourcePath, Value);

/// Maps an operation to the one call it stands for. `WhoAmI` maps to nothing.
pub fn route(operation: Operation) -> Result<Option<Route>, Error> {
    // A session's resource: its project (the default one when none is
    // named) and its name.
    let session_of =
        |project: Option<String>, session: String| ResourcePath::new([project_of(project)?, session]);
    Ok(Some(match operation {
        Operation::WhoAmI => return Ok(None),
        Operation::SessionsList { after, limit } => (
            sessions::ENDPOINT,
            sessions::ACTION_LIST,
            ResourcePath::root(),
            json!(sessions::ListRequest { after, limit }),
        ),
        Operation::SessionsCreate { project, session, model, reasoning_effort } => (
            sessions::ENDPOINT,
            sessions::ACTION_CREATE,
            session_of(project, session)?,
            json!(sessions::CreateRequest { model, reasoning_effort }),
        ),
        Operation::SessionsStop { project, session } => {
            (sessions::ENDPOINT, sessions::ACTION_STOP, session_of(project, session)?, json!({}))
        }
        Operation::SessionsInterrupt { project, session } => {
            (sessions::ENDPOINT, sessions::ACTION_INTERRUPT, session_of(project, session)?, json!({}))
        }
        Operation::ConversationsList { project, after } => {
            let scope = match project {
                None => ResourcePath::root(),
                Some(project) => ResourcePath::new([project_of(Some(project))?])?,
            };
            (conversation::ENDPOINT, conversation::ACTION_LIST, scope, json!({"after": after}))
        }
        Operation::ConversationEnsure { project, session } => (
            conversation::ENDPOINT,
            conversation::ACTION_ENSURE,
            session_of(project, session)?,
            json!(conversation::EnsureRequest {}),
        ),
        Operation::Say { project, session, message_id, text, files, images, settings, author } => (
            conversation::ENDPOINT,
            conversation::ACTION_SAY,
            session_of(project, session)?,
            json!(conversation::SayRequest { id: message_id, text, files, images, author, settings }),
        ),
        Operation::Queue { project, session } => (
            conversation::ENDPOINT,
            conversation::ACTION_QUEUE,
            session_of(project, session)?,
            json!(conversation::QueueRequest {}),
        ),
        Operation::Move { project, session, message_id, before, author } => (
            conversation::ENDPOINT,
            conversation::ACTION_MOVE,
            session_of(project, session)?,
            json!(conversation::MoveRequest { id: message_id, before, author }),
        ),
        Operation::Withdraw { project, session, message_id, author } => (
            conversation::ENDPOINT,
            conversation::ACTION_WITHDRAW,
            session_of(project, session)?,
            json!(conversation::WithdrawRequest { id: message_id, author }),
        ),
        Operation::SendNow { project, session, message_id, author } => (
            conversation::ENDPOINT,
            conversation::ACTION_SEND_NOW,
            session_of(project, session)?,
            json!(conversation::SendNowRequest { id: message_id, author }),
        ),
        Operation::JobsBackground { project, session, call, author } => (
            jobs::ENDPOINT,
            jobs::ACTION_BACKGROUND,
            session_of(project, session)?,
            json!(jobs::BackgroundRequest { call, author }),
        ),
        Operation::SwarmStatus { project, session } => (
            swarm::ENDPOINT,
            swarm::ACTION_STATUS,
            session_of(project, session)?,
            json!(swarm::StatusRequest::default()),
        ),
        Operation::SwarmRecords { project, session, swarm: id, after, limit } => {
            let request = swarm::RecordsRequest { swarm: id, after, limit };
            request.validate()?;
            (swarm::ENDPOINT, swarm::ACTION_RECORDS, session_of(project, session)?, json!(request))
        }
        Operation::ApprovalsList { after, limit } => (
            approvals::ENDPOINT,
            approvals::ACTION_LIST,
            ResourcePath::root(),
            json!(approvals::ListRequest { after, limit }),
        ),
        Operation::ApprovalsDecide { approval, digest, allow } => (
            approvals::ENDPOINT,
            approvals::ACTION_DECIDE,
            ResourcePath::root(),
            json!({"id": approval, "digest": digest, "allow": allow}),
        ),
        Operation::History { project, session, after_seq } => (
            conversation::ENDPOINT,
            conversation::ACTION_HISTORY,
            session_of(project, session)?,
            json!({"after_seq": after_seq}),
        ),
        Operation::Wait { project, session, after_seq } => (
            conversation::ENDPOINT,
            conversation::ACTION_WAIT,
            session_of(project, session)?,
            json!(conversation::WaitRequest { after_seq }),
        ),
        Operation::Changes { after, limit, wait } => {
            let request = conversation::ChangesRequest { after, limit, wait };
            request.validate()?;
            (conversation::ENDPOINT, conversation::ACTION_CHANGES, ResourcePath::root(), json!(request))
        }
        Operation::MemoryContents { project } => {
            (memory::ENDPOINT, memory::ACTION_CONTENTS, ResourcePath::new([project_of(project)?])?, json!({}))
        }
        Operation::SkillsList { project } => (
            skills::ENDPOINT,
            skills::ACTION_LIST,
            ResourcePath::new([
                projects::PROJECTS.to_owned(),
                project_of(project)?,
                projects::REPOSITORY.to_owned(),
            ])?,
            json!(skills::ListRequest {}),
        ),
        Operation::SuggestionsNext { project, session, messages, skills } => (
            suggestions::ENDPOINT,
            suggestions::ACTION_NEXT,
            session_of(project, session)?,
            json!(suggestions::NextRequest { messages, skills }),
        ),
        Operation::SuggestionsTitle { project, session, messages } => (
            suggestions::ENDPOINT,
            suggestions::ACTION_TITLE,
            session_of(project, session)?,
            json!(suggestions::TitleRequest { messages }),
        ),
        Operation::DiffsRepositories { project } => (
            diffs::ENDPOINT,
            diffs::ACTION_REPOSITORIES,
            projects::project_resource(&project_of(project)?)?,
            json!({}),
        ),
        Operation::DiffsChanges { project, repository, after } => (
            diffs::ENDPOINT,
            diffs::ACTION_CHANGES,
            projects::project_resource(&project_of(project)?)?,
            json!(diffs::ChangesRequest { repository, after }),
        ),
        Operation::DiffsFile { project, repository, path, after_line } => (
            diffs::ENDPOINT,
            diffs::ACTION_FILE,
            projects::project_resource(&project_of(project)?)?,
            json!(diffs::FileRequest { repository, path, after_line }),
        ),
        Operation::RunsList { project, after, limit } => {
            let resource = match project {
                None => ResourcePath::root(),
                Some(project) => ResourcePath::new([project_of(Some(project))?])?,
            };
            (
                discovery::ENDPOINT,
                discovery::ACTION_LIST,
                resource,
                json!(discovery::ListRequest { after, limit }),
            )
        }
        Operation::RunsStop { project, run } => {
            (discovery::ENDPOINT, discovery::ACTION_STOP, run_of(project, run)?, json!({}))
        }
        Operation::RunsStatus { project, run } => {
            (discovery::ENDPOINT, discovery::ACTION_STATUS, run_of(project, run)?, json!({}))
        }
        Operation::RunsProblems { project, run } => {
            (discovery::ENDPOINT, discovery::ACTION_PROBLEMS, run_of(project, run)?, json!({}))
        }
        Operation::RunsTree { project, run, problem } => (
            discovery::ENDPOINT,
            discovery::ACTION_TREE,
            ResourcePath::new([project_of(project)?, run, problem])?,
            json!({}),
        ),
        Operation::ScanFindings { project, session, salt, after } => {
            let request = publish::FindingsRequest { salt, after };
            request.validate()?;
            (publish::ENDPOINT, publish::ACTION_FINDINGS, session_of(Some(project), session)?, json!(request))
        }
        project @ (Operation::ProjectsList { .. }
        | Operation::ProjectsCreate { .. }
        | Operation::ProjectsStop { .. }
        | Operation::ProjectsArchive { .. }
        | Operation::ProjectsUnarchive { .. }
        | Operation::ProjectsDelete { .. }
        | Operation::FilesList { .. }
        | Operation::FilesDownload { .. }
        | Operation::FilesUpload { .. }
        | Operation::FilesMkdir { .. }
        | Operation::FilesRename { .. }
        | Operation::FilesRemove { .. }
        | Operation::FilesGitState { .. }
        | Operation::FilesGitInitialize { .. }
        | Operation::FilesGitRemote { .. }
        | Operation::FilesBundleOut { .. }
        | Operation::FilesBundleRead { .. }
        | Operation::FilesBundleWrite { .. }
        | Operation::FilesBundleImport { .. }) => route_project(&project)?,
    }))
}

/// `[project, run]`: a run of `project` (absent: the default project).
fn run_of(project: Option<String>, run: String) -> Result<ResourcePath, Error> {
    ResourcePath::new([project_of(project)?, run])
}

/// The project a request names, or the default one; refused unless a name.
fn project_of(project: Option<String>) -> Result<String, Error> {
    let project = project.unwrap_or_else(|| projects::DEFAULT_PROJECT.to_owned());
    if projects::valid_name(&project) {
        Ok(project)
    } else {
        Err(Error::new(ErrorCode::InvalidInput, format!("{project:?} is not a project name")))
    }
}

/// The call of a projects or files operation. A working directory is the
/// resource its `dir` id stands for.
fn route_project(operation: &Operation) -> Result<Route, Error> {
    use projects::{FILES_ENDPOINT as FILES, PROJECTS_ENDPOINT as PROJECTS};
    let dir = |id: &str| projects::Dir::parse(id).map(|dir| dir.resource());
    let all = || ResourcePath::new([projects::PROJECTS]);
    let owned = |text: &str| text.to_owned();
    Ok(match operation {
        Operation::ProjectsList { after, limit } => (
            PROJECTS,
            projects::ACTION_LIST,
            all()?,
            json!(projects::ListProjects { after: after.clone(), limit: *limit }),
        ),
        Operation::ProjectsCreate { name } => {
            (PROJECTS, projects::ACTION_CREATE, projects::project_resource(name)?, json!({}))
        }
        Operation::ProjectsArchive { project } => {
            (PROJECTS, projects::ACTION_ARCHIVE, projects::project_resource(project)?, json!({}))
        }
        Operation::ProjectsUnarchive { project } => {
            (PROJECTS, projects::ACTION_UNARCHIVE, projects::project_resource(project)?, json!({}))
        }
        Operation::ProjectsStop { project } => {
            (PROJECTS, projects::ACTION_STOP, projects::project_resource(project)?, json!({}))
        }
        Operation::ProjectsDelete { project } => {
            (PROJECTS, ACTION_DELETE_PROJECT, projects::project_resource(project)?, json!({}))
        }
        Operation::FilesList { dir: id, path, after } => (
            FILES,
            projects::ACTION_LIST,
            dir(id)?,
            json!(projects::ListRequest { path: owned(path), after: after.clone() }),
        ),
        Operation::FilesDownload { dir: id, path, offset } => (
            FILES,
            projects::ACTION_DOWNLOAD,
            dir(id)?,
            json!(projects::DownloadRequest { path: owned(path), offset: *offset }),
        ),
        Operation::FilesUpload {
            dir: id,
            path,
            offset,
            data_base64,
            last,
            replace,
            expected_revision,
            upload_id,
        } => (
            FILES,
            projects::ACTION_UPLOAD,
            dir(id)?,
            json!(projects::UploadRequest {
                path: owned(path),
                offset: *offset,
                data_base64: owned(data_base64),
                last: *last,
                replace: *replace,
                expected_revision: expected_revision.clone(),
                upload_id: upload_id.clone(),
            }),
        ),
        Operation::FilesMkdir { dir: id, path } => {
            (FILES, projects::ACTION_MKDIR, dir(id)?, json!(projects::PathRequest { path: owned(path) }))
        }
        Operation::FilesRename { dir: id, from, to } => (
            FILES,
            projects::ACTION_RENAME,
            dir(id)?,
            json!(projects::RenameRequest { from: owned(from), to: owned(to) }),
        ),
        Operation::FilesRemove { dir: id, path } => {
            (FILES, projects::ACTION_REMOVE, dir(id)?, json!(projects::PathRequest { path: owned(path) }))
        }
        Operation::FilesGitInitialize { dir: id, branch, oid } => (
            FILES,
            projects::ACTION_GIT_INITIALIZE,
            dir(id)?,
            json!(projects::GitInitialize { branch: owned(branch), oid: owned(oid) }),
        ),
        Operation::FilesGitRemote { dir: id, url } => {
            (FILES, projects::ACTION_GIT_REMOTE, dir(id)?, json!(projects::GitRemote { url: owned(url) }))
        }
        Operation::FilesGitState { dir: id } => (FILES, projects::ACTION_GIT_STATE, dir(id)?, json!({})),
        Operation::FilesBundleOut { dir: id, commit, basis } => (
            FILES,
            projects::ACTION_BUNDLE_OUT,
            dir(id)?,
            json!(projects::BundleOut { commit: owned(commit), basis: basis.clone() }),
        ),
        Operation::FilesBundleRead { dir: id, bundle, offset } => (
            FILES,
            projects::ACTION_BUNDLE_READ,
            dir(id)?,
            json!(projects::BundleRead { bundle: owned(bundle), offset: *offset }),
        ),
        Operation::FilesBundleWrite { dir: id, bundle, offset, data_base64, last } => (
            FILES,
            projects::ACTION_BUNDLE_WRITE,
            dir(id)?,
            json!(projects::BundleWrite {
                bundle: bundle.clone(),
                offset: *offset,
                data_base64: owned(data_base64),
                last: *last,
            }),
        ),
        Operation::FilesBundleImport { dir: id, bundle, branch, oid, review } => (
            FILES,
            projects::ACTION_BUNDLE_IMPORT,
            dir(id)?,
            json!(projects::BundleImport {
                bundle: owned(bundle),
                branch: owned(branch),
                oid: owned(oid),
                review: *review
            }),
        ),
        _ => return Err(Error::new(ErrorCode::InvalidInput, "not a projects operation")),
    })
}

type Kernel = Connection<std::io::Stdin, std::io::Stdout>;

fn handle_for(kernel: &Kernel, endpoint: &str, action: &str) -> Result<HandleId, Error> {
    // Without a matching handle the answer is the same one the kernel would give.
    kernel.find_handle(endpoint, action).ok_or_else(|| {
        Error::new(
            ErrorCode::PermissionDenied,
            format!("this front end was not granted `{endpoint}` `{action}`"),
        )
    })
}

/// Sets what an operation says about who makes it from the connection's
/// label: the author (the account, [`Peer::author`]) of what it says, moves,
/// withdraws, sends now or puts in the background. A request never sets it: it is not among its fields.
pub(crate) fn fill(operation: &mut Operation, connection: &Peer) {
    match operation {
        Operation::Say { author, .. }
        | Operation::Move { author, .. }
        | Operation::Withdraw { author, .. }
        | Operation::SendNow { author, .. }
        | Operation::JobsBackground { author, .. } => {
            *author = connection.author().map(str::to_owned);
        }
        _ => {}
    }
}

/// Kernel deadline of a git operation beyond the time git may take: the
/// project's host may have to start first.
const GIT_MARGIN_MS: u64 = 15_000;
/// Kernel deadline of a wait: longer than the service holds one, so an idle
/// wait comes back as `changed: false`.
const WAIT_DEADLINE_MS: u64 = conversation::WAIT_MS + 5_000;
/// Kernel deadline of `suggestions.next`, which waits for a model: under the
/// desktop's 30 s request timeout, so a slow model reaches the window as this
/// call's timeout and frees its slot here.
const SUGGESTIONS_DEADLINE_MS: u64 = 25_000;
/// Kernel deadline of `skills.list`, which may read the catalogs: under the
/// desktop's 30 s request timeout.
const SKILLS_DEADLINE_MS: u64 = 20_000;
/// Kernel deadline of a `diffs.*` call, which runs git in the workspace: under
/// the desktop's 30 s request timeout, so a slow read reaches the window as
/// this call's timeout.
const DIFFS_DEADLINE_MS: u64 = 25_000;

/// The kernel deadline of an operation's call; `None` is the kernel's default.
fn deadline_ms(operation: &Operation) -> Option<u64> {
    match operation {
        Operation::Wait { .. } | Operation::Changes { wait: true, .. } => Some(WAIT_DEADLINE_MS),
        Operation::DiffsRepositories { .. }
        | Operation::DiffsChanges { .. }
        | Operation::DiffsFile { .. } => Some(DIFFS_DEADLINE_MS),
        Operation::SuggestionsNext { .. } | Operation::SuggestionsTitle { .. } => {
            Some(SUGGESTIONS_DEADLINE_MS)
        }
        Operation::SkillsList { .. } => Some(SKILLS_DEADLINE_MS),
        Operation::ProjectsCreate { .. }
        | Operation::FilesGitState { .. }
        | Operation::FilesGitInitialize { .. }
        | Operation::FilesGitRemote { .. }
        | Operation::FilesBundleOut { .. }
        | Operation::FilesBundleImport { .. } => Some(projects::GIT_TIMEOUT_MS + GIT_MARGIN_MS),
        _ => None,
    }
}

/// What an operation of a connection comes to, before anything is sent.
pub(crate) enum Planned {
    /// Answered here, with no call.
    WhoAmI,
    /// The one call it stands for, as the connection's access lets it go out.
    Call(Route),
    /// The deletion of this project ([`deletion`]): the platform's, admitted
    /// for its service link of that project.
    Delete(String),
    Archive(String),
}

/// Decides an operation of a connection with `access`: refused, or what it
/// comes to. Everything a connection may not do is refused here.
pub(crate) fn plan(operation: Operation, access: &Access) -> Result<Planned, Error> {
    let archiving = match &operation {
        Operation::ProjectsArchive { project } => Some(project.clone()),
        _ => None,
    };
    let deleting = match &operation {
        Operation::ProjectsDelete { project } => Some(project.clone()),
        _ => None,
    };
    let Some(route) = route(operation)? else { return Ok(Planned::WhoAmI) };
    access.admit(&route)?;
    if let Some(project) = archiving {
        return Ok(Planned::Archive(project));
    }
    Ok(match deleting {
        Some(project) => Planned::Delete(project),
        None => Planned::Call(route),
    })
}

/// The room a request's call takes while outstanding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Room {
    Wait,
    Changes,
    Suggestion,
    Call,
    Git,
    /// A step of a project's deletion ([`deletion`]), which no operation's
    /// call takes.
    Deletion,
}

impl Room {
    fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Wait { .. } => Self::Wait,
            Operation::Changes { .. } => Self::Changes,
            Operation::SuggestionsNext { .. } | Operation::SuggestionsTitle { .. } => Self::Suggestion,
            Operation::ProjectsCreate { .. } => Self::Git,
            _ => Self::Call,
        }
    }
}

/// Serves windows until the kernel channel closes: the kernel's thread is
/// this one (`serve::serve_jobs`); the windows' connections are tasks of a
/// small runtime of their own.
pub fn run(options: &Options) -> Result<(), String> {
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    // Nothing here uses child exit events; keeping them would fill the backlog.
    kernel.set_event_handler(|_, event| Ok(!matches!(event, Event::Terminate)));
    let forgets = deletion::authority(&kernel)?;
    let (jobs, requests) = mpsc::channel();
    let inbox = serve::Inbox::for_kernel(jobs, kernel.waker());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("the windows' runtime: {e}"))?;
    let front = runtime.block_on(async { front_end(options, inbox) })?;
    let outcome = serve::serve_jobs(&mut kernel, &requests, forgets).map_err(|e| e.to_string());
    runtime.shutdown_background();
    if let Some(socket) = front {
        let _ = std::fs::remove_file(socket);
    }
    outcome
}

/// Starts serving windows as `options` says, on the current runtime; the
/// socket file to remove at the end, if any.
fn front_end(options: &Options, inbox: serve::Inbox) -> Result<Option<PathBuf>, String> {
    use std::os::unix::fs::PermissionsExt;
    match &options.front {
        Front::Link { config } => {
            link::run(config.clone(), inbox);
            Ok(None)
        }
        Front::Socket { path, labelled } => {
            // A socket file left by a previous instance would make bind fail.
            let _ = std::fs::remove_file(path);
            let listener =
                tokio::net::UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("restrict socket: {e}"))?;
            tokio::spawn(window::serve_socket(listener, *labelled, inbox));
            Ok(Some(path.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn parse(line: &str) -> Result<Request, serde_json::Error> {
        serde_json::from_str(line)
    }

    #[test]
    fn an_unknown_operation_is_answered_by_its_id() {
        let line = r#"{"id":7,"op":"conversation.later","session":"default"}"#;
        assert!(parse(line).is_err());
        assert_eq!(serve::request_id(line.as_bytes()), Some(7));
        assert_eq!(serve::request_id(b"not json"), None);
    }

    #[test]
    fn the_ui_service_is_not_a_generic_proxy() {
        for bad in [
            r#"{"id":1,"op":"kernel.spawn","program":"chat"}"#,
            r#"{"id":1,"op":"conversation.post","session":"default","entry":{"kind":"assistant","text":"x"}}"#,
            r#"{"id":1,"op":"conversation.say","session":"default","message_id":"u1","text":"hi","as":"operator"}"#,
            // Who says, moves or withdraws it is the connection's label, never the page's word.
            r#"{"id":1,"op":"conversation.say","session":"default","message_id":"u1","text":"hi","author":"Ada"}"#,
            r#"{"id":1,"op":"conversation.withdraw","session":"default","message_id":"u1","author":"Ada"}"#,
            r#"{"id":1,"op":"conversation.move","session":"default","message_id":"u1","author":"Ada"}"#,
            r#"{"id":1,"op":"conversation.send_now","session":"default","message_id":"u1","author":"Ada"}"#,
            r#"{"id":1,"op":"jobs.background","session":"default","call":"c1","author":"Ada"}"#,
            // A swarm is its session's agent's: the window only reads one, whole.
            r#"{"id":1,"op":"swarm.start","session":"default","task":"x"}"#,
            r#"{"id":1,"op":"swarm.stop","session":"default","swarm":"s1"}"#,
            r#"{"id":1,"op":"swarm.verdict","session":"default","member":"m1","accept":true,"reason":"r"}"#,
            r#"{"id":1,"op":"swarm.status","session":"default","direction":"d1"}"#,
            // Comments are the platform's: none enters the VM.
            r#"{"id":1,"op":"conversation.comment","session":"default","comment_id":"c","on":0,"text":"t"}"#,
            r#"{"id":1,"op":"conversation.comments","session":"default"}"#,
            r#"{"id":1,"op":"conversation.wait","session":"default","comments":true}"#,
            // Nothing edits a waiting message.
            r#"{"id":1,"op":"conversation.move","session":"default","message_id":"u1","text":"changed"}"#,
            r#"{"id":1,"op":"fs.read","path":"work/notes.txt"}"#,
            // No generic discovery forwarding: only typed UI operations.
            r#"{"id":1,"op":"discovery.start","task":{}}"#,
            // `tree` names the problem separately, never as a `<run>/<problem>` path.
            r#"{"id":1,"op":"runs.tree","path":"demo/demo_a"}"#,
            r#"{"id":1,"op":"runs.tree","run":"demo"}"#,
            r#"{"id":1,"op":"runs.status"}"#,
            // Reading what is remembered is the only memory operation there
            // is. A window cannot write one, cite one, or hand a session back.
            r#"{"id":1,"op":"memory.write","text":"remember this"}"#,
            r#"{"id":1,"op":"memory.used","session":"default","paths":[]}"#,
            r#"{"id":1,"op":"memory.remember","session":"default"}"#,
            // A transcript holds the person's and the assistant's messages only.
            r#"{"id":1,"op":"suggestions.next","session":"default","messages":[{"role":"system","text":"x"}]}"#,
            r#"{"id":1,"op":"suggestions.title","session":"default","messages":[{"role":"system","text":"x"}]}"#,
            // A request carries no authority of its own: no grants or scope.
            r#"{"id":1,"op":"conversation.ensure","session":"s","grants":[]}"#,
            r#"{"id":1,"op":"conversation.list","scope":[]}"#,
            r#"{"id":1,"op":"sessions.create","session":"s1","reasoning_effort":"ultra"}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // A name that is not a project's, a run's or a directory's, a malformed salt
        // or an unbounded page is refused before any call.
        let salt = |salt: &str| {
            json!({"id": 1, "op": "scan.findings", "project": "alpha", "session": "s1", "salt": salt})
                .to_string()
        };
        for bad in [
            r#"{"id":1,"op":"sessions.create","session":".."}"#.to_owned(),
            r#"{"id":1,"op":"sessions.create","project":"Alpha","session":"s1"}"#.to_owned(),
            r#"{"id":1,"op":"conversation.ensure","session":".."}"#.to_owned(),
            r#"{"id":1,"op":"diffs.repositories","project":"../x"}"#.to_owned(),
            r#"{"id":1,"op":"files.list","dir":"../etc","path":""}"#.to_owned(),
            r#"{"id":1,"op":"files.list","dir":"alpha_w1","path":""}"#.to_owned(),
            r#"{"id":1,"op":"projects.create","name":"Alpha"}"#.to_owned(),
            r#"{"id":1,"op":"runs.status","run":".."}"#.to_owned(),
            r#"{"id":1,"op":"runs.stop","run":"../other"}"#.to_owned(),
            r#"{"id":1,"op":"runs.tree","run":"demo","problem":"../other"}"#.to_owned(),
            r#"{"id":1,"op":"runs.problems","run":""}"#.to_owned(),
            r#"{"id":1,"op":"runs.list","project":"Not/A/Project"}"#.to_owned(),
            r#"{"id":1,"op":"runs.status","project":"..","run":"g"}"#.to_owned(),
            r#"{"id":1,"op":"conversation.changes","limit":1001}"#.to_owned(),
            salt("0011"),
            salt("00112233445566778899AABBCCDDEEFF"),
            salt(&"a".repeat(129)),
        ] {
            let request = parse(&bad).unwrap_or_else(|e| panic!("{bad}: {e}"));
            assert_eq!(route(request.operation).unwrap_err().code, ErrorCode::InvalidInput, "{bad}");
        }
    }

    /// A held `changes` is a wait, and has the wait's deadline; other calls
    /// keep the kernel's.
    #[test]
    fn a_held_change_feed_has_the_waits_deadline() {
        for (line, expected) in [
            (r#"{"id":6,"op":"runs.status","run":"n"}"#, None),
            (r#"{"id":7,"op":"conversation.changes","wait":true}"#, Some(WAIT_DEADLINE_MS)),
            (r#"{"id":8,"op":"conversation.changes"}"#, None),
        ] {
            let request = parse(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!(deadline_ms(&request.operation), expected, "{line}");
        }
    }
}
