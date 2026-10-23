//! Images in this agent's replies (conversation interface, "Images in
//! replies"): the agent shows the person an image of its project by writing
//! a Markdown image in its reply, `![ALT](DEST)` or `![ALT](DEST "TITLE")`.
//!
//! Before a reply is posted as an `assistant` record, [`read`] finds its image
//! references ([`references`], outside fenced code blocks and inline code
//! spans, the syntax the apps share) and reads each DEST the interface takes
//! as a path (`valid_reply_image_path`), once, as this agent reads every file
//! of its working directory (module `workdir`: `openat2` beneath it with no
//! symbolic link followed, a regular file, its type by its bytes). The record
//! carries at most 8 images of at most 256 KiB; each reference it cannot show
//! is named with a reason, and the model reads [`note`] on them after the
//! response that wrote them, before its next request (module `restore`
//! rebuilds the note from the record). The model is not shown the images.
//! The files are small and opened non-blocking, read on the agent's own
//! thread between two model calls.
//!
//! Which images a reply carries is the agent's choice, so text that steers
//! the agent (a page, a paper, a tool's output) can make it show any image of
//! the working directory, where every member, every agent of the project and
//! every command of the projects' user can put one. The record goes wherever
//! the session's records go, to the public of a public project or a chat
//! link included, and the secret scan does not read images: what an image
//! shows is public with its record unless that record is blocked, held, or
//! has a finding in its text that was not released.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use base64::Engine;
use nucleus_conversation_interface::{
    MAX_IMAGE_ALT_BYTES, MAX_REPLY_IMAGE_BYTES, MAX_REPLY_IMAGES, RefusedImage, RefusedReason, ReplyImage,
    valid_reply_image_path,
};
use nucleus_model_interface::image_media_type;
use rustix::io::Errno;

use crate::workdir::{Unopened, Workdir, read_all};

/// The line of the agent's instructions on showing an image.
pub(crate) const INSTRUCTION: &str = "To show the person an image from the project, write it in your \
    reply as Markdown, `![caption](relative/path.png)` with its path relative to your working directory: a \
    PNG, JPEG, WebP or GIF of at most 256 KiB. Pillow is available in Python to shrink one.";

/// How [`note`] begins.
pub(crate) const NOTE: &str = "[Images your reply referenced that the person was not shown; shrink or fix \
    one and show it again if it matters:";

/// What a reply's text shows the person (module doc): the images read and
/// the references not shown, each DEST once, in the order the text has them.
pub(crate) fn read(workdir: &Workdir, text: &str) -> (Vec<ReplyImage>, Vec<RefusedImage>) {
    let (mut images, mut refused) = (Vec::new(), Vec::new());
    let mut seen = BTreeSet::new();
    for dest in references(text) {
        if refused.len() == MAX_REPLY_IMAGES {
            break;
        }
        if !valid_reply_image_path(dest) || !seen.insert(dest) {
            continue;
        }
        let shown = if images.len() == MAX_REPLY_IMAGES {
            Err((RefusedReason::TooMany, None))
        } else {
            image(workdir, dest)
        };
        match shown {
            Ok((media_type, data_base64)) => {
                images.push(ReplyImage {
                    path: dest.to_owned(),
                    media_type: media_type.to_owned(),
                    data_base64,
                });
            }
            Err((reason, bytes)) => refused.push(RefusedImage { path: dest.to_owned(), reason, bytes }),
        }
    }
    (images, refused)
}

