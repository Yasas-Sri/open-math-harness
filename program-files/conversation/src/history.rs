//! A page of a session's log without reading the rest of it (`history`).
//!
//! A reader pages through a long conversation with `after_seq`. Reading and
//! parsing the whole log for every page makes that quadratic in the log's
//! size. So the service keeps, for the logs read lately, an index: where
//! every [`MARK_EVERY`]-th record starts, and which answers and reasonings
//! are final (their pieces are no longer history). A page then starts
//! reading at the mark before its first record and stops when it is full.
//!
//! The index is derived from the log and never the other way round. This
//! service is the log's only writer and only appends, so an index is extended
//! by what the log grew; a log that is another file, shorter than it was,
//! that ended in the middle of a line, or whose last indexed bytes are no
//! longer what they were is indexed anew. A log whose `seq`s do
//! not grow in file order (nothing this service writes) is read whole.
//! Nothing here is kept across a restart.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nucleus_conversation_interface::HistoryResponse;
use nucleus_protocol::{Error, ErrorCode, ResourcePath};
use serde::Deserialize;
use serde_json::Value;

use crate::{LOG_FORMAT_VERSION, failed, history_page};

/// One of every this many records has its place in the file remembered: a
/// page parses at most that many records before its first.
const MARK_EVERY: u64 = 64;
/// Bytes at the end of what an index covers that it remembers.
const TAIL: usize = 64;
/// Logs whose index is kept; the one read longest ago goes first.
const MAX_INDEXED: usize = 16;

/// What the index needs of a record, read without building the record.
#[derive(Deserialize)]
struct Head {
    v: Option<Value>,
    seq: Option<Value>,
    kind: Option<Value>,
    message_id: Option<Value>,
}

/// A record's `v`, `seq`, `kind` and `message_id`.
type Named = (Option<u64>, Option<u64>, Option<String>, Option<String>);

/// A line's `v`, `seq`, `kind` and `message_id`, exactly as the record built
/// from it has them; `None` for a line that is no JSON, which is no record.
fn head(line: &str) -> Option<Named> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    // The common case, an object with each key once, without building it.
    if line.trim_start().starts_with('{')
        && let Ok(head) = serde_json::from_str::<Head>(line)
    {
        let number = |value: &Option<Value>| value.as_ref().and_then(Value::as_u64);
        let (kind, id) = (head.kind.as_ref().and_then(text), head.message_id.as_ref().and_then(text));
        return Some((number(&head.v), number(&head.seq), kind, id));
    }
    let record = serde_json::from_str::<Value>(line).ok()?;
    Some((record["v"].as_u64(), record["seq"].as_u64(), text(&record["kind"]), text(&record["message_id"])))
}

fn newer_format() -> Error {
    Error::new(
        ErrorCode::ServiceError,
        "the session log was written by a newer format version; refusing to touch it",
    )
}

/// One log's index.
#[derive(Default)]
struct Index {
    /// The file it is of: device and inode.
    file: (u64, u64),
    /// Bytes of the log it covers.
    covered: u64,
    /// The last line covered had no end yet.
    unterminated: bool,
    /// The last bytes covered, at most [`TAIL`]: a log that does not hold
    /// them there any more was rewritten, not appended to.
    tail: Vec<u8>,
    /// `(seq, offset of its line)` of every [`MARK_EVERY`]-th record with a `seq`.
    marks: Vec<(u64, u64)>,
    numbered: u64,
    last_seq: Option<u64>,
    /// Every `seq` so far is larger than the one before it in the file.
    ordered: bool,
    finished: BTreeSet<String>,
    finished_reasoning: BTreeSet<String>,
}

/// The lines of a log from an offset on, as `BufRead::lines` gives them:
/// without their line end, and a line that is not UTF-8 is an error.
struct Lines {
    reader: BufReader<File>,
    /// Where the next line starts.
    offset: u64,
    /// Bytes read so far: what a page cost.
    read: u64,
}

impl Lines {
    fn from(mut file: File, offset: u64) -> Result<Self, Error> {
        file.seek(SeekFrom::Start(offset)).map_err(|e| failed("read log", &e))?;
        Ok(Self { reader: BufReader::new(file), offset, read: 0 })
    }

