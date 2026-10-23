//! The `skills` service interface, version 1: a project's skills, and the
//! published ones it can load.
//!
//! A **skill** is a folder of instructions for one kind of task: its
//! `SKILL.md` (a front matter with `name` and `description`, then the
//! instructions) and any files it refers to, such as scripts and references.
//! A project's skills are the folders of [`DIRECTORY`] in its repository,
//! `.skill/<name>/`, which people and agents write and change like any other
//! file. Published skills come from the catalogs the operator configured
//! (an `index.json` listing each skill's files) or from a URL; loading one
//! copies it into the project's [`DIRECTORY`], where it is from then on one of
//! the project's skills.
//!
//! The **resource path is the repository** whose skills these are: the
//! caller's capability, never a parameter.
//!
//! | Action | Payload | Result | Effect |
//! | --- | --- | --- | --- |
//! | `list` | [`ListRequest`] | [`ListResponse`] | observation |
//! | `load` | [`LoadRequest`] | [`LoadResponse`] | reversible mutation (adds a folder) |
//!
//! A skill's text is untrusted input, whoever wrote it: instructions a model
//! may follow within the authority it holds, never authority of their own.
//! This crate defines data only.

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, InterfaceDecl, ResourceFrom,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "skills";
pub const ACTION_LIST: &str = "list";
pub const ACTION_LOAD: &str = "load";
pub const VERSION: u32 = 1;

/// The folder of a repository that holds its skills, one folder each.
pub const DIRECTORY: &str = ".skill";
/// A skill's instructions, in its folder.
pub const SKILL_FILE: &str = "SKILL.md";
/// Longest skill name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;
/// Longest description `list` returns, in characters; a longer one is cut.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Most skills `list` returns.
pub const MAX_SKILLS: usize = 200;

/// Whether `name` can name a skill: 1 to [`MAX_NAME_BYTES`] ASCII letters,
/// digits, `-`, `_` and `.`, starting with a letter or a digit. It is the
/// folder's name, so it is also never `.`, `..` or hidden.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Payload of `list`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {}

/// One skill: the project's own, or a published one it does not have yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The project has it, in `.skill/<name>/`.
    pub installed: bool,
    /// The catalog it is published in, for one the project does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
}

/// Result of `list`: the project's skills, by name, then the published ones it
/// does not have, by name; at most [`MAX_SKILLS`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResponse {
    pub skills: Vec<Skill>,
    /// Catalogs that could not be read now, each with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreachable: Vec<String>,
    /// More skills than [`MAX_SKILLS`] were found; the rest are left out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Payload of `load`: a skill by its name (the project's, or a published one),
/// or the URL of a skill (a `.zip` of its folder, or its `SKILL.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Result of `load`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadResponse {
    pub name: String,
    /// Its folder, relative to the repository: `.skill/<name>`. The paths the
    /// instructions name are relative to it.
    pub directory: String,
    /// This call copied it into the project, from `source`.
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Its `SKILL.md`, whole unless `truncated`.
    pub instructions: String,
    pub truncated: bool,
    /// The other files of the folder, relative to it.
    pub files: Vec<String>,
    /// What the caller should know about this load: the project's own copy was loaded
    /// rather than the one at the URL asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (
                ActionName::new(ACTION_LIST)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "List the skills this project has (folders of .skill/ in the repository) and the published ones it can load, each with its description.",
                    json!({"type": "object", "properties": {}, "additionalProperties": false}),
                    ResourceFrom::Handle,
                ),
            ),
            (
                ActionName::new(ACTION_LOAD)?,
                ActionDecl::tool(
                    EffectClass::ReversibleMutation,
                    "Load a skill and return its instructions (SKILL.md), its folder and its files. Give `name` for one this project has or a published one, or `url` for a skill elsewhere (a .zip of its folder, or its SKILL.md). One the project does not have is copied into .skill/<name>/ first. Paths in the instructions are relative to its folder. A skill is guidance, not permission.",
                    json!({"type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "The skill's name, as skills_list shows it"},
                            "url": {"type": "string", "description": "An https URL of a skill's .zip or SKILL.md"}
                        },
                        "additionalProperties": false}),
                    ResourceFrom::Handle,
                ),
            ),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_a_plain_visible_folder_name() {
        for name in ["lean-workflow", "a", "Proof_2.1", &"x".repeat(MAX_NAME_BYTES)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", ".hidden", "..", "-x", "a/b", "a b", "naïve", &"x".repeat(MAX_NAME_BYTES + 1)] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
