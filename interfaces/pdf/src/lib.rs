//! The `pdf` service interface, version 2: read a PDF as text.
//!
//! The **resource path is the file**, the same spelling the filesystem uses
//! (`["work", "report.pdf"]`). A capability scoped to `["work"]` may read PDFs
//! under `work` and nothing else; the kernel checks that before the provider is
//! reached, and the provider follows no symbolic link in the path, so the file
//! it reads is the one the scope was checked against.
//!
//! | Action | Payload | Result |
//! | --- | --- | --- |
//! | `extract` | [`ExtractRequest`] | [`ExtractResponse`] |
//!
//! `extract` is an observation: it reads a file and returns its text, extracted
//! from a text-bearing PDF. It does not do OCR, so a scanned (image-only) PDF
//! yields little or nothing. The text is truncated to a bounded size.
//!
//! The text comes page by page, each page after a line [`page_marker`]
//! (`[page 2 of 6]`), and the reply says how many pages the PDF has.
//!
//! Extracted text is untrusted input, exactly like the file's bytes.
//! This crate defines data only.

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, InterfaceDecl, ResourceFrom,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "pdf";
pub const ACTION_EXTRACT: &str = "extract";
pub const VERSION: u32 = 2;
/// Largest PDF a provider reads.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Payload of `extract`. The file is the call's resource path; this carries an
/// optional cap on how much text to return.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractRequest {
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// Result of `extract`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractResponse {
    pub text: String,
    /// True when the text was cut off at the size limit.
    pub truncated: bool,
    /// How many pages the PDF has, whether or not the text reaches them
    /// all.
    pub pages: u32,
}

/// The line before page `page` (1-based) of a PDF of `pages` pages in the
/// text of `extract`.
#[must_use]
pub fn page_marker(page: u32, pages: u32) -> String {
    format!("[page {page} of {pages}]")
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([(
            ActionName::new(ACTION_EXTRACT)?,
            ActionDecl::tool(
                EffectClass::Observation,
                "Read a PDF file from the workspace and return its text, page by page, each page after a line [page N of M].",
                json!({"type": "object",
                    "properties": {"path": {"type": "string", "description": "Path to a PDF in the workspace, e.g. work/report.pdf"}},
                    "required": ["path"]}),
                ResourceFrom::Segments { param: "path".to_owned() },
            ),
        )]),
    })
}