    /// The next line, where it started, and whether it was ended.
    fn next(&mut self) -> Result<Option<(String, u64, bool)>, Error> {
        let mut line = Vec::new();
        let length = self.reader.read_until(b'\n', &mut line).map_err(|e| failed("read log", &e))?;
        if length == 0 {
            return Ok(None);
        }
        let (start, ended) = (self.offset, line.ends_with(b"\n"));
        self.offset += length as u64;
        self.read += length as u64;
        if ended {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        let line = String::from_utf8(line).map_err(|e| failed("read log", &e))?;
        Ok(Some((line, start, ended)))
    }
}

impl Index {
    /// Brings the index up to the log's end. `None`: there is no log.
    fn extend(&mut self, path: &Path) -> Result<Option<File>, Error> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(failed("open log", &error)),
        };
        let meta = file.metadata().map_err(|e| failed("open log", &e))?;
        let grew_past_an_open_line = self.unterminated && meta.len() != self.covered;
        let same = (meta.dev(), meta.ino()) == self.file
            && meta.len() >= self.covered
            && !grew_past_an_open_line
            && self.tail_is_there(&file)?;
        if !same {
            *self = Self { file: (meta.dev(), meta.ino()), ordered: true, ..Self::default() };
        }
        let again = file.try_clone().map_err(|e| failed("open log", &e))?;
        let mut lines = Lines::from(again, self.covered)?;
        while let Some((line, start, ended)) = lines.next()? {
            self.covered = lines.offset;
            self.unterminated = !ended;
            let kept = line.len().saturating_sub(TAIL);
            self.tail = line.as_bytes()[kept..].to_vec();
            let Some((v, seq, kind, id)) = head(&line) else { continue };
            if v.is_some_and(|v| v > LOG_FORMAT_VERSION) {
                return Err(newer_format());
            }
            if let Some(seq) = seq {
                self.ordered &= self.last_seq.is_none_or(|last| seq > last);
                if self.numbered.is_multiple_of(MARK_EVERY) {
                    self.marks.push((seq, start));
                }
                self.numbered += 1;
                self.last_seq = Some(seq);
            }
            match (kind.as_deref(), id) {
                (Some("assistant"), Some(id)) => drop(self.finished.insert(id)),
                (Some("reasoning"), Some(id)) => drop(self.finished_reasoning.insert(id)),
                _ => {}
            }
        }
        Ok(Some(file))
    }

    /// Whether the log still holds, where this index ends, what it held.
    fn tail_is_there(&self, file: &File) -> Result<bool, Error> {
        use std::os::unix::fs::FileExt;
        // The tail is of the last line, without its end.
        let end = self.covered - u64::from(!self.unterminated && self.covered > 0);
        let Some(start) = end.checked_sub(self.tail.len() as u64) else { return Ok(false) };
        let mut there = vec![0; self.tail.len()];
        match file.read_exact_at(&mut there, start) {
            Ok(()) => Ok(there == self.tail),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(error) => Err(failed("read log", &error)),
        }
    }

    /// Whether `history` returns `record` to a reader that has everything up
    /// to `after`: not a piece of an answer or a reasoning that is final (the
    /// answer of record replaced it), and after `after`.
    fn returned(&self, record: &Value, after: Option<u64>) -> bool {
        let piece_of = |finished: &BTreeSet<String>| {
            record["message_id"].as_str().is_some_and(|id| finished.contains(id))
        };
        (record["kind"] != "assistant_delta" || !piece_of(&self.finished))
            && (record["kind"] != "reasoning_delta" || !piece_of(&self.finished_reasoning))
            && after.is_none_or(|after| record["seq"].as_u64().is_some_and(|seq| seq > after))
    }

    /// One page of the records after `after`, and the bytes read for it.
    fn page(&self, file: File, after: Option<u64>) -> Result<(HistoryResponse, u64), Error> {
        // Without an order of the `seq`s in the file, a record after `after`
        // may be anywhere: the whole log is read.
        let start = match after.filter(|_| self.ordered) {
            None => 0,
            Some(after) => {
                let before = self.marks.partition_point(|(seq, _)| *seq <= after);
                before.checked_sub(1).map_or(0, |mark| self.marks[mark].1)
            }
        };
        let mut lines = Lines::from(file, start)?;
        let mut stopped = None;
        let records = std::iter::from_fn(|| {
            loop {
                let line = match lines.next() {
                    Ok(Some((line, _, _))) => line,
                    Ok(None) => return None,
                    Err(error) => {
                        stopped = Some(error);
                        return None;
                    }
                };
                // A line that is no JSON is no record, as when the log is read whole.
                let Ok(record) = serde_json::from_str::<Value>(&line) else { continue };
                if record["v"].as_u64().is_some_and(|v| v > LOG_FORMAT_VERSION) {
                    stopped = Some(newer_format());
                    return None;
                }
                if self.returned(&record, after) {
                    return Some(record);
                }
            }
        });
        let page = history_page(records)?;
        match stopped {
            Some(error) => Err(error),
            None => Ok((page, lines.read)),
        }
    }
}

