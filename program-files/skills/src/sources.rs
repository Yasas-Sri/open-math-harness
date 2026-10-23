//! Where published skills come from: the catalogs the operator configured, an
//! `index.json` naming each skill and its files (a `.zip` of its folder, or
//! each file's URL), and any URL of a skill's `.zip` or `SKILL.md`. Every
//! fetch goes through the `web` service's checks (`nucleus_web`): each
//! redirect hop and resolved address admitted, a refusal of automated access
//! reported as one, and a size and a time limit. What arrives is untrusted:
//! a path is checked before it is written, an archive's entries are read
//! within limits, and a link in it is refused.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{Duration, Instant};

use nucleus_protocol::{Error, ErrorCode};
use nucleus_skills_interface::{SKILL_FILE, valid_name};
use nucleus_web::WebService;
use serde::Deserialize;

use crate::front_matter;
use crate::store::{Files, MAX_FILES, MAX_SKILL_BYTES, file_path};

/// How long a catalog read is used before it is read again, and how long a
/// catalog that could not be read is not asked again.
const CATALOG_KEPT: Duration = Duration::from_secs(600);
const FAILURE_KEPT: Duration = Duration::from_secs(60);
/// Largest catalog and file fetched.
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Kept back from the caller's deadline for the reply.
const MARGIN: Duration = Duration::from_millis(500);

fn refuse(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, message.into())
}

/// A catalog's skill, as its `index.json` names it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Entry {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    zip: Option<String>,
    #[serde(default)]
    files: Vec<IndexFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct IndexFile {
    path: String,
    url: String,
}

#[derive(Deserialize)]
struct Index {
    skills: Vec<Entry>,
}

/// A skill's files, fetched, with the name its source gives it.
pub struct Fetched {
    pub name: String,
    pub files: Files,
}

/// What reading a catalog gave: its skills, or why there are none.
type CatalogRead = Result<Vec<Entry>, String>;

pub struct Sources {
    web: WebService,
    catalogs: Vec<String>,
    read: RefCell<BTreeMap<String, (Instant, CatalogRead)>>,
}

impl Sources {
    pub fn new(catalogs: Vec<String>) -> Result<Self, String> {
        let mut options = nucleus_web::Options::parse(Vec::<String>::new())?;
        options.max_bytes = MAX_SKILL_BYTES;
        Ok(Self { web: WebService::new(&options), catalogs, read: RefCell::new(BTreeMap::new()) })
    }

    pub fn catalogs(&self) -> &[String] {
        &self.catalogs
    }

    /// One download, over https alone: a catalog's link and every redirect too, so that no
    /// one on the network path can swap what arrives.
    fn get(&self, url: &str, limit: u64, deadline: Instant) -> Result<nucleus_web::Fetched, Error> {
        if !url.starts_with("https://") {
            return Err(refuse(format!("{url} is not https: skills come over https alone")));
        }
        let left = deadline.saturating_duration_since(Instant::now()).saturating_sub(MARGIN);
        let fetched = self.web.get_bytes(url, limit, u64::try_from(left.as_millis()).unwrap_or(u64::MAX))?;
        if !fetched.url.starts_with("https://") {
            return Err(refuse(format!("{url} redirected to {}, which is not https", fetched.url)));
        }
        if !(200..300).contains(&fetched.status) {
            return Err(Error::new(ErrorCode::ServiceError, format!("{url} answered {}", fetched.status)));
        }
        if fetched.truncated {
            return Err(refuse(format!("{url} is larger than {} bytes", limit)));
        }
        Ok(fetched)
    }

    /// The skills a catalog lists, read at most every [`CATALOG_KEPT`].
    pub fn catalog(&self, url: &str, deadline: Instant) -> Result<Vec<Entry>, String> {
        if let Some((at, kept)) = self.read.borrow().get(url) {
            let keep = if kept.is_ok() { CATALOG_KEPT } else { FAILURE_KEPT };
            if at.elapsed() < keep {
                return kept.clone();
            }
        }
        let read = self
            .get(url, MAX_INDEX_BYTES, deadline)
            .map_err(|e| e.message)
            .and_then(|f| {
                serde_json::from_slice::<Index>(&f.bytes).map_err(|e| format!("not a skills index: {e}"))
            })
            .map(|index| index.skills.into_iter().filter(|s| valid_name(&s.name)).collect::<Vec<_>>());
        self.read.borrow_mut().insert(url.to_owned(), (Instant::now(), read.clone()));
        read
    }

