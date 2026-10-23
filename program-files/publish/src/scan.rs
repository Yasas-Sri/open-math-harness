//! What a record's scan finds: every match of the rules in every text of the
//! record, each distinct (rule, secret) once, with a masked preview and a
//! salted fingerprint.

use std::borrow::Cow;
use std::ops::Range;

use nucleus_publish_interface::{Finding, MAX_PREVIEW_BYTES, MAX_RECORD_FINDINGS};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::rules::{self, Match};

/// What a page is scanned with: the platform's salt for the project.
pub struct Scanner {
    pub salt: String,
}

/// Lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

/// A record as a scan reads it: an image a record carries in `images`, one
/// a message carries inline (its name and media type) or one of the project
/// an agent's reply shows (its path and media type), is read without its
/// bytes; a scan reads text, and base64 is no text. A reply's
/// `images_refused` is text and read whole.
fn readable(record: &Value) -> Cow<'_, Value> {
    let Some(fields) = record.as_object().filter(|fields| fields.contains_key("images")) else {
        return Cow::Borrowed(record);
    };
    let without_data = |image: &Value| match image {
        Value::Object(image) => Value::Object(
            image
                .iter()
                .filter(|(name, _)| *name != "data_base64")
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    };
    let fields = fields.iter().map(|(name, value)| {
        let value = match (name.as_str(), value) {
            ("images", Value::Array(images)) => Value::Array(images.iter().map(without_data).collect()),
            _ => value.clone(),
        };
        (name.clone(), value)
    });
    Cow::Owned(Value::Object(fields.collect()))
}

/// Every text of a record a scan reads: each string in it on its own, as it
/// was written, and each field that is an object or an array as the JSON it
/// is, where a name and its value stand side by side (a tool call's
/// arguments); of an image only its name or path and type ([`readable`]).
pub fn texts(record: &Value) -> Vec<String> {
    let record = readable(record);
    let record = record.as_ref();
    let mut found = Vec::new();
    strings(record, &mut found);
    let mut texts: Vec<String> = found.into_iter().map(str::to_owned).collect();
    if let Some(fields) = record.as_object() {
        texts.extend(fields.values().filter(|v| v.is_object() || v.is_array()).map(Value::to_string));
    }
    texts
}

impl Scanner {
    fn fingerprint(&self, rule: &str, secret: &[u8]) -> String {
        let mut hash = Sha256::new();
        hash.update(self.salt.as_bytes());
        for part in [rule.as_bytes(), secret] {
            hash.update([0]);
            hash.update(part);
        }
        hex(&hash.finalize())
    }

    /// The findings of one record, each distinct (rule, fingerprint) once,
    /// at most [`MAX_RECORD_FINDINGS`]; and whether it holds more.
    pub fn record(&self, record: &Value) -> (Vec<Finding>, bool) {
        let mut findings: Vec<Finding> = Vec::new();
        for text in texts(record) {
            let text = text.as_bytes();
            let matches = rules::scan(text);
            let masks = masks(&matches);
            for found in &matches {
                let fingerprint = self.fingerprint(found.rule, &text[found.secret.clone()]);
                if findings.iter().any(|f| f.rule == found.rule && f.fingerprint == fingerprint) {
                    continue;
                }
                if findings.len() == MAX_RECORD_FINDINGS {
                    return (findings, true);
                }
                findings.push(Finding {
                    rule: found.rule.to_owned(),
                    preview: preview(text, found, &masks),
                    fingerprint,
                });
            }
        }
        (findings, false)
    }
}

/// Every secret's span in a text, merged and in order.
fn masks(matches: &[Match]) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = matches.iter().map(|found| found.secret.clone()).collect();
    spans.sort_unstable_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Bytes of the line shown before a match, and after its secret.
const PREVIEW_BEFORE: usize = 40;
const PREVIEW_AFTER: usize = 24;

/// The line around a match, for the owner to recognise the place: every
/// secret any rule found there is replaced by `****`, this match's and its
/// neighbours' alike.
fn preview(text: &[u8], found: &Match, masks: &[Range<usize>]) -> String {
    let line = text[..found.at.start].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let start = line.max(found.at.start.saturating_sub(PREVIEW_BEFORE));
    let rest = &text[found.secret.end..];
    let end =
        found.secret.end + rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len()).min(PREVIEW_AFTER);
    let mut shown = Vec::new();
    let mut at = start;
    while at < end {
        let next = masks.partition_point(|mask| mask.end <= at);
        match masks.get(next).filter(|mask| mask.start <= at) {
            Some(mask) => {
                shown.extend_from_slice(b"****");
                at = mask.end;
            }
            None => {
                shown.push(if text[at].is_ascii_control() { b' ' } else { text[at] });
                at += 1;
            }
        }
    }
    let mut shown = String::from_utf8_lossy(&shown).into_owned();
    while shown.len() > MAX_PREVIEW_BYTES {
        shown.pop();
    }
    shown
}