/// The image at `dest`, a path of the working directory: its media type and
/// base64, or why it is not shown and, when too large, its size.
fn image(workdir: &Workdir, dest: &str) -> Result<(&'static str, String), (RefusedReason, Option<u64>)> {
    if dest.split('/').all(|part| part.is_empty() || part == ".") {
        return Err((RefusedReason::NotAFile, None));
    }
    // Only a name longer than a file system takes fails here.
    let relative = workdir.relative(dest).map_err(|_| (RefusedReason::NotFound, None))?;
    let (file, size) = workdir.open_regular(&relative).map_err(|why| {
        let reason = match why {
            Unopened::Open(Errno::NOENT | Errno::NOTDIR) => RefusedReason::NotFound,
            Unopened::Open(Errno::LOOP | Errno::XDEV) => RefusedReason::OutsideProject,
            Unopened::NotRegular => RefusedReason::NotAFile,
            Unopened::Open(_) | Unopened::Stat(_) => RefusedReason::Unreadable,
        };
        (reason, None)
    })?;
    if size > MAX_REPLY_IMAGE_BYTES as u64 {
        return Err((RefusedReason::TooLarge, Some(size)));
    }
    let bytes = read_all(file, size, MAX_REPLY_IMAGE_BYTES).map_err(|_| (RefusedReason::Unreadable, None))?;
    let media_type = image_media_type(&bytes).ok_or((RefusedReason::NotAnImage, None))?;
    Ok((media_type, base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// What the model reads, after the response that wrote them, of the
/// references it could not show; none when it showed them all.
pub(crate) fn note(refused: &[RefusedImage]) -> Option<String> {
    if refused.is_empty() {
        return None;
    }
    let lines: Vec<String> =
        refused.iter().map(|refused| format!("- {}: {}", refused.path, why(refused))).collect();
    Some(format!("{NOTE}\n{}]", lines.join("\n")))
}

fn why(refused: &RefusedImage) -> String {
    match refused.reason {
        RefusedReason::NotFound => "not_found, nothing is at that path in your working directory".to_owned(),
        RefusedReason::NotAFile => "not_a_file".to_owned(),
        RefusedReason::TooLarge => format!(
            "too_large, {} bytes, over {} KiB",
            refused.bytes.unwrap_or_default(),
            MAX_REPLY_IMAGE_BYTES / 1024
        ),
        RefusedReason::NotAnImage => "not_an_image, it is no PNG, JPEG, WebP or GIF".to_owned(),
        RefusedReason::OutsideProject => "outside_project, a symbolic link is in its path".to_owned(),
        RefusedReason::TooMany => format!("too_many, a reply shows at most {MAX_REPLY_IMAGES} images"),
        RefusedReason::Unreadable => "unreadable".to_owned(),
    }
}

/// The DEST of every image reference in `text`, in order (conversation
/// interface, module doc "Images in replies"): outside fenced code blocks,
/// and in each paragraph (lines up to a blank one or a fence) outside its
/// inline code spans.
pub(crate) fn references(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut paragraph: Option<(usize, usize)> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let bare = line.trim_end_matches(['\n', '\r']);
        if let Some((mark, length)) = fence {
            if fence_line(bare).is_some_and(|(m, l, rest)| m == mark && l >= length && rest.trim().is_empty())
            {
                fence = None;
            }
            continue;
        }
        let opens = fence_line(bare);
        if opens.is_some() || bare.trim().is_empty() {
            if let Some((from, to)) = paragraph.take() {
                prose(&text[from..to], &mut found);
            }
            fence = opens.map(|(mark, length, _)| (mark, length));
            continue;
        }
        paragraph = Some((paragraph.map_or(start, |(from, _)| from), start + bare.len()));
    }
    if let Some((from, to)) = paragraph {
        prose(&text[from..to], &mut found);
    }
    found
}

/// A fence line's character, its run's length and what follows the run: at
/// most three characters of indentation, then three or more backticks or
/// tildes.
fn fence_line(line: &str) -> Option<(char, usize, &str)> {
    let rest = line.trim_start();
    if line[..line.len() - rest.len()].chars().count() > 3 {
        return None;
    }
    let mark = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = rest.len() - rest.trim_start_matches(mark).len();
    (length >= 3).then(|| (mark, length, &rest[length..]))
}

/// The references of a paragraph, outside its code spans.
fn prose<'a>(paragraph: &'a str, found: &mut Vec<&'a str>) {
    let mut from = 0;
    for (start, end) in code_spans(paragraph) {
        images(&paragraph[from..start], found);
        from = end;
    }
    images(&paragraph[from..], found);
}

/// Where a paragraph's code spans are: from a run of backticks to the next
/// run of exactly as many. A run with none after it is text.
fn code_spans(paragraph: &str) -> Vec<(usize, usize)> {
    let bytes = paragraph.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let length = bytes[i..].iter().take_while(|b| **b == b'`').count();
        if length > 0 {
            runs.push((i, length));
        }
        i += length.max(1);
    }
    let mut by_length: BTreeMap<usize, VecDeque<usize>> = BTreeMap::new();
    for (index, (_, length)) in runs.iter().enumerate() {
        by_length.entry(*length).or_default().push_back(index);
    }
    let (mut spans, mut index) = (Vec::new(), 0);
    while let Some(&(start, length)) = runs.get(index) {
        let same = by_length.entry(length).or_default();
        while same.front().is_some_and(|other| *other <= index) {
            same.pop_front();
        }
        match same.front() {
            Some(&close) => {
                spans.push((start, runs[close].0 + length));
                index = close + 1;
            }
            None => index += 1,
        }
    }
    spans
}

