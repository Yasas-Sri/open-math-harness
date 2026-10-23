//! The files a person attached to a message, as the model is shown them
//! (conversation interface, "Files").
//!
//! Inline text attachments follow the message's text in `<attached_file>`
//! blocks. A file uploaded into the session's working directory is read
//! there ([`crate::workdir`]): the file actually opened is checked against
//! the reference (its size) and against what it claims to be. An image (PNG, JPEG, WebP or GIF by its first bytes, at most
//! 4 MiB) follows the message's text as an image when the session's model
//! takes images ([`crate::images`]); a PDF (`%PDF-`) becomes its text through
//! the `pdf` service, page by page; any other file whose bytes are UTF-8 text
//! is inlined, at most [`crate::MAX_TOOL_RESULT_CHARS`] characters of it;
//! anything else is only named. A note before the files lists each one with
//! its path, so the model knows they are in its working directory and can
//! open them again with its tools. A file it cannot use is named with the
//! reason and the conversation shows an error saying so; the turn goes on.
//! What a file holds is untrusted input like the person's text, instructions
//! in an image or a PDF included.
//!
//! An image the person sent inside the message (conversation interface,
//! "Images") follows the text before the files' images, after a note naming
//! it; it is no file, so the note says so. A continuation shows it again
//! from the conversation, which keeps it ([`replayed`]).
//!
//! An uploaded attachment is an ordinary file of the project: every member
//! and every agent of the project can see and change it until someone
//! deletes it. The size check catches a change of size between attaching and
//! reading, and an image shown once is shown again only unchanged, but a
//! change that keeps the size, made before the agent first reads the file,
//! goes unnoticed.

use nucleus_conversation_interface::{self as conversation, Entry, FileRef, InlineImage, UserMessage};
use nucleus_pdf_interface as pdf;
use nucleus_protocol::ResourcePath;
use nucleus_sdk::ChannelError;

use crate::images::{self, Held};
use crate::workdir::{self, Refused, human};

/// The most of a text file read: [`crate::MAX_TOOL_RESULT_CHARS`] characters
/// of any UTF-8, and one byte to tell that there is more.
const TEXT_READ_BYTES: usize = crate::MAX_TOOL_RESULT_CHARS * 4 + 1;
/// A PDF's header may start anywhere in its first kilobyte.
const PDF_HEADER_WITHIN: usize = 1024;

/// What became of one attached file.
#[derive(Debug)]
enum Outcome {
    Image(Held),
    /// Its text, inlined after the note; a PDF's says how many pages it has.
    Text {
        block: String,
        pages: Option<u32>,
    },
    /// Not shown, by design: only named.
    Named,
    Refused(String),
    /// Not read again: the message is replayed from the conversation
    /// (module `restore`).
    Earlier,
}

/// What precedes the list of a message's files.
const FILES_NOTE: &str = "\n\n[The person attached these files. They are in your working directory, at \
     these paths relative to it:\n";
/// What precedes the list of the images a message carries.
const IMAGES_NOTE: &str = "\n\n[The person sent these images inside the message; they are not files in your \
     working directory:\n";
/// Why an image is not shown to a model that takes none.
const NO_IMAGES: &str = "this session's model takes no images";

/// The images `message` carries as this agent shows them: each one, or why
/// it is not shown. `image_input`: whether the model takes images.
pub(crate) fn sent_images(message: &UserMessage, image_input: bool) -> Vec<Result<Held, String>> {
    let shown = |image: &InlineImage| match image_input {
        true => Held::sent(image).map_err(|why| why.0),
        false => Err(NO_IMAGES.to_owned()),
    };
    message.images.iter().map(shown).collect()
}