    /// The first catalog that lists `name`, and its entry.
    pub fn find(&self, name: &str, deadline: Instant) -> Result<(String, Entry), Error> {
        let mut unreachable = Vec::new();
        for catalog in &self.catalogs {
            match self.catalog(catalog, deadline) {
                Ok(entries) => {
                    if let Some(entry) = entries.into_iter().find(|e| e.name == name) {
                        return Ok((catalog.clone(), entry));
                    }
                }
                Err(why) => unreachable.push(format!("{catalog}: {why}")),
            }
        }
        let mut message = format!("this project has no skill {name}, and no catalog lists one");
        if !unreachable.is_empty() {
            message.push_str(&format!(" (not read: {})", unreachable.join("; ")));
        }
        Err(refuse(message))
    }

    /// A catalog's skill: its archive, or its files one by one.
    pub fn fetch_entry(&self, catalog: &str, entry: &Entry, deadline: Instant) -> Result<Fetched, Error> {
        if let Some(zip) = &entry.zip {
            let fetched = self.get(&join(catalog, zip), MAX_SKILL_BYTES, deadline)?;
            let (_, files) = unzip(&fetched.bytes)?;
            return Ok(Fetched { name: entry.name.clone(), files });
        }
        if entry.files.len() > MAX_FILES {
            return Err(refuse(format!("the skill {} has more than {MAX_FILES} files", entry.name)));
        }
        let mut files = Vec::new();
        let mut total = 0;
        for file in &entry.files {
            let path = file_path(&file.path)?;
            let fetched = self.get(&join(catalog, &file.url), MAX_FILE_BYTES, deadline)?;
            total += fetched.bytes.len() as u64;
            if total > MAX_SKILL_BYTES {
                return Err(refuse(format!(
                    "the skill {} is larger than {MAX_SKILL_BYTES} bytes",
                    entry.name
                )));
            }
            files.push((path, fetched.bytes));
        }
        if !files.iter().any(|(path, _)| path == SKILL_FILE) {
            return Err(refuse(format!("the catalog lists no {SKILL_FILE} for {}", entry.name)));
        }
        Ok(Fetched { name: entry.name.clone(), files })
    }

    /// The skill at `url`: a `.zip` of its folder, or its `SKILL.md`. Its
    /// name is its front matter's, or else its folder's.
    pub fn fetch_url(&self, url: &str, deadline: Instant) -> Result<Fetched, Error> {
        if !url.starts_with("https://") {
            return Err(refuse("a skill's URL must be https"));
        }
        let fetched = self.get(url, MAX_SKILL_BYTES, deadline)?;
        let (folder, files) = if fetched.bytes.starts_with(b"PK\x03\x04") {
            unzip(&fetched.bytes)?
        } else if fetched.content_type.contains("html") {
            return Err(refuse(format!("{url} is a web page, not a skill's .zip or {SKILL_FILE}")));
        } else {
            std::str::from_utf8(&fetched.bytes)
                .map_err(|_| refuse(format!("{url} is neither a .zip nor text")))?;
            (None, vec![(SKILL_FILE.to_owned(), fetched.bytes)])
        };
        let instructions =
            files.iter().find(|(path, _)| path == SKILL_FILE).map(|(_, b)| String::from_utf8_lossy(b));
        let named = instructions.and_then(|text| front_matter::parse(&text).name).filter(|n| valid_name(n));
        let name = named
            .or_else(|| folder.filter(|f| valid_name(f)))
            .or_else(|| name_in_url(&fetched.url))
            .ok_or_else(|| {
            refuse(format!("{url} gives the skill no name: its {SKILL_FILE} needs a `name`"))
        })?;
        Ok(Fetched { name, files })
    }
}

/// The name a URL's path suggests: the archive's stem, or the folder of a
/// `SKILL.md`.
fn name_in_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let mut segments = path.rsplit('/').filter(|s| !s.is_empty());
    let last = segments.next()?;
    let name = if last.eq_ignore_ascii_case(SKILL_FILE) {
        segments.next()?
    } else {
        last.strip_suffix(".zip").unwrap_or(last)
    };
    Some(name.to_owned()).filter(|n| valid_name(n))
}

