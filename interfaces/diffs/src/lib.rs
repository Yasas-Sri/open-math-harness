//! The `diffs` service interface, version 2: the uncommitted changes in one
//! git repository, for the window's Review: in the VM, a project's own
//! repository, `repo/` of its folder (no other repository of the folder is
//! looked for, a worktree included).
//!
//! The repository is the person's; the provider only reads it. It keeps
//! nothing of its own, records no history and never runs a command that
//! writes to a repository. A change shows up whoever made it (the agent's file
//! tools, its shell, other agents) as long as it is in the repository: a
//! directory outside it has no diff to show, and discovery runs write
//! outside it, so they are not shown. What was committed is no longer a change.
//!
//! | Action | Payload | Result |
//! | --- | --- | --- |
//! | `repositories` | `{}` | [`Repositories`] |
//! | `changes` | [`ChangesRequest`] | [`Changes`] |
//! | `file` | [`FileRequest`] | [`FileDiff`] |
//!
//! All three are observations and none is a tool. The resource path is `[]`,
//! the whole workspace; a `repository` is a path relative to it, as
//! `repositories` named it, and `changes` and `file` refuse every other.
//!
//! `changes` compares a repository's working tree, staged or not, with its
//! `HEAD` (with nothing, before the first commit), and adds the untracked
//! files git does not ignore. `file` gives one of those files' changed lines.
//!
//! Nothing is cut. A `changes` or `file` reply that would exceed its bound is
//! a page, and `next` / `next_line` say where the next one starts. A
//! `repositories` reply lists the one repository, or none before it is made.
//! A `changes` of a repository with more changed paths than the provider can
//! read is refused with an error naming why, never answered in part. A `binary` file is changed
//! without lines; so is a `too_large` one, with a line over
//! [`MAX_LINE_BYTES`] (no page could carry it).
//!
//! Every changed file carries a `version`: an opaque token that changes when
//! the file's content in the working tree or its `HEAD` version changes (the
//! provider derives it from the `HEAD` object and the file's metadata, not by
//! reading it). Each page of `file` carries the version it was read at, so a
//! caller paging through a file that changed meanwhile sees the versions
//! differ and starts again from the first page instead of mixing two
//! versions; and a caller showing a file's lines re-reads them only when a
//! later `changes` lists another version.
//!
//! This crate defines data only.

use nucleus_protocol::{ActionDecl, ActionName, EffectClass, EndpointName, Error, InterfaceDecl};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "diffs";
pub const ACTION_REPOSITORIES: &str = "repositories";
pub const ACTION_CHANGES: &str = "changes";
pub const ACTION_FILE: &str = "file";
pub const VERSION: u32 = 2;

/// Longest line shown; a file with a longer one is `too_large`.
pub const MAX_LINE_BYTES: usize = 128 * 1024;
/// About the most bytes of paths or lines in one page.
pub const PAGE_BYTES: usize = 256 * 1024;
/// Most files in one page of `changes`.
pub const PAGE_FILES: usize = 500;

/// Result of `repositories`: the one repository the provider shows, or none
/// while it does not exist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repositories {
    pub repositories: Vec<Repository>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    /// Relative to the workspace, `/`-separated; `""` for the workspace itself.
    pub path: String,
    /// The checked-out branch, or `None` when detached or before any commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Payload of `changes`. `after`: the `next` of the previous page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesRequest {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// One page of a repository's changed files, in path order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub repository: String,
    pub files: Vec<FileSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// New since `HEAD`, including untracked files.
    Added,
    Modified,
    Deleted,
}

/// One changed file. `path` is relative to the repository. Line counts are
/// zero for a `binary` or `too_large` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub path: String,
    pub status: Status,
    pub additions: u64,
    pub deletions: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untracked: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub too_large: bool,
    /// Changes whenever the file's lines may have (module doc).
    pub version: String,
}

/// Payload of `file`. `after_line`: the `next_line` of the previous page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRequest {
    pub repository: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_line: Option<u64>,
}

/// One page of a file's changed lines. With `next_line`, ask again with
/// `after_line: next_line`. Each hunk's `line` is exact for its first line,
/// also when it continues a hunk of the previous page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// The file's version when this page was read (module doc).
    pub version: String,
    pub hunks: Vec<Hunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_line: Option<u64>,
}

/// Consecutive changed lines. `line` is the new file's line where the added
/// lines start (1-based); deleted lines stood just before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub line: u64,
    pub deleted: Vec<String>,
    pub added: Vec<String>,
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (ActionName::new(ACTION_REPOSITORIES)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_CHANGES)?, ActionDecl::internal(EffectClass::Observation)),
            (ActionName::new(ACTION_FILE)?, ActionDecl::internal(EffectClass::Observation)),
        ]),
    })
}
