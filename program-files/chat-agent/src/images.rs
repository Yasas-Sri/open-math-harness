//! Images in this agent's conversation: the ones a person attached to a
//! message and the ones `view_image` shows.
//!
//! An image travels in a user message as a label naming its file followed by
//! the image: the person's in their own message, a tool's in a user message
//! right after the tool results, which stay text. Every model call re-sends
//! the conversation with its images, and one call is one kernel frame
//! ([`MAX_FRAME_BYTES`]) shared with the conversation's text. So the history
//! keeps the newest images that fit ([`History::settle`]): at most
//! [`MAX_REQUEST_IMAGES`], at most [`MAX_REQUEST_IMAGE_DATA_BYTES`] of base64,
//! and no more than the text leaves of the frame. Each older one is replaced
//! by a note naming its file, which the model can look at again with
//! `view_image`. An image left out is not shown again in this history and its
//! bytes are dropped, so what the agent holds stays bounded; compaction
//! summarises the images away with the rest.
//!
//! The transcript keeps where each image came from ([`Source`]: its path in
//! the working directory, size and SHA-256), never its bytes. A continuation
//! reads the file again and shows it only if it is the same image: a file
//! gone or changed is said to be, and nothing else is shown in its place.
//!
//! An image the person sent inside a message (conversation interface,
//! "Images") is no file: it is shown as the message's other images are, by
//! its name, and the conversation keeps its bytes, from which a continuation
//! of the session shows it again (module `restore`); a transcript names it
//! only, and `view_image` cannot open it.

use std::collections::VecDeque;

use base64::Engine;
use nucleus_conversation_interface::InlineImage;
use nucleus_model_interface::{
    IMAGE_MEDIA_TYPES, MAX_IMAGE_BYTES, MAX_REQUEST_IMAGE_DATA_BYTES, MAX_REQUEST_IMAGES, Message, Part,
    Role, ToolSpec, base64_len, image_media_type,
};
use nucleus_protocol::{Error, ErrorCode, MAX_FRAME_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;

use crate::workdir::{NOT_THERE, Refused, Workdir, human, read_all};

/// The agent-local tool that shows the model an image file.
pub const VIEW_IMAGE: &str = "view_image";

/// What a request needs besides the history: the instructions, the tool
/// list, the envelope. Generous, so that a request with images never
/// outgrows its frame.
const RESERVED_BYTES: usize = 1024 * 1024;

/// The text of the user message that carries the images a batch of tool
/// calls read.
pub const VIEWED: &str = "The images view_image read, each after its path:";

/// Where an image came from: what the transcript keeps instead of its bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Relative to the working directory; empty for an image sent inline.
    pub path: String,
    /// What its bytes are, one of [`IMAGE_MEDIA_TYPES`].
    pub media_type: String,
    pub bytes: u64,
    /// Lowercase hexadecimal.
    pub sha256: String,
    /// The name of an image the person sent inside a message (module doc),
    /// which no file holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    /// Shown, with its base64.
    Shown(String),
    /// Read back from a transcript and not read yet.
    Unread,
    /// Not shown, and why.
    Hidden(String),
}

/// An image the conversation holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    source: Source,
    state: State,
}

/// A name as JSON quotes it, so it cannot end the label it is in.
fn quoted(name: &str) -> String {
    serde_json::to_string(name).unwrap_or_default()
}

fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

impl Held {
    /// The image at `relative` in `workdir`, at most [`MAX_IMAGE_BYTES`]; its
    /// bytes decide its media type. `size`: the size it must have.
    pub fn read(workdir: &Workdir, relative: &str, size: Option<u64>) -> Result<Self, Refused> {
        let (file, found) = workdir.open_file(relative)?;
        if let Some(expected) = size.filter(|expected| *expected != found) {
            return Err(Refused(format!(
                "it changed after it was attached (it has {}, not {})",
                human(found),
                human(expected)
            )));
        }
        let bytes = read_all(file, found, MAX_IMAGE_BYTES)?;
        let media_type = image_media_type(&bytes)
            .ok_or_else(|| Refused("it is not a PNG, JPEG, WebP or GIF image".to_owned()))?;
        let source = Source {
            path: relative.to_owned(),
            media_type: media_type.to_owned(),
            bytes: found,
            sha256: sha256(&bytes),
            sent: None,
        };
        Ok(Self { source, state: State::Shown(base64::engine::general_purpose::STANDARD.encode(&bytes)) })
    }

