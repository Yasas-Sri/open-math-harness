//! Reading what git prints: `status --porcelain=v2 -z`, `diff --numstat -z`
//! and `-U0` patches, the last as a stream cut into pages.

use std::collections::BTreeMap;

use nucleus_diffs_interface::{FileSummary, Hunk, MAX_LINE_BYTES, PAGE_BYTES, Status};
use nucleus_protocol::{Error, ErrorCode};

use crate::git::Flow;
use crate::workspace::MAX_PATH_BYTES;

fn malformed(what: &str) -> Error {
    Error::new(ErrorCode::ServiceError, format!("git printed {what} this service cannot read"))
}

/// A path git printed, as the interface carries it. A name that is not UTF-8
/// is refused rather than shown altered or left out.
pub fn path_string(bytes: &[u8]) -> Result<String, Error> {
    if bytes.len() > MAX_PATH_BYTES {
        return Err(Error::new(
            ErrorCode::ServiceError,
            format!(
                "a changed file's path is over {MAX_PATH_BYTES} bytes: {}",
                String::from_utf8_lossy(bytes)
            ),
        ));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        Error::new(
            ErrorCode::ServiceError,
            format!(
                "the changed file {:?} has a name that is not UTF-8, which Review cannot show; rename it to review this repository",
                String::from_utf8_lossy(bytes)
            ),
        )
    })
}

/// One changed path `git status` reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// Changed between `HEAD` and the working tree (staged or not), with the
    /// blob it has in `HEAD`, if any.
    Tracked {
        path: String,
        status: Status,
        head: Option<String>,
    },
    Untracked {
        path: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    /// The commit `HEAD` names; `None` before the first commit.
    pub head: Option<String>,
    pub records: Vec<Record>,
}

fn zero(oid: &[u8]) -> bool {
    oid.iter().all(|b| *b == b'0')
}

fn text(bytes: &[u8]) -> Result<String, Error> {
    String::from_utf8(bytes.to_vec()).map_err(|_| malformed("an object name"))
}

/// A tracked path from its mode in `HEAD` and in the working tree (zero where
/// it has none) and its blob in `HEAD`.
fn tracked(m_head: &[u8], m_work: &[u8], h_head: &[u8], path: &[u8]) -> Result<Record, Error> {
    let status = if zero(m_head) {
        Status::Added
    } else if zero(m_work) {
        Status::Deleted
    } else {
        Status::Modified
    };
    let head = (!zero(h_head)).then(|| text(h_head)).transpose()?;
    Ok(Record::Tracked { path: path_string(path)?, status, head })
}

/// Reads `git status --porcelain=v2 -z --branch`. A nested repository (an
/// untracked directory, `dir/`) is left out.
pub fn parse_status(out: &[u8]) -> Result<StatusReport, Error> {
    let mut head = None;
    let mut saw_head = false;
    let mut records = Vec::new();
    let mut tokens = out.split(|b| *b == 0);
    while let Some(token) = tokens.next() {
        let fields = |n: usize| -> Result<Vec<&[u8]>, Error> {
            let parts: Vec<&[u8]> = token.splitn(n, |b| *b == b' ').collect();
            if parts.len() == n { Ok(parts) } else { Err(malformed("a status line")) }
        };
        match token.first() {
            None => {}
            Some(b'#') => {
                if let Some(oid) = token.strip_prefix(b"# branch.oid ") {
                    saw_head = true;
                    head = (oid != b"(initial)").then(|| text(oid)).transpose()?;
                }
            }
            Some(b'1' | b'2') => {
                // `1 XY sub mH mI mW hH hI path`; a `2` (a rename, which
                // `--no-renames` rules out) has a score first and its old path after.
                let rename = token[0] == b'2';
                let parts = fields(if rename { 10 } else { 9 })?;
                records.push(tracked(parts[3], parts[5], parts[6], parts[parts.len() - 1])?);
                if rename {
                    tokens.next();
                }
            }
            Some(b'u') => {
                // `u XY sub m1 m2 m3 mW h1 h2 h3 path`: in a conflict `HEAD`'s
                // side is stage 2, which is empty when `HEAD` lacks the path.
                let parts = fields(11)?;
                records.push(tracked(parts[4], parts[6], parts[8], parts[10])?);
            }
            Some(b'?') => {
                let path = token.strip_prefix(b"? ").ok_or_else(|| malformed("an untracked line"))?;
                if path.last() != Some(&b'/') {
                    records.push(Record::Untracked { path: path_string(path)? });
                }
            }
            Some(b'!') => {}
            Some(_) => return Err(malformed("a status line")),
        }
    }
    if !saw_head {
        return Err(malformed("a status without its branch.oid header"));
    }
    Ok(StatusReport { head, records })
}