/// The indexes of the logs read lately, most recent first.
#[derive(Default)]
pub struct Histories {
    indexes: Vec<(ResourcePath, Index)>,
    /// Bytes the last page read from its log, beyond what extended the index.
    #[cfg(test)]
    pub(crate) last_page_read: u64,
}

impl Histories {
    /// Drops the indexes of the sessions `gone` names.
    pub fn forget(&mut self, gone: &impl Fn(&ResourcePath) -> bool) {
        self.indexes.retain(|(resource, _)| !gone(resource));
    }

    /// The page of `history` for the session `resource`, whose log is at `path`.
    pub fn page(
        &mut self,
        resource: &ResourcePath,
        path: &Path,
        after: Option<u64>,
    ) -> Result<HistoryResponse, Error> {
        let at = self.indexes.iter().position(|(indexed, _)| indexed == resource);
        let (_, mut index) = match at {
            Some(at) => self.indexes.remove(at),
            None => (resource.clone(), Index::default()),
        };
        // An index that failed half-way is not kept: the next call starts anew.
        let Some(file) = index.extend(path)? else {
            return Ok(HistoryResponse { records: Vec::new(), has_more: false });
        };
        let (page, _read) = index.page(file, after)?;
        #[cfg(test)]
        {
            self.last_page_read = _read;
        }
        self.indexes.insert(0, (resource.clone(), index));
        self.indexes.truncate(MAX_INDEXED);
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::read_log;
    use nucleus_conversation_interface::ACTION_POST;
    use serde_json::json;

    /// What a reader gets from `after` on, page after page, each page from `page`.
    fn read(page: &mut dyn FnMut(Option<u64>) -> HistoryResponse, mut after: Option<u64>) -> Vec<Value> {
        let mut records = Vec::new();
        loop {
            let page = page(after);
            let next = page.continuation(after).unwrap();
            records.extend(page.records);
            match next {
                Some(seq) => after = Some(seq),
                None => return records,
            }
        }
    }

    /// Through the index.
    fn paged(histories: &mut Histories, path: &Path, after: Option<u64>) -> Vec<Value> {
        let resource = ResourcePath::new(["default", "default"]).unwrap();
        read(&mut |after| histories.page(&resource, path, after).unwrap(), after)
    }

    /// The same reader, each page made from the whole log.
    fn expected(path: &Path, after: Option<u64>) -> Vec<Value> {
        read(&mut |after| history_page(whole(path, after).into_iter()).unwrap(), after)
    }

    /// The rule a page must keep, said once more without the index: the
    /// whole log, without the pieces of what is final, after `after`.
    fn whole(path: &Path, after: Option<u64>) -> Vec<Value> {
        let log = read_log(path).unwrap();
        let final_of = |kind: &str| -> BTreeSet<String> {
            let ids = log.iter().filter(|r| r["kind"] == kind);
            ids.filter_map(|r| r["message_id"].as_str().map(str::to_owned)).collect()
        };
        let (answers, reasonings) = (final_of("assistant"), final_of("reasoning"));
        let piece = |r: &Value, of: &str, finals: &BTreeSet<String>| {
            r["kind"] == of && r["message_id"].as_str().is_some_and(|id| finals.contains(id))
        };
        log.iter()
            .filter(|r| !piece(r, "assistant_delta", &answers) && !piece(r, "reasoning_delta", &reasonings))
            .filter(|r| after.is_none_or(|after| r["seq"].as_u64().is_some_and(|seq| seq > after)))
            .cloned()
            .collect()
    }

    /// A long conversation's lines: turns of a message, pieces of an answer
    /// and of its reasoning, and the answer of record for every second one.
    fn turns(from: u64, count: u64) -> String {
        let mut log = String::new();
        for turn in 0..count {
            let seq = from + turn * 6;
            let id = format!("m{}", from + turn);
            let text = "x".repeat(300);
            let mut lines = vec![
                json!({"v": 1, "seq": seq, "kind": "user", "text": text}),
                json!({"v": 1, "seq": seq + 1, "kind": "reasoning_delta", "message_id": id, "text": text}),
                json!({"v": 1, "seq": seq + 2, "kind": "assistant_delta", "message_id": id, "text": text}),
            ];
            if turn % 2 == 0 {
                lines.push(
                    json!({"v": 1, "seq": seq + 3, "kind": "reasoning", "message_id": id, "text": text}),
                );
                lines.push(
                    json!({"v": 1, "seq": seq + 4, "kind": "assistant", "message_id": id, "text": text}),
                );
            }
            lines.push(json!({"v": 1, "seq": seq + 5, "kind": "turn_complete"}));
            lines.iter().for_each(|line| log.push_str(&format!("{line}\n")));
        }
        log
    }

    /// Paging through a log returns what reading it whole returns, from
    /// wherever a reader is, while the log grows, and a page costs what it
    /// holds and a little before it, not the log: the cost of a whole
    /// conversation is its size, not its size times its pages.
    #[test]
    fn a_page_is_what_the_whole_log_gives_and_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("default/default");
        std::fs::create_dir_all(&session).unwrap();
        let path = session.join("log.jsonl");
        let mut log = turns(0, 400);
        log.push_str("not a record\n{\"kind\":\"legacy\",\"text\":\"no seq\"}\n[1, 2]\n");
        std::fs::write(&path, &log).unwrap();
        let mut histories = Histories::default();
        let all = paged(&mut histories, &path, None);
        assert_eq!(all, expected(&path, None));
        assert!(all.len() > 3 * 500, "several pages: {}", all.len());
        for after in (0..2400).step_by(97) {
            assert_eq!(paged(&mut histories, &path, Some(after)), expected(&path, Some(after)), "{after}");
        }
        // The last records of a long log are read without the log before them.
        let tail = paged(&mut histories, &path, Some(2390));
        assert_eq!(tail.len(), 7, "{tail:?}");
        assert!(
            histories.last_page_read < log.len() as u64 / 20,
            "{} of {}",
            histories.last_page_read,
            log.len()
        );
        // The log grows through the service: the index follows, and a piece
        // whose answer of record arrives is history no more.
        let mut bench = crate::tests::Bench::new(dir.path());
        assert_eq!(bench.history(Some(2390)), tail, "the service pages the same log the same way");
        let late = json!({"kind": "assistant", "message_id": "m399", "text": "the answer of record"});
        bench.call(ACTION_POST, &late);
        let grown = bench.history(Some(2390));
        assert_eq!(grown, expected(&path, Some(2390)));
        assert!(grown.iter().all(|r| r["kind"] != "assistant_delta") && grown.len() == tail.len());
        drop(bench);
        // A log that is another file, or shorter, is indexed anew.
        std::fs::write(&path, turns(0, 3)).unwrap();
        assert_eq!(paged(&mut histories, &path, Some(7)), expected(&path, Some(7)));
        // Numbers that do not grow in the file (nothing this service writes):
        // every record after the reader's is still found.
        let unordered = format!("{}{}", turns(600, 2), turns(0, 100));
        std::fs::write(&path, unordered).unwrap();
        let resource = ResourcePath::new(["default", "default"]).unwrap();
        for after in [None, Some(3), Some(300), Some(601)] {
            let page = histories.page(&resource, &path, after).unwrap();
            assert_eq!(page, history_page(whole(&path, after).into_iter()).unwrap(), "{after:?}");
            assert!(after.is_none() || !page.records.is_empty());
        }
        // A log of a newer format is refused, page after page, and no log is no history.
        std::fs::write(&path, format!("{}{}\n", turns(0, 2), json!({"v": 2, "seq": 99, "kind": "future"})))
            .unwrap();
        for _ in 0..2 {
            assert_eq!(histories.page(&resource, &path, None).unwrap_err().code, ErrorCode::ServiceError);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(histories.page(&resource, &path, None).unwrap().records.is_empty());
    }
}