    /// An image the person sent inside a message, as the conversation keeps
    /// it (its service checked it); one whose bytes are not the image it says
    /// is refused.
    pub fn sent(image: &InlineImage) -> Result<Self, Refused> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| Refused("its base64 could not be read".to_owned()))?;
        let media_type = image_media_type(&bytes)
            .filter(|found| *found == image.media_type)
            .ok_or_else(|| Refused("it is not the PNG, JPEG, WebP or GIF image it says".to_owned()))?;
        let source = Source {
            path: String::new(),
            media_type: media_type.to_owned(),
            bytes: bytes.len() as u64,
            sha256: sha256(&bytes),
            sent: Some(image.name.clone()),
        };
        Ok(Self { source, state: State::Shown(image.data_base64.clone()) })
    }

    /// What the model reads for this image: its label and the image, or
    /// the label with why it is not shown.
    fn parts(&self) -> Vec<Part> {
        let Source { path, media_type, bytes, sent, .. } = &self.source;
        let label = match sent {
            Some(name) => {
                format!("{} (sent with the message, {media_type}, {}):", quoted(name), human(*bytes))
            }
            None => format!("{path} ({media_type}, {}):", human(*bytes)),
        };
        match &self.state {
            State::Shown(data) => vec![
                Part::Text { text: label },
                Part::Image { media_type: media_type.clone(), data_base64: data.clone() },
            ],
            State::Hidden(why) => vec![Part::Text { text: format!("{label} [not shown: {why}]") }],
            State::Unread => vec![Part::Text { text: format!("{label} [not read yet]") }],
        }
    }
}

/// Reads the image a transcript recorded, and gives its base64 only if it is
/// the same image: a file that is gone, or is anything else now (another
/// image, not an image, a link), is said to be.
pub fn reload(workdir: Option<&Workdir>, source: &Source) -> Result<String, String> {
    if source.sent.is_some() {
        return Err("it was sent inside a message, which only the conversation keeps".to_owned());
    }
    let workdir = workdir.ok_or("this agent has no working directory to read it from")?;
    let changed = || "the file has changed since it was shown".to_owned();
    let relative = workdir.relative(&source.path).map_err(|why| why.0)?;
    let held = Held::read(workdir, &relative, None).map_err(|why| {
        if why.0 == NOT_THERE { "the file is gone since it was shown".to_owned() } else { changed() }
    })?;
    match held.state {
        State::Shown(data) if held.source == *source => Ok(data),
        _ => Err(changed()),
    }
}

/// A message as the agent keeps it: its request form (its images as parts
/// after its text) and the images it carries.
#[derive(Clone, Debug)]
struct Kept {
    message: Message,
    images: Vec<Held>,
    /// Bytes of the request form but the images' base64.
    text_bytes: usize,
}

impl Kept {
    fn new(mut message: Message, images: Vec<Held>) -> Self {
        message.parts = images.iter().flat_map(Held::parts).collect();
        let text_bytes = Self::measure(&message);
        Self { message, images, text_bytes }
    }

    fn measure(message: &Message) -> usize {
        let data: usize = message.parts.iter().filter_map(Part::image_data).map(str::len).sum();
        serde_json::to_vec(message).map_or(0, |json| json.len()).saturating_sub(data)
    }
}

/// The conversation: every message the next model call sends after the
/// opening, and the images they carry within the budget (module doc).
#[derive(Clone, Debug, Default)]
pub struct History {
    entries: VecDeque<Kept>,
    /// Bytes of every message's request form but the images' base64.
    text_bytes: usize,
}

impl History {
    /// The messages as the next request sends them.
    pub fn iter(&self) -> impl Iterator<Item = &Message> {
        self.entries.iter().map(|kept| &kept.message)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.text_bytes = 0;
    }

    pub fn push_back(&mut self, message: Message) {
        self.push(message, Vec::new());
    }

    /// Appends `message` with `images` after its text, then keeps the
    /// newest images that fit.
    pub fn push(&mut self, message: Message, images: Vec<Held>) {
        self.restore(message, images);
        self.settle(|_| Err("it was not read".to_owned()));
    }

    /// Appends a message read back from a transcript; its images are read
    /// by the next [`Self::settle`], newest first, as far as they fit.
    pub fn restore(&mut self, message: Message, images: Vec<Held>) {
        let kept = Kept::new(message, images);
        self.text_bytes += kept.text_bytes;
        self.entries.push_back(kept);
    }