/// Reads `git diff --numstat -z --no-renames`: added and deleted lines per
/// path, `None` for a binary file.
pub fn parse_numstat(out: &[u8]) -> Result<BTreeMap<String, Option<(u64, u64)>>, Error> {
    let mut counts = BTreeMap::new();
    for token in out.split(|b| *b == 0).filter(|t| !t.is_empty()) {
        let parts: Vec<&[u8]> = token.splitn(3, |b| *b == b'\t').collect();
        let [added, deleted, path] = parts[..] else { return Err(malformed("a numstat line")) };
        let count = |n: &[u8]| std::str::from_utf8(n).ok().and_then(|s| s.parse::<u64>().ok());
        let lines = match (added, deleted) {
            (b"-", b"-") => None,
            _ => Some(count(added).zip(count(deleted)).ok_or_else(|| malformed("a numstat count"))?),
        };
        counts.insert(path_string(path)?, lines);
    }
    Ok(counts)
}

/// Bytes `text` takes as a JSON string, quotes included, escaped the way
/// `serde_json` escapes it.
pub fn json_len(text: &str) -> usize {
    2 + text
        .chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum::<usize>()
}

/// The most a summary of `path` can take in a `changes` reply: every flag
/// set and the largest counts, so a page chosen by it never grows later.
pub fn summary_cost(path: &str) -> usize {
    let widest = FileSummary {
        path: String::new(),
        status: Status::Modified,
        additions: u64::MAX,
        deletions: u64::MAX,
        untracked: true,
        binary: true,
        too_large: true,
        version: "x".repeat(crate::workspace::MAX_VERSION_BYTES),
    };
    serde_json::to_vec(&widest).map_or(256, |v| v.len()) + json_len(path) + 1
}

fn hunk_cost() -> usize {
    let widest = Hunk { line: u64::MAX, deleted: Vec::new(), added: Vec::new() };
    serde_json::to_vec(&widest).map_or(64, |v| v.len()) + 1
}