/// `href` against the catalog's URL: absolute as it is, else beside it.
fn join(base: &str, href: &str) -> String {
    if href.starts_with("https://") || href.starts_with("http://") {
        return href.to_owned();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let authority = rest.split('/').next().unwrap_or(rest);
    if let Some(absolute) = href.strip_prefix('/') {
        return format!("{scheme}://{authority}/{absolute}");
    }
    let dir = base.rfind('/').map_or(base, |i| &base[..=i]);
    format!("{dir}{href}")
}

/// A skill's folder from a `.zip`: its files, from its top, or from the one
/// folder that holds everything, which then names it. Links, absolute and
/// escaping paths are refused; sizes are checked on what is read, not on
/// what the archive claims.
pub fn unzip(bytes: &[u8]) -> Result<(Option<String>, Files), Error> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| refuse(format!("not a readable .zip: {e}")))?;
    if archive.len() > 2 * MAX_FILES {
        return Err(refuse(format!("the archive has more than {} entries", 2 * MAX_FILES)));
    }
    let mut files = Vec::new();
    let mut total = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| refuse(format!("the archive: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        if entry.is_symlink() {
            return Err(refuse(format!("the archive holds a link, {}", entry.name())));
        }
        let path = entry
            .enclosed_name()
            .ok_or_else(|| refuse(format!("the archive's path {:?} leaves it", entry.name())))?;
        let path = path.to_string_lossy().replace('\\', "/");
        if path.starts_with("__MACOSX/") || path.ends_with(".DS_Store") {
            continue;
        }
        let mut content = Vec::new();
        entry
            .by_ref()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut content)
            .map_err(|e| refuse(format!("{path}: {e}")))?;
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(refuse(format!("{path} is larger than {MAX_FILE_BYTES} bytes")));
        }
        total += content.len() as u64;
        if total > MAX_SKILL_BYTES || files.len() >= MAX_FILES {
            return Err(refuse(format!(
                "the skill is larger than {MAX_SKILL_BYTES} bytes or {MAX_FILES} files"
            )));
        }
        files.push((path, content));
    }
    let top = |path: &str| path.split_once('/').map(|(top, _)| top.to_owned());
    let folder = match files.first().and_then(|(path, _)| top(path)) {
        Some(folder) if files.iter().all(|(path, _)| top(path).as_deref() == Some(folder.as_str())) => {
            Some(folder)
        }
        _ => None,
    };
    let files: Files = match &folder {
        Some(folder) => {
            files.into_iter().map(|(path, content)| (path[folder.len() + 1..].to_owned(), content)).collect()
        }
        None => files,
    };
    if !files.iter().any(|(path, _)| path == SKILL_FILE) {
        return Err(refuse(format!("the archive has no {SKILL_FILE} at its top or in its one folder")));
    }
    let files = files
        .into_iter()
        .map(|(path, content)| Ok((file_path(&path)?, content)))
        .collect::<Result<_, Error>>()?;
    Ok((folder, files))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        for (path, content) in entries {
            zip.start_file(*path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn an_archive_gives_its_folders_files() {
        let zip = archive(&[("proof/SKILL.md", b"---\nname: proof\n---\n"), ("proof/refs/a.md", b"a")]);
        let (folder, files) = unzip(&zip).unwrap();
        assert_eq!(folder.as_deref(), Some("proof"));
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["SKILL.md", "refs/a.md"]);
        let flat = archive(&[("SKILL.md", b"x"), ("b.md", b"b")]);
        assert_eq!(unzip(&flat).unwrap().0, None);
        assert!(
            unzip(&archive(&[("a/SKILL.md", b"x"), ("b/c.md", b"c")])).is_err(),
            "two folders, no top SKILL.md"
        );
        assert!(unzip(&archive(&[("../SKILL.md", b"x")])).is_err());
        assert!(unzip(b"PK\x03\x04 not really").is_err());
    }

    #[test]
    fn urls_name_and_join() {
        assert_eq!(name_in_url("https://x.org/s/lean-workflow.zip").as_deref(), Some("lean-workflow"));
        assert_eq!(
            name_in_url("https://raw.example/o/r/main/skills/proof/SKILL.md?x=1").as_deref(),
            Some("proof")
        );
        assert_eq!(join("https://s.org/index.json", "a/SKILL.md"), "https://s.org/a/SKILL.md");
        assert_eq!(join("https://s.org/c/index.json", "/a.zip"), "https://s.org/a.zip");
        assert_eq!(join("https://s.org/index.json", "https://t.org/a.zip"), "https://t.org/a.zip");
    }
}