/// The note on the images a message carries, by what became of each; none
/// without images.
fn images_note(message: &UserMessage, shown: &[Result<Held, String>]) -> String {
    if message.images.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = message
        .images
        .iter()
        .zip(shown)
        .map(|(image, shown)| {
            let what = match shown {
                Ok(_) => "shown below".to_owned(),
                Err(why) => format!("not shown: {why}"),
            };
            let size = human(u64::try_from(image.bytes()).unwrap_or(u64::MAX));
            format!("- {} ({}, {size}): {what}", quote(&image.name), image.media_type)
        })
        .collect();
    format!("{IMAGES_NOTE}{}]", lines.join("\n"))
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// The text of `message` with its inline attachments, each in a block
/// whose name is quoted, so it cannot close the block it labels.
pub fn inline(message: &UserMessage) -> String {
    let mut prompt = message.text.clone();
    for file in &message.attachments {
        prompt.push_str(&format!(
            "\n\n<attached_file name={} media_type=\"{}\" bytes=\"{}\">\n{}\n</attached_file>",
            quote(&file.name),
            file.media_type,
            file.content.len(),
            file.content
        ));
    }
    prompt
}

/// The opening line of a file's block.
fn tag(file: &FileRef, extra: &str) -> String {
    format!(
        "<attached_file name={} path={} media_type=\"{}\" bytes=\"{}\"{extra}>",
        quote(&file.name),
        quote(&file.path),
        file.media_type,
        file.bytes
    )
}

/// The block of a text file: at most [`crate::MAX_TOOL_RESULT_CHARS`]
/// characters of `text`, the first `read` bytes of a file of `size`.
fn text_block(file: &FileRef, text: &str, size: u64, read: usize) -> String {
    let kept: String = text.chars().take(crate::MAX_TOOL_RESULT_CHARS).collect();
    let cut = kept.len() < text.len() || (read as u64) < size;
    let note = if cut {
        format!(
            "\n[truncated: the first {} characters of a {} file; read the rest with your tools]",
            kept.chars().count(),
            human(size)
        )
    } else {
        String::new()
    };
    let kept = kept.strip_suffix('\n').unwrap_or(&kept);
    format!("{}\n{kept}{note}\n</attached_file>", tag(file, ""))
}

fn pdf_block(file: &FileRef, extracted: &pdf::ExtractResponse) -> String {
    let note = if extracted.truncated {
        format!(
            "\n[truncated: the first {} characters of its text; read the rest with your pdf tool]",
            crate::MAX_TOOL_RESULT_CHARS
        )
    } else {
        String::new()
    };
    let pages = format!(" pages=\"{}\"", extracted.pages);
    format!("{}\n{}{note}\n</attached_file>", tag(file, &pages), extracted.text.trim_end())
}

/// One line of the note: the file's path, name when it differs, type and
/// size, and what the model gets of it.
fn describe(file: &FileRef, outcome: &Outcome) -> String {
    let own_name = file.path.rsplit('/').next() == Some(file.name.as_str());
    let name = if own_name { String::new() } else { format!("{}, ", quote(&file.name)) };
    let pages = match outcome {
        Outcome::Text { pages: Some(pages), .. } => format!(", {pages} pages"),
        _ => String::new(),
    };
    let what = match outcome {
        Outcome::Image(_) => "shown below".to_owned(),
        Outcome::Text { .. } => "its text is below".to_owned(),
        Outcome::Named => "not shown; open it with your tools if you need it".to_owned(),
        Outcome::Refused(why) => format!("not shown: {why}"),
        Outcome::Earlier => "not repeated here; open it with your tools if you need it".to_owned(),
    };
    format!("- {} ({name}{}, {}{pages}): {what}", file.path, file.media_type, human(file.bytes))
}

/// What the model was told a person said, as far as the conversation keeps
/// it: the text and inline attachments as they were, the images it carries
/// as `images` says ([`sent_images`]), and each file named by its path, its
/// content not read again (module `restore`).
pub(crate) fn replayed(message: &UserMessage, images: &[Result<Held, String>]) -> String {
    let mut prompt = inline(message);
    prompt.push_str(&images_note(message, images));
    if !message.files.is_empty() {
        let lines: Vec<String> = message.files.iter().map(|file| describe(file, &Outcome::Earlier)).collect();
        prompt.push_str(FILES_NOTE);
        prompt.push_str(&lines.join("\n"));
        prompt.push(']');
    }
    prompt
}

impl crate::Agent<'_> {
    /// What the model is told the person said: the text, the inline
    /// attachments, the note on the images it carries, then the note on the
    /// files and their text; its images and the files' wait in
    /// `prompt_images` for the message `answer` remembers.
    pub(crate) fn user_prompt(&mut self, message: UserMessage) -> Result<String, ChannelError> {
        self.skill_commands.extend(crate::skills::commands(&message.text));
        let mut prompt = inline(&message);
        let images = sent_images(&message, self.image_input);
        prompt.push_str(&images_note(&message, &images));
        for (image, shown) in message.images.iter().zip(images) {
            match shown {
                Ok(held) => self.prompt_images.push(held),
                Err(why) => {
                    let name = quote(&image.name);
                    let text =
                        format!("The image {name} sent with the message was not shown to the model: {why}");
                    self.post(&Entry::Error { text })?;
                }
            }
        }
        if message.files.is_empty() {
            return Ok(prompt);
        }
        let (mut lines, mut blocks) = (Vec::new(), Vec::new());
        for file in &message.files {
            let outcome = self.attached(file)?;
            lines.push(describe(file, &outcome));
            match outcome {
                Outcome::Image(held) => self.prompt_images.push(held),
                Outcome::Text { block, .. } => blocks.push(block),
                Outcome::Named | Outcome::Earlier => {}
                Outcome::Refused(why) => {
                    let text = format!("{} was not shown to the model: {why}", file.path);
                    self.post(&Entry::Error { text })?;
                }
            }
        }
        prompt.push_str(FILES_NOTE);
        prompt.push_str(&lines.join("\n"));
        prompt.push(']');
        for block in blocks {
            prompt.push_str("\n\n");
            prompt.push_str(&block);
        }
        Ok(prompt)
    }

    /// Reads one attached file (module doc).
    fn attached(&mut self, file: &FileRef) -> Result<Outcome, ChannelError> {
        let refused = |why: &str| Ok(Outcome::Refused(why.to_owned()));
        let Some(workdir) = &self.workdir else {
            return refused("this agent has no working directory to read it from");
        };
        let relative = match workdir.relative(&file.path) {
            Ok(relative) => relative,
            Err(Refused(why)) => return Ok(Outcome::Refused(why)),
        };
        let media = file.media_type.as_str();
        if media.starts_with("image/") {
            if !images::shown_type(media) {
                return refused("only PNG, JPEG, WebP and GIF images are shown");
            }
            if !self.image_input {
                return refused("this session's model takes no images");
            }
            return Ok(Held::read(workdir, &relative, Some(file.bytes))
                .map_or_else(|why| Outcome::Refused(why.0), Outcome::Image));
        }
        let (handle, size) = match workdir.open_file(&relative) {
            Ok(opened) => opened,
            Err(Refused(why)) => return Ok(Outcome::Refused(why)),
        };
        if size != file.bytes {
            return Ok(Outcome::Refused(format!(
                "it changed after it was attached (it has {}, not {})",
                human(size),
                human(file.bytes)
            )));
        }
        let head = match workdir::read_head(handle, TEXT_READ_BYTES) {
            Ok(head) => head,
            Err(Refused(why)) => return Ok(Outcome::Refused(why)),
        };
        if media == "application/pdf" {
            if !head[..head.len().min(PDF_HEADER_WITHIN)].windows(5).any(|w| w == b"%PDF-") {
                return refused("it is not a PDF");
            }
            if size > pdf::MAX_FILE_BYTES {
                return Ok(Outcome::Refused(format!("it is larger than {}", human(pdf::MAX_FILE_BYTES))));
            }
            return self.pdf_text(file, &relative);
        }
        Ok(match workdir::text(&head, size) {
            Some(text) => Outcome::Text { block: text_block(file, text, size, head.len()), pages: None },
            None if media.starts_with("text/") || conversation::TEXT_MEDIA_TYPES.contains(&media) => {
                Outcome::Refused("it is not UTF-8 text".to_owned())
            }
            None => Outcome::Named,
        })
    }

    /// A PDF's text through the `pdf` service, with this agent's own `pdf`
    /// tool: its binding names the working directory as the service sees it.
    fn pdf_text(&mut self, file: &FileRef, relative: &str) -> Result<Outcome, ChannelError> {
        let refused = |why: &str| Ok(Outcome::Refused(why.to_owned()));
        let Some(binding) = self.options.tool_bindings.iter().find(|b| b.service.as_str() == pdf::ENDPOINT)
        else {
            return refused("this agent has no pdf tool to read it with");
        };
        let segments = binding.resource.segments().iter().map(String::as_str).chain(relative.split('/'));
        let Ok(resource) = ResourcePath::new(segments) else {
            return refused("its path is not one the pdf tool takes");
        };
        let Some(handle) =
            self.kernel.find_handle_for(binding.endpoint.as_str(), pdf::ACTION_EXTRACT, &resource)
        else {
            return refused("this agent's pdf tool does not reach it");
        };
        let request = pdf::ExtractRequest { max_chars: Some(crate::MAX_TOOL_RESULT_CHARS) };
        let reply = self.kernel.call(handle, pdf::ACTION_EXTRACT, &resource, &request, None)?;
        Ok(match reply.and_then(|payload| payload.decode::<pdf::ExtractResponse>()) {
            Ok(extracted) => {
                Outcome::Text { block: pdf_block(file, &extracted), pages: Some(extracted.pages) }
            }
            Err(e) => Outcome::Refused(format!("its text could not be read: {}", e.message)),
        })
    }
}
