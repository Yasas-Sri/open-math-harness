//! An untracked file as `git diff` shows a new one: every line added. This
//! service reads it itself, through [`crate::workspace::open`], so no git
//! runs per file and a symbolic link is shown as git stores it, by its
//! target, never followed. At most one line of [`MAX_LINE_BYTES`] and one
//! chunk are held, whatever the file's size.

use std::io::Read;
use std::time::Instant;

use nucleus_diffs_interface::MAX_LINE_BYTES;
use nucleus_protocol::{Error, ErrorCode};

use crate::diff::Event;
use crate::git::Flow;
use crate::workspace::Content;

/// Git's test for binary: a NUL byte in this many first bytes.
const FIRST_FEW_BYTES: u64 = 8000;
/// Git takes a bigger file as binary without reading it (its default
/// `core.bigFileThreshold`).
const BIG_FILE_BYTES: u64 = 512 << 20;
const CHUNK: usize = 64 * 1024;

/// Streams `content`, a file or a link (anything else is refused), into `on`:
/// [`Event::Binary`], or a hunk at line 1 and each line added. Past
/// `deadline` it is a timeout.
pub fn lines(
    content: Content,
    deadline: Instant,
    on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>,
) -> Result<(), Error> {
    let mut split = Split::default();
    match content {
        Content::Link(target) => {
            if split.feed(&target, on)? == Flow::Stop {
                return Ok(());
            }
        }
        Content::File(file, size) => {
            if size > BIG_FILE_BYTES {
                on(Event::Binary)?;
                return Ok(());
            }
            let failed =
                |e: std::io::Error| Error::new(ErrorCode::ServiceError, format!("reading the file: {e}"));
            let mut first = Vec::new();
            let mut file = file.take(FIRST_FEW_BYTES);
            file.read_to_end(&mut first).map_err(failed)?;
            if first.contains(&0) {
                on(Event::Binary)?;
                return Ok(());
            }
            if split.feed(&first, on)? == Flow::Stop {
                return Ok(());
            }
            let mut file = file.into_inner();
            let mut chunk = vec![0u8; CHUNK];
            loop {
                if Instant::now() >= deadline {
                    return Err(Error::new(ErrorCode::Timeout, "reading the file did not finish in time"));
                }
                let read = match file.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(failed(e)),
                };
                if split.feed(&chunk[..read], on)? == Flow::Stop {
                    return Ok(());
                }
            }
        }
        Content::Other => return Err(Error::new(ErrorCode::InvalidInput, "the path is not a regular file")),
    }
    split.finish(on)
}

/// Cuts bytes into lines on `\n` as they arrive; a last line without a
/// newline is a line too.
#[derive(Default)]
struct Split {
    line: Vec<u8>,
    overflow: bool,
    started: bool,
}

impl Split {
    fn feed(
        &mut self,
        mut bytes: &[u8],
        on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>,
    ) -> Result<Flow, Error> {
        while let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            self.take(&bytes[..end]);
            bytes = &bytes[end + 1..];
            if self.line_done(on)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        self.take(bytes);
        Ok(Flow::More)
    }

    fn finish(&mut self, on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>) -> Result<(), Error> {
        if !self.line.is_empty() || self.overflow {
            self.line_done(on)?;
        }
        Ok(())
    }

    fn take(&mut self, bytes: &[u8]) {
        if self.overflow {
            return;
        }
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            self.line.clear();
            self.overflow = true;
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn line_done(&mut self, on: &mut dyn FnMut(Event<'_>) -> Result<Flow, Error>) -> Result<Flow, Error> {
        if !self.started {
            self.started = true;
            if on(Event::Hunk { first_line: 1 })? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        let line = std::mem::take(&mut self.line);
        let overflow = std::mem::take(&mut self.overflow);
        on(Event::Line { added: true, text: (!overflow).then_some(&line[..]) })
    }
}