/// The references in text that holds no code.
fn images<'a>(text: &'a str, found: &mut Vec<&'a str>) {
    // Where a title was found to have no closing quote before its line ends:
    // one that starts there is not looked at again.
    let mut unclosed = 0..0;
    let mut at = 0;
    while let Some(next) = text[at..].find("![") {
        let start = at + next + 2;
        match reference(text, start, &mut unclosed) {
            Some((dest, end)) => {
                found.push(dest);
                at = end;
            }
            None => at = start - 1,
        }
    }
}

/// The reference whose `![` ends at `start`: its DEST and where it ends.
fn reference<'a>(
    text: &'a str,
    start: usize,
    unclosed: &mut std::ops::Range<usize>,
) -> Option<(&'a str, usize)> {
    let stop = |b: &u8| matches!(b, b'\n' | b'\r');
    // ALT may hold one level of balanced brackets (`f on [-20, 20]`), never a line break.
    let mut depth = 0u8;
    let mut alt = None;
    for (at, b) in text.as_bytes()[start..].iter().take(MAX_IMAGE_ALT_BYTES + 1).enumerate() {
        match b {
            b'\n' | b'\r' => return None,
            b'[' if depth == 0 => depth = 1,
            b'[' => return None,
            b']' if depth == 1 => depth = 0,
            b']' => {
                alt = Some(at);
                break;
            }
            _ => {}
        }
    }
    let open = start + alt?;
    if !text[open + 1..].starts_with('(') {
        return None;
    }
    let dest_at = open + 2;
    let dest_len = text[dest_at..]
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(text.len() - dest_at);
    let dest = &text[dest_at..dest_at + dest_len];
    let mut end = dest_at + dest_len;
    if dest.is_empty() {
        return None;
    }
    if !text[end..].starts_with(')') {
        let spaced = text[end..].trim_start_matches([' ', '\t']);
        let quote = text.len() - spaced.len();
        if quote == end || !spaced.starts_with('"') || unclosed.contains(&quote) {
            return None;
        }
        let Some(title) = text.as_bytes()[quote + 1..].iter().position(|b| *b == b'"' || stop(b)) else {
            *unclosed = quote..text.len();
            return None;
        };
        let close = quote + 1 + title;
        if text.as_bytes()[close] != b'"' {
            *unclosed = quote..close;
            return None;
        }
        end = close + 1;
        if !text[end..].starts_with(')') {
            return None;
        }
    }
    Some((dest, end + 1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The references of a reply, row by row: the syntax the apps share,
    /// never inside code.
    #[test]
    fn references_are_markdown_images_outside_code() {
        let long_alt = format!("![{}](too-long-alt.png) ![ok](b.png)", "a".repeat(MAX_IMAGE_ALT_BYTES + 1));
        for (text, expected) in [
            ("See ![the plot](figures/sinc.png).", vec!["figures/sinc.png"]),
            ("![f on [-20, 20]](f.png) ![x [a [b]]](x.png) ![open [a](y.png)", vec!["f.png"]),
            ("![](a.png)![t](b.png \"A title\")", vec!["a.png", "b.png"]),
            ("![a](a.png\t\"tab\") ![b](b.png  \"two\")", vec!["a.png", "b.png"]),
            (
                "![a](https://x/y.png) ![b](/abs.png) ![c](../up.png)",
                vec!["https://x/y.png", "/abs.png", "../up.png"],
            ),
            ("![a](a b.png) ![c](c(1).png) ![d]() ![e] (e.png) ![f\ng](f.png)", vec![]),
            ("![a](a.png \"no close) ![b](b.png \"x\"y\")", vec![]),
            ("![a](a.png \"open\n![b](b.png)", vec!["b.png"]),
            ("![a](a.png \"t\" ) ![b](b.png\"t\")", vec!["b.png\"t\""]),
            ("![a]![b](b.png) ![![c](c.png)", vec!["b.png", "c.png"]),
            (
                "`![a](a.png)` ![b](b.png) ``![c](c.png)`` ```![d](d.png)``` `![e](e.png)",
                vec!["b.png", "e.png"],
            ),
            ("``![a](a.png)` ![b](b.png)", vec!["a.png", "b.png"]),
            ("`spans a\nline ![a](a.png)` ![b](b.png)", vec!["b.png"]),
            ("`ends at a blank line\n\n![a](a.png)` ![b](b.png)", vec!["a.png", "b.png"]),
            ("```\n![a](a.png)\n```\n![b](b.png)", vec!["b.png"]),
            ("   ~~~~ python\n![a](a.png)\n~~~\n![b](b.png)\n~~~~~\n![c](c.png)", vec!["c.png"]),
            ("~~~\n![a](a.png)\n```\n![b](b.png)", vec![]),
            ("    ```\n![a](a.png)\n", vec!["a.png"]),
            ("text\r\n```\r\n![a](a.png)\r\n```\r\n![b](b.png)", vec!["b.png"]),
            (long_alt.as_str(), vec!["b.png"]),
        ] {
            assert_eq!(references(text), expected, "{text:?}");
        }
    }

    /// Each DEST the interface takes as a path is read once beneath the
    /// working directory: no symbolic link followed, a regular file, an
    /// image by its bytes within the bound, at most 8; every other DEST is
    /// left alone.
    #[test]
    fn a_reply_shows_what_it_may_read_and_names_what_it_cannot() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("figures")).unwrap();
        std::fs::write(root.join("figures/a.png"), PNG).unwrap();
        std::fs::write(root.join("big.png"), [PNG, &vec![0; MAX_REPLY_IMAGE_BYTES]].concat()).unwrap();
        std::fs::write(root.join("notes.txt"), "text").unwrap();
        std::os::unix::fs::symlink("figures/a.png", root.join("linked.png")).unwrap();
        std::os::unix::fs::symlink("figures", root.join("dir")).unwrap();
        let workdir = Workdir::open(root.to_str().unwrap()).unwrap();
        let text = "![a](figures/a.png) ![again](figures/a.png) ![b](big.png) ![c](missing.png) \
            ![d](notes.txt) ![e](linked.png) ![f](dir/a.png) ![g](figures) ![h](https://x/a.png) ![i](../a.png)";
        let (images, refused) = read(&workdir, text);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].path.as_str(), images[0].media_type.as_str()), ("figures/a.png", "image/png"));
        let got: Vec<(&str, RefusedReason, Option<u64>)> =
            refused.iter().map(|r| (r.path.as_str(), r.reason, r.bytes)).collect();
        let big = Some(PNG.len() as u64 + MAX_REPLY_IMAGE_BYTES as u64);
        assert_eq!(
            got,
            [
                ("big.png", RefusedReason::TooLarge, big),
                ("missing.png", RefusedReason::NotFound, None),
                ("notes.txt", RefusedReason::NotAnImage, None),
                ("linked.png", RefusedReason::OutsideProject, None),
                ("dir/a.png", RefusedReason::OutsideProject, None),
                ("figures", RefusedReason::NotAFile, None),
            ]
        );
        let note = note(&refused).unwrap();
        assert!(
            note.starts_with(NOTE) && note.contains("- big.png: too_large, 262152 bytes, over 256 KiB"),
            "{note}"
        );

        for i in 0..MAX_REPLY_IMAGES {
            std::fs::write(root.join(format!("figures/{i}.png")), PNG).unwrap();
        }
        let many: String =
            (0..20).map(|i| format!("![{i}](figures/{i}.png) ![again](./figures//0.png) ")).collect();
        let (images, refused) = read(&workdir, &many);
        assert_eq!((images.len(), refused.len()), (MAX_REPLY_IMAGES, MAX_REPLY_IMAGES));
        assert_eq!(images[1].path, "./figures//0.png", "a DEST is kept as written");
        assert!(refused.iter().all(|r| r.reason == RefusedReason::TooMany && r.bytes.is_none()));
    }
}
