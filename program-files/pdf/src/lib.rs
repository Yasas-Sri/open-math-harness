//! The PDF text service: the provider of the `pdf` interface.
//!
//! It reads PDFs from one directory (its root: a bind of its sandbox, or, as
//! a project's host, the project's folder, as the projects' user) and
//! returns their text, so the agent can read a PDF without holding any file
//! authority of its own beyond `pdf extract`. Like the filesystem service, it
//! has the Linux kernel resolve each path beneath its root (`openat2` with
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`) and refuses any symbolic link in
//! it: the kernel checked the caller's scope against the literal path, and a
//! link inside the root would make a name in one scope read a file in another.
//! Only a regular file is read, at most [`MAX_FILE_BYTES`] of it.
//!
//! It extracts text only — no OCR — so a scanned PDF yields little. The text
//! comes page by page, each page after its marker (`[page 2 of 6]`).
//! Extraction runs in caught-panic boundaries, because the PDF parser can
//! panic on a malformed file, and a bad input must be an error, not a dead
//! service; a page that cannot be read says so in its place. The text is
//! truncated to a bounded size.
//!
//! It uses no network and no credentials, and the file's bytes and the text
//! it returns are untrusted input.

use std::io::Read;
use std::path::PathBuf;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use nucleus_pdf_interface::{
    ACTION_EXTRACT, ENDPOINT, ExtractRequest, ExtractResponse, MAX_FILE_BYTES, interface, page_marker,
};
use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_sdk::{Connection, Event};

/// Default and largest amount of text returned.
const DEFAULT_MAX_CHARS: usize = 200_000;
const HARD_MAX_CHARS: usize = 2_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub root: PathBuf,
    pub endpoint: EndpointName,
    pub max_chars: usize,
}

impl Options {
    /// Parses `--root <dir> [--endpoint <name>] [--max-chars <n>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut root = None;
        let mut endpoint = ENDPOINT.to_owned();
        let mut max_chars = DEFAULT_MAX_CHARS;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--root" => root = Some(PathBuf::from(value)),
                "--endpoint" => endpoint = value,
                "--max-chars" => max_chars = value.parse().map_err(|_| "--max-chars must be a number")?,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            root: root.ok_or("--root is required")?,
            endpoint: EndpointName::new(&endpoint).map_err(|e| e.to_string())?,
            max_chars: max_chars.clamp(1, HARD_MAX_CHARS),
        })
    }
}

fn refuse(code: ErrorCode, message: &str) -> Error {
    Error::new(code, message.to_owned())
}

/// The service's whole authority over the filesystem: one directory.
pub struct PdfService {
    root: Dir,
    max_chars: usize,
}

impl PdfService {
    pub fn open(root: &std::path::Path, max_chars: usize) -> std::io::Result<Self> {
        Ok(Self { root: Dir::open_ambient_dir(root, ambient_authority())?, max_chars })
    }

    fn relative(resource: &ResourcePath) -> PathBuf {
        resource.segments().iter().collect()
    }

    /// Opens `path` beneath the root with no symbolic link followed in any
    /// component (module doc), and only if it is a regular file of at most
    /// [`MAX_FILE_BYTES`].
    fn open_file(&self, path: &std::path::Path) -> Result<std::fs::File, Error> {
        use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        // Non-blocking, so a FIFO planted under the name cannot hang the service.
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd =
            rustix::fs::openat2(&self.root, path, flags, Mode::empty(), resolve).map_err(|e| match e {
                rustix::io::Errno::LOOP | rustix::io::Errno::XDEV => {
                    refuse(ErrorCode::PermissionDenied, "the path crosses a symbolic link or leaves the root")
                }
                e => {
                    let e = std::io::Error::from(e);
                    Error::new(io_code(&e), format!("open: {e}"))
                }
            })?;
        let stat =
            rustix::fs::fstat(&fd).map_err(|e| Error::new(ErrorCode::ServiceError, format!("stat: {e}")))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(refuse(ErrorCode::InvalidInput, "not a regular file"));
        }
        if u64::try_from(stat.st_size).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return Err(refuse(ErrorCode::ServiceError, "PDF exceeds the read limit"));
        }
        Ok(fd.into())
    }

    /// Reads the PDF at `resource` (beneath the root) and returns its text.
    pub fn extract(
        &self,
        resource: &ResourcePath,
        request: &ExtractRequest,
    ) -> Result<ExtractResponse, Error> {
        if resource.segments().is_empty() {
            return Err(refuse(ErrorCode::InvalidInput, "a file path is required"));
        }
        let file = self.open_file(&Self::relative(resource))?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::new(ErrorCode::ServiceError, format!("read: {e}")))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(refuse(ErrorCode::ServiceError, "PDF exceeds the read limit"));
        }
        let limit = request.max_chars.map_or(self.max_chars, |n| n.clamp(1, self.max_chars));
        let (text, truncated, pages) = pages_text(&bytes, limit)?;
        Ok(ExtractResponse { text, truncated, pages })
    }
}

