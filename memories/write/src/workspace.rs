//! The memory workspace: plain files, with a baseline in the job store.
//!
//! Everything remembered is a file. The contents of those files at the last
//! successful consolidation are the baseline, kept in SQLite beside the job
//! state (`Store::baseline`), outside the directory any agent can reach. That is
//! not bookkeeping — the unified diff against the baseline *is* the prompt. The
//! agent that merges memories is told what changed since it last looked, in
//! the form a person would read, rather than being handed the whole corpus and
//! asked to spot the difference.
//!
//! It also decides whether there is anything to do at all. If syncing the
//! selected rows leaves the files equal to the baseline, nothing has changed
//! and no model is called.
//!
//! Only the files this module names are part of the baseline: `raw_memories.md`,
//! `summaries/*`, `MEMORY.md` and `memory_summary.md`. Moving the baseline is one
//! SQLite transaction, so readers see either the previous index or the new one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::storage::{Stage1Output, Store};

/// Where the merged raw memories are written, in stable order.
pub const RAW: &str = "raw_memories.md";
/// One file per selected session.
pub const SUMMARIES: &str = "summaries";
/// What changed since the last successful consolidation.
pub const DIFF: &str = "workspace_diff.md";
/// What the consolidating agent maintains.
pub const INDEX: &str = "MEMORY.md";
pub const SUMMARY: &str = "memory_summary.md";
pub const CANDIDATES: &str = ".candidates";
/// A candidate is untrusted text, bounded before it is published.
pub const MAX_ARTIFACT_BYTES: u64 = 32 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Store(rusqlite::Error),
    /// A workspace file or candidate is not what this module accepts.
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Store(e) => write!(f, "store: {e}"),
            Error::Invalid(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Store(e)
    }
}

/// Writes the selected memories into the workspace and removes the summaries of
/// anything no longer selected.
///
/// Order is by session id rather than by rank, deliberately: ranking moves
/// every time a memory is cited, and a file whose content is stable should not
/// appear in the diff because something else was used.
pub fn sync(root: &Path, selected: &[Stage1Output]) -> Result<(), Error> {
    let summaries = root.join(SUMMARIES);
    std::fs::create_dir_all(&summaries)?;

    let mut ordered: Vec<&Stage1Output> = selected.iter().collect();
    ordered.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    let mut raw = String::new();
    let mut keep: Vec<String> = Vec::new();
    for out in &ordered {
        raw.push_str(&format!("## {}\n\n{}\n\n", out.session_id, out.raw_memory.trim()));
        let name = file_name_for(out);
        std::fs::write(summaries.join(&name), format!("{}\n", out.session_summary.trim()))?;
        keep.push(name);
    }
    if ordered.is_empty() {
        raw.push_str("Nothing is currently selected.\n");
    }
    std::fs::write(root.join(RAW), raw)?;

    for entry in std::fs::read_dir(&summaries)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The summary file for one session: its slug when it has a usable one, its id
/// otherwise, so two sessions cannot collide on a slug the model chose.
///
/// Public because it is the only mapping between a file a session can cite and
/// the row whose usage that citation counts towards. A second implementation
/// of this rule elsewhere would be a second answer to the same question.
#[must_use]
pub fn file_name_for(out: &Stage1Output) -> String {
    let slug = out.session_slug.as_deref().unwrap_or("").trim();
    let safe: String = slug
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if safe.is_empty() { format!("{}.md", out.session_id) } else { format!("{safe}-{}.md", out.session_id) }
}

/// A fresh per-attempt directory. It holds copies, never the workspace itself, and is never reused,
/// so late writes from cancelled calls cannot affect a subsequent attempt.
pub fn candidate(root: &Path, key: &str) -> Result<PathBuf, Error> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::Invalid("invalid candidate identity".into()));
    }
    let parent = root.join(CANDIDATES);
    std::fs::create_dir_all(&parent)?;
    let path = parent.join(key);
    std::fs::create_dir(&path)?;
    std::fs::create_dir(path.join(SUMMARIES))?;
    for name in [RAW, DIFF, INDEX, SUMMARY] {
        let source = root.join(name);
        if source.exists() {
            std::fs::copy(source, path.join(name))?;
        }
    }
    for entry in std::fs::read_dir(root.join(SUMMARIES))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(Error::Invalid("summary is not a regular file".into()));
        }
        std::fs::copy(entry.path(), path.join(SUMMARIES).join(entry.file_name()))?;
    }
    Ok(path)
}