    /// Keeps the newest images that fit one request (module doc) and hides
    /// the others for good; an image read back from a transcript is read now
    /// with `load` if it fits.
    pub fn settle(&mut self, mut load: impl FnMut(&Source) -> Result<String, String>) {
        let room = MAX_FRAME_BYTES.saturating_sub(self.text_bytes + RESERVED_BYTES);
        let (mut budget, mut count) = (room.min(MAX_REQUEST_IMAGE_DATA_BYTES), MAX_REQUEST_IMAGES);
        let mut changed = Vec::new();
        for (index, kept) in self.entries.iter_mut().enumerate().rev() {
            let mut touched = false;
            for held in kept.images.iter_mut().rev() {
                let size = match &held.state {
                    State::Shown(data) => data.len(),
                    State::Unread => base64_len(usize::try_from(held.source.bytes).unwrap_or(usize::MAX)),
                    State::Hidden(_) => continue,
                };
                if count == 0 || size > budget {
                    let again = match held.source.sent {
                        Some(_) => "",
                        None => "; look at it again with view_image if you need it",
                    };
                    held.state = State::Hidden(format!(
                        "newer images and the conversation's text fill what one model request can carry{again}"
                    ));
                    touched = true;
                    continue;
                }
                if held.state == State::Unread {
                    held.state = load(&held.source).map_or_else(State::Hidden, State::Shown);
                    touched = true;
                }
                if let State::Shown(data) = &held.state {
                    budget -= data.len().min(budget);
                    count -= 1;
                }
            }
            if touched {
                changed.push(index);
            }
        }
        for index in changed {
            if let Some(kept) = self.entries.get_mut(index) {
                kept.message.parts = kept.images.iter().flat_map(Held::parts).collect();
                let text_bytes = Kept::measure(&kept.message);
                self.text_bytes = self.text_bytes - kept.text_bytes + text_bytes;
                kept.text_bytes = text_bytes;
            }
        }
    }

    /// Whether every call of the last assistant message has its result:
    /// where the images a batch of tool calls read may follow.
    pub fn tool_batch_complete(&self) -> bool {
        let mut answered = 0;
        for kept in self.entries.iter().rev() {
            match kept.message.role {
                Role::Tool => answered += 1,
                Role::Assistant => {
                    return !kept.message.tool_calls.is_empty() && answered >= kept.message.tool_calls.len();
                }
                Role::System | Role::User => return false,
            }
        }
        false
    }
}

/// A transcript line of a message that carries images: the message as the
/// model read its text, and where each image came from.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagesLine {
    message: Message,
    images: Vec<Source>,
}

/// One transcript line for `message` with `images`: the message alone when it
/// has none, or an [`ImagesLine`]. Never an image's bytes.
pub fn line(message: &Message, images: &[Held]) -> serde_json::Result<String> {
    if images.is_empty() {
        return serde_json::to_string(message);
    }
    let images = images.iter().map(|held| held.source.clone()).collect();
    serde_json::to_string(&ImagesLine { message: Message { parts: Vec::new(), ..message.clone() }, images })
}

/// A transcript line read back: the message and its images, to be read again
/// by [`History::settle`].
pub fn parse_line(line: &str) -> serde_json::Result<(Message, Vec<Held>)> {
    if let Ok(ImagesLine { message, images }) = serde_json::from_str(line) {
        let held = images.into_iter().map(|source| Held { source, state: State::Unread }).collect();
        return Ok((message, held));
    }
    Ok((serde_json::from_str(line)?, Vec::new()))
}

/// The `view_image` tool.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: VIEW_IMAGE.into(),
        description: format!(
            "Look at an image file in your working directory: a plot or a screenshot you made, or an \
             image the person attached. PNG, JPEG, WebP or GIF, at most {}. The image is shown to you in \
             the next message, after the tool results. Every request re-sends the images in the \
             conversation and only the newest fit (at most {MAX_REQUEST_IMAGES}, about {} together): an \
             older one is replaced by a note naming its file, and you can look at it again.",
            human(MAX_IMAGE_BYTES as u64),
            human(MAX_REQUEST_IMAGE_DATA_BYTES as u64 / 4 * 3),
        ),
        parameters: json!({"type": "object",
            "properties": {"path": {"type": "string",
                "description": "The image's path, relative to your working directory"}},
            "required": ["path"],
            "additionalProperties": false}),
    }
}

/// Reads the image `view_image` asks for: the model's arguments are only a
/// request, checked here, and the file is checked as it is opened.
pub fn view(workdir: &Workdir, arguments: &Value) -> Result<Held, Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        path: String,
    }
    let invalid = |why: String| Error::new(ErrorCode::InvalidInput, why);
    let Arguments { path } = serde_json::from_value(arguments.clone())
        .map_err(|_| invalid("view_image takes {\"path\": <a path in your working directory>}".into()))?;
    let relative = workdir.relative(&path).map_err(|why| invalid(format!("{path}: {}", why.0)))?;
    Held::read(workdir, &relative, None).map_err(|why| invalid(format!("{relative}: {}", why.0)))
}

/// What `view_image` answers for `held`, which the next message shows.
pub fn viewed(held: &Held) -> String {
    let Source { path, media_type, bytes, .. } = &held.source;
    json!({"path": path, "media_type": media_type, "bytes": bytes,
           "shown": "in the next message, after the tool results"})
    .to_string()
}