/// The text of a PDF, page by page after each page's marker, at most `limit`
/// characters of it; whether it was cut; how many pages the PDF has. The
/// parser can panic on a malformed PDF: that is an error for the whole file,
/// or a note in a page's place, never a crash of the service.
fn pages_text(bytes: &[u8], limit: usize) -> Result<(String, bool, u32), Error> {
    let document = std::panic::catch_unwind(|| {
        let mut document = pdf_extract::Document::load_mem(bytes)?;
        if document.is_encrypted() {
            document.decrypt("")?;
        }
        Ok::<_, pdf_extract::Error>(document)
    })
    .map_err(|_| refuse(ErrorCode::InvalidInput, "the PDF could not be parsed"))?
    .map_err(|e| Error::new(ErrorCode::InvalidInput, format!("extract: {e}")))?;
    let numbers: Vec<u32> = document.get_pages().into_keys().collect();
    let pages = u32::try_from(numbers.len()).unwrap_or(u32::MAX);
    let (mut text, mut chars) = (String::new(), 0);
    for number in numbers {
        let mut page = String::new();
        let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::output_doc_page(&document, &mut pdf_extract::PlainTextOutput::new(&mut page), number)
        }));
        let body = match read {
            Ok(Ok(())) => page.trim_matches('\n').to_owned(),
            _ => "[the text of this page could not be read]".to_owned(),
        };
        let block = format!("{}\n{body}\n", page_marker(number, pages));
        let count = block.chars().count();
        if chars + count > limit {
            text.extend(block.chars().take(limit - chars));
            return Ok((text, true, pages));
        }
        chars += count;
        text.push_str(&block);
    }
    Ok((text, false, pages))
}

fn io_code(e: &std::io::Error) -> ErrorCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::ServiceError,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::ServiceError,
    }
}