/// Accept only the two declared UTF-8 artifacts, never model-selected paths. A success status alone is not artifact validation.
pub fn accept_candidate(root: &Path, candidate: &Path) -> Result<(), Error> {
    let mut accepted = Vec::new();
    for name in [INDEX, SUMMARY] {
        let file = candidate.join(name);
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && name == SUMMARY => {
                accepted.push((name, None));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() || meta.len() > MAX_ARTIFACT_BYTES {
            return Err(Error::Invalid(format!(
                "{name}: expected a regular text file of at most {MAX_ARTIFACT_BYTES} bytes"
            )));
        }
        let text = std::fs::read_to_string(file)?;
        accepted.push((name, Some(text)));
    }
    for (name, text) in accepted {
        match text {
            Some(text) => std::fs::write(root.join(name), text)?,
            None if root.join(name).exists() => std::fs::remove_file(root.join(name))?,
            None => {}
        }
    }
    Ok(())
}

/// The memory directory the operator selected, not a path supplied by a model
/// or a service request.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Creates the directory and, the first time this store sees it, records the
    /// files already there as the baseline: nothing for a fresh workspace, the
    /// files of an existing one.
    pub fn init(&self, store: &Store, now: i64) -> Result<(), Error> {
        std::fs::create_dir_all(self.root.join(SUMMARIES))?;
        if store.baseline()?.is_none() {
            store.set_baseline(&self.files()?, now)?;
        }
        Ok(())
    }

    /// The baselined files as they are on disk now, keyed by relative path.
    fn files(&self) -> Result<BTreeMap<String, String>, Error> {
        let mut files = BTreeMap::new();
        let mut read = |relative: String, path: &Path| -> Result<(), Error> {
            match std::fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
                Ok(meta) if !meta.is_file() => {
                    return Err(Error::Invalid(format!("{relative} is not a regular file")));
                }
                Ok(_) => {}
            }
            let text = std::fs::read_to_string(path)
                .map_err(|e| Error::Invalid(format!("{relative} is not readable UTF-8: {e}")))?;
            files.insert(relative, text);
            Ok(())
        };
        for name in [RAW, INDEX, SUMMARY] {
            read(name.to_owned(), &self.root.join(name))?;
        }
        let summaries = self.root.join(SUMMARIES);
        if summaries.is_dir() {
            for entry in std::fs::read_dir(&summaries)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                read(format!("{SUMMARIES}/{name}"), &entry.path())?;
            }
        }
        Ok(files)
    }

    fn baseline(store: &Store) -> Result<BTreeMap<String, String>, Error> {
        Ok(store.baseline()?.unwrap_or_default())
    }

    /// Whether anything changed. When nothing did, no model is called: the
    /// consolidation has nothing to consolidate.
    pub fn is_dirty(&self, store: &Store) -> Result<bool, Error> {
        Ok(self.files()? != Self::baseline(store)?)
    }

    /// Writes the diff since the last successful consolidation and returns its path.
    ///
    /// Includes files that are new, changed and removed: a memory that fell out of
    /// selection is exactly the kind of thing the consolidating agent needs to act
    /// on, and it is invisible in a listing of what remains.
    pub fn write_diff(&self, store: &Store) -> Result<PathBuf, Error> {
        let path = self.root.join(DIFF);
        std::fs::write(&path, unified_diff(&Self::baseline(store)?, &self.files()?))?;
        Ok(path)
    }

    /// Throws away everything since the baseline, after a consolidation failed.
    ///
    /// The workspace is the input to the next pass as well as the output of this
    /// one, so a half-merged index would be diffed as though a person had meant
    /// it. The previous baseline is the last state that was whole, and the rows
    /// the pass had synced are regenerated from the store next time.
    pub fn reset(&self, store: &Store) -> Result<(), Error> {
        let baseline = Self::baseline(store)?;
        for name in self.files()?.keys() {
            if !baseline.contains_key(name) {
                std::fs::remove_file(self.root.join(name))?;
            }
        }
        std::fs::create_dir_all(self.root.join(SUMMARIES))?;
        for (name, text) in &baseline {
            std::fs::write(self.root.join(name), text)?;
        }
        match std::fs::remove_file(self.root.join(DIFF)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Moves the baseline to the current state, after a consolidation succeeded.
    /// The diff file is a prompt artifact and is removed, not baselined.
    pub fn commit_baseline(&self, store: &Store, now: i64) -> Result<(), Error> {
        match std::fs::remove_file(self.root.join(DIFF)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        store.set_baseline(&self.files()?, now)?;
        Ok(())
    }
}

/// A unified diff of every file that differs, in path order.
fn unified_diff(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> String {
    let mut names: Vec<&String> = old.keys().chain(new.keys()).collect();
    names.sort();
    names.dedup();
    let mut out = String::new();
    for name in names {
        let (before, after) = (old.get(name), new.get(name));
        if before == after {
            continue;
        }
        let a = if before.is_some() { format!("a/{name}") } else { "/dev/null".to_owned() };
        let b = if after.is_some() { format!("b/{name}") } else { "/dev/null".to_owned() };
        out.push_str(&format!("diff a/{name} b/{name}\n"));
        match (before, after) {
            (None, _) => out.push_str("new file\n"),
            (_, None) => out.push_str("deleted file\n"),
            _ => {}
        }
        let (before, after) = (before.map_or("", String::as_str), after.map_or("", String::as_str));
        let diff = similar::TextDiff::from_lines(before, after);
        out.push_str(&diff.unified_diff().context_radius(3).header(&a, &b).to_string());
    }
    out
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