/// Whether `media_type` is one this agent shows as an image.
pub fn shown_type(media_type: &str) -> bool {
    IMAGE_MEDIA_TYPES.contains(&media_type)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn held(path: &str, data_bytes: usize) -> Held {
        let source = Source {
            path: path.into(),
            media_type: "image/png".into(),
            bytes: 1,
            sha256: String::new(),
            sent: None,
        };
        Held { source, state: State::Shown("A".repeat(data_bytes)) }
    }

    /// The paths of the images the next request shows, oldest first.
    fn shown(history: &History) -> Vec<String> {
        let mut paths = Vec::new();
        for message in history.iter() {
            for pair in message.parts.windows(2) {
                if let [Part::Text { text }, Part::Image { .. }] = pair {
                    paths.push(text.split(' ').next().unwrap().to_owned());
                }
            }
        }
        paths
    }

    /// Only the newest images that fit one request are shown: by count, by
    /// bytes, and by what the conversation's text leaves of the frame. One
    /// left out names its file and is not shown again.
    #[test]
    fn only_the_newest_images_that_fit_one_request_are_shown() {
        let mut history = History::default();
        for i in 0..=MAX_REQUEST_IMAGES {
            history.push(Message::text(Role::User, "look"), vec![held(&format!("{i}.png"), 1024)]);
        }
        let expected: Vec<String> = (1..=MAX_REQUEST_IMAGES).map(|i| format!("{i}.png")).collect();
        assert_eq!(shown(&history), expected, "by count");
        let first = history.iter().next().unwrap();
        assert!(
            matches!(&first.parts[..], [Part::Text { text }] if text.starts_with("0.png") && text.contains("view_image")),
            "{:?}",
            first.parts
        );

        let large = MAX_REQUEST_IMAGE_DATA_BYTES - 64 * 1024;
        history.push(Message::text(Role::User, "and this"), vec![held("large.png", large)]);
        let mut expected: Vec<String> = (2..=MAX_REQUEST_IMAGES).map(|i| format!("{i}.png")).collect();
        expected.push("large.png".into());
        assert_eq!(shown(&history), expected, "by bytes and count, newest first");

        // The conversation's text grows past what leaves room for the large one.
        history
            .push_back(Message::text(Role::Assistant, "x".repeat(MAX_FRAME_BYTES - large - RESERVED_BYTES)));
        expected.pop();
        assert_eq!(shown(&history), expected, "the frame is shared with the text");
        history.push_back(Message::text(Role::User, "and again"));
        assert_eq!(shown(&history), expected, "what was left out is not shown again");
        let request: usize = history.iter().map(|m| serde_json::to_vec(m).unwrap().len()).sum();
        assert!(request + RESERVED_BYTES <= MAX_FRAME_BYTES, "{request}");
    }

    /// The transcript keeps where an image came from, never its bytes; a
    /// continuation shows it again only when the file is unchanged, and says
    /// so when it is gone or changed.
    #[test]
    fn a_continuation_shows_an_image_again_only_if_its_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let png = |tail: &str| [b"\x89PNG\r\n\x1a\n".as_slice(), tail.as_bytes()].concat();
        for name in ["same", "changed", "gone"] {
            std::fs::write(dir.path().join(format!("{name}.png")), png(name)).unwrap();
        }
        let workdir = Workdir::open(dir.path().to_str().unwrap()).unwrap();
        let mut lines =
            vec![serde_json::to_string(&Message::text(Role::User, "a line from before images")).unwrap()];
        for name in ["same", "changed", "gone"] {
            let image = Held::read(&workdir, &format!("{name}.png"), None).unwrap();
            let line = line(&Message::text(Role::User, format!("see {name}")), &[image]).unwrap();
            let State::Shown(data) = Held::read(&workdir, &format!("{name}.png"), None).unwrap().state else {
                unreachable!()
            };
            assert!(!line.contains(&data) && line.contains(&sha256(&png(name))), "{line}");
            lines.push(line);
        }
        std::fs::write(dir.path().join("changed.png"), png("changed again")).unwrap();
        std::fs::remove_file(dir.path().join("gone.png")).unwrap();

        let mut history = History::default();
        for line in &lines {
            let (message, images) = parse_line(line).unwrap();
            history.restore(message, images);
        }
        history.settle(|source| reload(Some(&workdir), source));
        assert_eq!(shown(&history), ["same.png"]);
        let notes: Vec<String> = history
            .iter()
            .skip(2)
            .map(|message| match &message.parts[..] {
                [Part::Text { text }] => text.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert!(notes[0].starts_with("changed.png") && notes[0].contains("has changed"), "{notes:?}");
        assert!(notes[1].starts_with("gone.png") && notes[1].contains("is gone"), "{notes:?}");
        assert_eq!(history.iter().next().unwrap().content, "a line from before images");
    }
}