/// Provides the endpoint and serves extractions until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let service =
        PdfService::open(&options.root, options.max_chars).map_err(|e| format!("open root: {e}"))?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    // As a project's host, its router forwards nothing before this.
    kernel.announce_ready().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

    loop {
        match kernel.next_event().map_err(|e| e.to_string())? {
            Event::Dispatch(d) => {
                let result = serve(&service, d.action.as_str(), &d.resource, &d.payload);
                kernel.reply(d.id, result).map_err(|e| e.to_string())?;
            }
            Event::CancelDispatch(_) | Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

fn serve(
    service: &PdfService,
    action: &str,
    resource: &ResourcePath,
    payload: &Payload,
) -> Result<Payload, Error> {
    if action != ACTION_EXTRACT {
        return Err(refuse(ErrorCode::InvalidInput, "unknown action"));
    }
    let request: ExtractRequest = payload.decode()?;
    Payload::encode(&service.extract(resource, &request)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A minimal, valid one-page PDF whose only content draws `text`, with a
    /// correct cross-reference table so any conformant parser can read it.
    fn minimal_pdf(text: &str) -> Vec<u8> {
        pdf_of_pages(&[text])
    }

    /// A valid PDF with one page per text, each drawing its text.
    fn pdf_of_pages(texts: &[&str]) -> Vec<u8> {
        // Objects: 1 catalog, 2 pages, 3 font, then a page and its content per text.
        let kids: Vec<String> = (0..texts.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
        let mut objects = vec![
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            format!("<</Type/Pages/Kids[{}]/Count {}>>", kids.join(" "), texts.len()),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
        ];
        for (i, text) in texts.iter().enumerate() {
            let content = format!("BT /F1 24 Tf 20 100 Td ({text}) Tj ET");
            objects.push(format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 144]/Contents {} 0 R/Resources<</Font<</F1 3 0 R>>>>>>",
                5 + 2 * i
            ));
            objects.push(format!("<</Length {}>>stream\n{content}\nendstream", content.len()));
        }
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj{object}endobj\n", i + 1));
        }
        let xref = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
        for offset in &offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!("trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF", objects.len() + 1));
        pdf.into_bytes()
    }

    fn service_over(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PdfService) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        for (name, bytes) in files {
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        let service = PdfService::open(dir.path(), DEFAULT_MAX_CHARS).unwrap();
        (dir, service)
    }

    fn path(segments: &[&str]) -> ResourcePath {
        ResourcePath::new(segments.iter().copied()).unwrap()
    }

    #[test]
    fn extracts_the_text_of_a_pdf() {
        let (_dir, service) = service_over(&[("work/report.pdf", &minimal_pdf("Milestone shipped"))]);
        let got =
            service.extract(&path(&["work", "report.pdf"]), &ExtractRequest { max_chars: None }).unwrap();
        let flat: String = got.text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("Milestone shipped"), "unexpected text: {:?}", got.text);
        assert!(!got.truncated);
    }

    #[test]
    fn the_text_comes_page_by_page_after_each_pages_marker() {
        let (_dir, service) =
            service_over(&[("work/two.pdf", &pdf_of_pages(&["First page", "Second page"]))]);
        let got = service.extract(&path(&["work", "two.pdf"]), &ExtractRequest { max_chars: None }).unwrap();
        assert_eq!(got.pages, 2);
        let first = got.text.find("[page 1 of 2]\n").unwrap();
        let second = got.text.find("[page 2 of 2]\n").unwrap();
        let flat = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat(&got.text[first..second]).contains("First page"), "{:?}", got.text);
        assert!(flat(&got.text[second..]).contains("Second page"), "{:?}", got.text);
    }

    /// The kernel checked the caller's scope against the literal path; a link
    /// beneath the root, to a file or through a directory, would read a file
    /// in another scope.
    #[test]
    fn a_symbolic_link_in_the_path_is_refused() {
        let (dir, service) = service_over(&[("work/real.pdf", &minimal_pdf("Secret"))]);
        std::os::unix::fs::symlink("real.pdf", dir.path().join("work/link.pdf")).unwrap();
        std::os::unix::fs::symlink("work", dir.path().join("alias")).unwrap();
        for linked in [&["work", "link.pdf"][..], &["alias", "real.pdf"]] {
            let got = service.extract(&path(linked), &ExtractRequest { max_chars: None });
            assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::PermissionDenied), "{linked:?}");
        }
        assert!(service.extract(&path(&["work", "real.pdf"]), &ExtractRequest { max_chars: None }).is_ok());
    }

    #[test]
    fn a_short_limit_truncates() {
        let (_dir, service) = service_over(&[("work/report.pdf", &minimal_pdf("Milestone shipped"))]);
        let got =
            service.extract(&path(&["work", "report.pdf"]), &ExtractRequest { max_chars: Some(4) }).unwrap();
        assert!(got.truncated);
        assert!(got.text.chars().count() <= 4);
    }

    #[test]
    fn a_non_pdf_file_is_an_error_not_a_crash() {
        let (_dir, service) = service_over(&[("work/notes.txt", b"just text, not a pdf")]);
        let got = service.extract(&path(&["work", "notes.txt"]), &ExtractRequest { max_chars: None });
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::InvalidInput));
    }

    #[test]
    fn a_missing_file_and_an_empty_path_are_refused() {
        let (_dir, service) = service_over(&[]);
        assert!(service.extract(&path(&["work", "gone.pdf"]), &ExtractRequest { max_chars: None }).is_err());
        assert_eq!(
            service.extract(&path(&[]), &ExtractRequest { max_chars: None }).map_err(|e| e.code),
            Err(ErrorCode::InvalidInput)
        );
    }
}