/// What a `-U0` patch says, one line at a time.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A file's section starts, with its header (`diff --git a/x b/x`). A
    /// path whose type changed has two sections under the same header: the
    /// old file deleted, then the new one added.
    File { header: &'a [u8] },
    /// Git compared the file as binary: it has no lines.
    Binary,
    /// A hunk starts; its added lines start at `first_line` in the new file,
    /// and its deleted lines stood just before that line.
    Hunk { first_line: u64 },
    /// A changed line without its `-`/`+` and newline, or `None` when it is
    /// over [`MAX_LINE_BYTES`] (its bytes were not kept).
    Line { added: bool, text: Option<&'a [u8]> },
}

/// Splits a `-U0` patch into [`Event`]s as its bytes arrive. It holds at most
/// one line of [`MAX_LINE_BYTES`], whatever the patch's size. Lines in a hunk
/// are told apart by the counts in its header, never by their text.
#[derive(Default)]
pub struct Patch {
    line: Vec<u8>,
    overflow: bool,
    /// Deleted and added lines still expected in the current hunk.
    body: Option<(u64, u64)>,
}

impl Patch {
    pub fn feed(
        &mut self,
        mut chunk: &[u8],
        on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>,
    ) -> Result<Flow, Error> {
        while let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            self.take(&chunk[..end]);
            chunk = &chunk[end + 1..];
            if self.line_done(on)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        self.take(chunk);
        Ok(Flow::More)
    }

    /// Ends the stream: a last line without a newline is a line too.
    pub fn finish(&mut self, on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>) -> Result<(), Error> {
        if !self.line.is_empty() || self.overflow {
            self.line_done(on)?;
        }
        if self.body.is_some() {
            return Err(malformed("a hunk shorter than its header"));
        }
        Ok(())
    }

    fn take(&mut self, bytes: &[u8]) {
        if self.overflow || bytes.is_empty() {
            return;
        }
        // One byte more than the longest line: the `-` or `+` in front.
        if self.line.len() + bytes.len() > MAX_LINE_BYTES + 1 {
            if self.line.is_empty() {
                self.line.push(bytes[0]);
            }
            self.line.truncate(1);
            self.overflow = true;
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn line_done(&mut self, on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>) -> Result<Flow, Error> {
        let line = std::mem::take(&mut self.line);
        let overflow = std::mem::take(&mut self.overflow);
        if let Some((deleted, added)) = self.body {
            let (is_added, left) = match line.first() {
                Some(b'-') if deleted > 0 => (false, (deleted - 1, added)),
                Some(b'+') if added > 0 => (true, (deleted, added - 1)),
                // `\ No newline at end of file`.
                Some(b'\\') => return Ok(Flow::More),
                _ => return Err(malformed("a hunk line that does not match its header")),
            };
            self.body = (left != (0, 0)).then_some(left);
            let text = (!overflow).then(|| &line[1..]);
            return on(Event::Line { added: is_added, text });
        }
        if overflow {
            return Err(malformed(&format!("a patch header line over {MAX_LINE_BYTES} bytes")));
        }
        if line.starts_with(b"@@ ") {
            let (deleted, new_start, new_count) = hunk_header(&line)?;
            self.body = ((deleted, new_count) != (0, 0)).then_some((deleted, new_count));
            // Only deletions: they stood before the line after `new_start`.
            let first_line = if new_count == 0 { new_start + 1 } else { new_start };
            return on(Event::Hunk { first_line });
        }
        if line.starts_with(b"diff ") {
            return on(Event::File { header: &line });
        }
        if line.starts_with(b"Binary files ") {
            return on(Event::Binary);
        }
        // `index`, `---`, `+++`, mode lines, `\ No newline at end of file`.
        Ok(Flow::More)
    }
}

/// `@@ -a[,b] +c[,d] @@ ...` as (b, c, d); an omitted count is 1.
fn hunk_header(line: &[u8]) -> Result<(u64, u64, u64), Error> {
    let bad = || malformed("a hunk header");
    let text = std::str::from_utf8(line).ok();
    let mut words = text.ok_or_else(bad)?.split(' ');
    let (_, old, new) = (words.next(), words.next(), words.next());
    let range = |word: Option<&str>, sign: char| -> Result<(u64, u64), Error> {
        let word = word.and_then(|w| w.strip_prefix(sign)).ok_or_else(bad)?;
        let (start, count) = word.split_once(',').unwrap_or((word, "1"));
        Ok((start.parse().map_err(|_| bad())?, count.parse().map_err(|_| bad())?))
    };
    let (_, deleted) = range(old, '-')?;
    let (new_start, new_count) = range(new, '+')?;
    if words.next() != Some("@@") {
        return Err(bad());
    }
    Ok((deleted, new_start, new_count))
}

/// What `changes` needs of one file's patch: its added lines and whether it
/// can be shown. Stops at the first line over [`MAX_LINE_BYTES`].
#[derive(Default, Debug)]
pub struct Scan {
    pub additions: u64,
    pub binary: bool,
    pub too_large: bool,
}

impl Scan {
    pub fn on(&mut self, event: Event<'_>) -> Flow {
        match event {
            Event::Binary => self.binary = true,
            Event::Line { text: None, .. } => {
                self.too_large = true;
                return Flow::Stop;
            }
            Event::Line { added: true, .. } => self.additions += 1,
            Event::File { .. } | Event::Hunk { .. } | Event::Line { added: false, .. } => {}
        }
        Flow::More
    }
}

/// One page of a file's changed lines, built as the patch streams by.
///
/// The cursor (`after_line`, `next_line`) counts changed lines, deleted and
/// added, in the order the patch gives them. A page that starts inside a hunk
/// starts a new [`Hunk`] whose `line` is still exact: deleted lines do not
/// move the new file, each added line moves it by one.
pub struct Page {
    after: u64,
    seen: u64,
    bytes: usize,
    /// The header of the file's first section.
    header: Option<Vec<u8>>,
    hunk_cost: usize,
    /// Where the current patch hunk's added lines start in the new file.
    first_line: u64,
    added_seen: u64,
    /// The last page hunk belongs to the current patch hunk.
    open: bool,
    pub hunks: Vec<Hunk>,
    pub next_line: Option<u64>,
}

impl Page {
    pub fn new(after: u64) -> Self {
        Self {
            after,
            seen: 0,
            bytes: 0,
            header: None,
            hunk_cost: hunk_cost(),
            first_line: 1,
            added_seen: 0,
            open: false,
            hunks: Vec::new(),
            next_line: None,
        }
    }

    pub fn on(&mut self, event: Event<'_>) -> Result<Flow, Error> {
        let (added, text) = match event {
            // A second section is the same path's only under the same header
            // (its type changed); otherwise the path also named files beneath it.
            Event::File { header } => match &self.header {
                None => {
                    self.header = Some(header.to_vec());
                    return Ok(Flow::More);
                }
                Some(first) if first == header => return Ok(Flow::More),
                Some(_) => {
                    return Err(Error::new(
                        ErrorCode::InvalidInput,
                        "the path names more than one changed file",
                    ));
                }
            },
            Event::Binary => return Ok(Flow::More),
            Event::Hunk { first_line } => {
                self.first_line = first_line;
                self.added_seen = 0;
                self.open = false;
                return Ok(Flow::More);
            }
            Event::Line { added, text } => (added, text),
        };
        self.seen += 1;
        if self.seen <= self.after {
            self.added_seen += u64::from(added);
            return Ok(Flow::More);
        }
        let Some(text) = text else {
            return Err(Error::new(
                ErrorCode::ServiceError,
                format!(
                    "changed line {} is over {MAX_LINE_BYTES} bytes; the file is too large to show",
                    self.seen
                ),
            ));
        };
        let text = String::from_utf8_lossy(text).into_owned();
        // A deleted line after added ones starts a new hunk: deleted lines
        // always stand just before a hunk's `line`.
        let continues = self.open && (added || self.hunks.last().is_some_and(|h| h.added.is_empty()));
        let cost = json_len(&text) + 1 + if continues { 0 } else { self.hunk_cost };
        if !self.hunks.is_empty() && self.bytes + cost > PAGE_BYTES {
            self.next_line = Some(self.seen - 1);
            return Ok(Flow::Stop);
        }
        if !continues {
            let line = self.first_line + self.added_seen;
            self.hunks.push(Hunk { line, deleted: Vec::new(), added: Vec::new() });
            self.open = true;
        }
        if let Some(hunk) = self.hunks.last_mut() {
            if added {
                hunk.added.push(text);
            } else {
                hunk.deleted.push(text);
            }
        }
        self.added_seen += u64::from(added);
        self.bytes += cost;
        Ok(Flow::More)
    }
}
