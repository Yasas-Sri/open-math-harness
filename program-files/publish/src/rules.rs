//! The scan rules, version 1: what in a text looks like a secret.
//!
//! Each rule is a hand-written matcher over bytes, linear in the text, with
//! no backtracking: what it reads is sessions' records, which are untrusted
//! input. A rule reports where the secret is and where the text
//! that names it starts; nothing here keeps or prints a secret.
//!
//! The rules look for shapes. They miss a secret with no known shape, one
//! that is encoded or split, anything in a binary file, an image or a PDF,
//! and what is private without being a credential (names, hosts, a page of a
//! private document); and they flag a value that only looks like one. No
//! rule matches a bare hexadecimal string, a UUID, a commit id or a checksum.
//! The owner decides each finding.
//!
//! What each rule must match and must not are the two test tables below
//! (`each_rule_finds_its_secret`, `near_misses_are_not_findings`). Change a
//! rule together with them and the interface's `RULES_VERSION`, which makes
//! the platform scan anew what an earlier version scanned.

use std::ops::Range;

/// One rule's match in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub rule: &'static str,
    /// The whole match: where a preview starts and ends around the secret.
    pub at: Range<usize>,
    /// The secret itself: masked in every preview, and what a fingerprint is of.
    pub secret: Range<usize>,
}

/// Every match of every rule in `text`.
pub fn scan(text: &[u8]) -> Vec<Match> {
    let mut found = Vec::new();
    private_keys(text, &mut found);
    for shape in SHAPES {
        shape.matches(text, &mut found);
    }
    web_tokens(text, &mut found);
    authorization(text, &mut found);
    url_credentials(text, &mut found);
    assignments(text, &mut found);
    found
}

/// Where `needle` occurs in `text`; with `fold`, whatever its ASCII case
/// (`needle` is then lowercase).
fn occurrences<'a>(text: &'a [u8], needle: &'a [u8], fold: bool) -> impl Iterator<Item = usize> + 'a {
    let first = needle.first().copied().unwrap_or(0);
    let same = move |a: u8, b: u8| if fold { a.to_ascii_lowercase() == b } else { a == b };
    let mut from = 0;
    std::iter::from_fn(move || {
        while !needle.is_empty() && from + needle.len() <= text.len() {
            let at = from + text[from..].iter().position(|b| same(*b, first))?;
            from = at + 1;
            let candidate = text.get(at..at + needle.len())?;
            if candidate.iter().zip(needle).all(|(a, b)| same(*a, *b)) {
                return Some(at);
            }
        }
        None
    })
}

/// The end of the longest run of bytes in `set` starting at `from`.
fn run(text: &[u8], from: usize, set: fn(u8) -> bool) -> usize {
    run_within(text, from, set, usize::MAX)
}

/// [`run`], looking at no more than `limit` bytes: for a run that may hold
/// the next start of a match, so that a text of nothing but starts stays
/// linear to scan.
fn run_within(text: &[u8], from: usize, set: fn(u8) -> bool, limit: usize) -> usize {
    from + text.get(from..).map_or(0, |rest| rest.iter().take(limit).take_while(|b| set(**b)).count())
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn upper_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}
fn alnum_underscore(b: u8) -> bool {
    alnum(b) || b == b'_'
}
fn alnum_dash(b: u8) -> bool {
    alnum(b) || b == b'-'
}
/// The base64url alphabet.
fn url_safe(b: u8) -> bool {
    alnum(b) || b == b'_' || b == b'-'
}
fn blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// A value nobody keeps secret: a word that stands for one, a template, a
/// filler.
fn placeholder(value: &[u8]) -> bool {
    const PARTS: [&[u8]; 12] = [
        b"example",
        b"placeholder",
        b"changeme",
        b"change_me",
        b"redacted",
        b"your_",
        b"your-",
        b"dummy",
        b"${",
        b"{{",
        b"...",
        b"<",
    ];
    const WORDS: [&[u8]; 13] = [
        b"password",
        b"passwd",
        b"pass",
        b"secret",
        b"token",
        b"none",
        b"null",
        b"true",
        b"false",
        b"undefined",
        b"required",
        b"username",
        b"user",
    ];
    let lower: Vec<u8> = value.iter().take(512).map(u8::to_ascii_lowercase).collect();
    lower.windows(2).all(|pair| pair[0] == pair[1])
        || lower.iter().all(|b| matches!(b, b'x' | b'*' | b'.' | b'-' | b'_' | b'#'))
        || PARTS.iter().any(|part| occurrences(&lower, part, false).next().is_some())
        || WORDS.contains(&lower.as_slice())
}

fn mixed(value: &[u8]) -> bool {
    value.iter().any(u8::is_ascii_digit) && value.iter().any(u8::is_ascii_alphabetic)
}

/// A provider's key: a fixed prefix, then a run of its alphabet of a known
/// length, standing alone (no letter or digit touches it), and no placeholder
/// (AWS's documented `…EXAMPLE` key is none).
struct Shape {
    rule: &'static str,
    prefix: &'static str,
    set: fn(u8) -> bool,
    min: usize,
    max: usize,
    /// The run holds a digit and a letter: what tells a key from a word.
    mixed: bool,
}

const SHAPES: &[Shape] = &[
    Shape { rule: "aws-access-key", prefix: "AKIA", set: upper_digit, min: 16, max: 16, mixed: false },
    Shape { rule: "aws-access-key", prefix: "ASIA", set: upper_digit, min: 16, max: 16, mixed: false },
    Shape { rule: "github-token", prefix: "ghp_", set: alnum, min: 36, max: 255, mixed: false },
    Shape { rule: "github-token", prefix: "gho_", set: alnum, min: 36, max: 255, mixed: false },
    Shape { rule: "github-token", prefix: "ghu_", set: alnum, min: 36, max: 255, mixed: false },
    Shape { rule: "github-token", prefix: "ghs_", set: alnum, min: 36, max: 255, mixed: false },
    Shape { rule: "github-token", prefix: "ghr_", set: alnum, min: 36, max: 255, mixed: false },
    Shape {
        rule: "github-token",
        prefix: "github_pat_",
        set: alnum_underscore,
        min: 22,
        max: 255,
        mixed: true,
    },
    Shape { rule: "gitlab-token", prefix: "glpat-", set: url_safe, min: 20, max: 255, mixed: false },
    Shape { rule: "sk-key", prefix: "sk-", set: url_safe, min: 20, max: 255, mixed: true },
    Shape { rule: "slack-token", prefix: "xoxb-", set: alnum_dash, min: 10, max: 255, mixed: true },
    Shape { rule: "slack-token", prefix: "xoxa-", set: alnum_dash, min: 10, max: 255, mixed: true },
    Shape { rule: "slack-token", prefix: "xoxp-", set: alnum_dash, min: 10, max: 255, mixed: true },
    Shape { rule: "slack-token", prefix: "xoxr-", set: alnum_dash, min: 10, max: 255, mixed: true },
    Shape { rule: "slack-token", prefix: "xoxs-", set: alnum_dash, min: 10, max: 255, mixed: true },
    Shape { rule: "google-api-key", prefix: "AIza", set: url_safe, min: 35, max: 35, mixed: false },
    Shape { rule: "stripe-key", prefix: "sk_live_", set: alnum, min: 16, max: 255, mixed: false },
    Shape { rule: "stripe-key", prefix: "sk_test_", set: alnum, min: 16, max: 255, mixed: false },
    Shape { rule: "stripe-key", prefix: "rk_live_", set: alnum, min: 16, max: 255, mixed: false },
    Shape { rule: "stripe-key", prefix: "rk_test_", set: alnum, min: 16, max: 255, mixed: false },
    Shape { rule: "huggingface-token", prefix: "hf_", set: alnum, min: 30, max: 255, mixed: false },
    Shape { rule: "npm-token", prefix: "npm_", set: alnum, min: 36, max: 36, mixed: false },
    Shape {
        rule: "pypi-token",
        prefix: "pypi-AgEIcHlwaS5vcmc",
        set: url_safe,
        min: 50,
        max: 4096,
        mixed: false,
    },
    Shape { rule: "nucleus-session", prefix: "nuc_sess_", set: url_safe, min: 16, max: 255, mixed: false },
];

impl Shape {
    fn matches(&self, text: &[u8], found: &mut Vec<Match>) {
        for at in occurrences(text, self.prefix.as_bytes(), false) {
            let start = at + self.prefix.len();
            // One byte past the longest key: a longer run is not one.
            let end = run_within(text, start, self.set, self.max + 1);
            let touched = at > 0 && alnum(text[at - 1]) || text.get(end).is_some_and(|b| alnum(*b));
            if !touched
                && (self.min..=self.max).contains(&(end - start))
                && (!self.mixed || mixed(&text[start..end]))
                && !placeholder(&text[at..end])
            {
                found.push(Match { rule: self.rule, at: at..end, secret: at..end });
            }
        }
    }
}

/// Longest body of a private key, to its `END` line; and what is taken for
/// the body of one whose end is not within that.
const MAX_KEY_BODY: usize = 16 * 1024;
const UNENDED_KEY_BODY: usize = 256;

/// `-----BEGIN … PRIVATE KEY-----` (any label that ends so, `PRIVATE KEY
/// BLOCK` too) and what follows it, to its `-----END`, with at least 32 base64
/// characters: never a public key, a certificate, or a header with a
/// placeholder under it.
fn private_keys(text: &[u8], found: &mut Vec<Match>) {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"-----END";
    // The next `END` at or after the last body looked at; `None` once there
    // is no further one, so the text is searched for them once in all.
    let mut next_end = Some(0);
    for at in occurrences(text, BEGIN, false) {
        let label = at + BEGIN.len();
        let label_end = run(text, label, |b| upper_digit(b) || b == b' ');
        let name = &text[label..label_end];
        let private = name.ends_with(b"PRIVATE KEY") || name.ends_with(b"PRIVATE KEY BLOCK");
        if !private || name.len() > 40 || !text[label_end..].starts_with(b"-----") {
            continue;
        }
        let body = label_end + 5;
        while next_end.is_some_and(|end| end < body || !text[end..].starts_with(END)) {
            next_end = occurrences(&text[body..], END, false).next().map(|found| body + found);
        }
        // Without its end within reach, the lines right after the header.
        let end = next_end
            .filter(|end| end - body <= MAX_KEY_BODY)
            .unwrap_or_else(|| text.len().min(body + UNENDED_KEY_BODY));
        // A key, not a template of one: enough base64 between the lines.
        if text[body..end].iter().filter(|b| alnum(**b) || matches!(b, b'+' | b'/' | b'=')).count() >= 32
            && !placeholder(&text[body..end])
        {
            found.push(Match { rule: "private-key", at: at..end, secret: body..end });
        }
    }
}

/// A JSON web token: three base64url parts, the first two JSON objects.
fn web_tokens(text: &[u8], found: &mut Vec<Match>) {
    for at in occurrences(text, b"eyJ", false) {
        if at > 0 && (url_safe(text[at - 1]) || text[at - 1] == b'.') {
            continue;
        }
        let header = run(text, at, url_safe);
        let payload = run(text, header + 1, url_safe);
        let signature = run(text, payload + 1, url_safe);
        let dotted = text.get(header) == Some(&b'.') && text.get(payload) == Some(&b'.');
        if dotted
            && header - at >= 13
            && text[header + 1..].starts_with(b"eyJ")
            && payload - header > 13
            && signature - payload > 10
        {
            found.push(Match { rule: "jwt", at: at..signature, secret: at..signature });
        }
    }
}

/// Past one quote at `at`, as a text or as a JSON string inside one writes it.
fn past_quote(text: &[u8], at: usize) -> usize {
    match text.get(at..) {
        Some([b'\\', b'"' | b'\'', ..]) => at + 2,
        Some([b'"' | b'\'' | b'`', ..]) => at + 1,
        _ => at,
    }
}

fn past_blanks(text: &[u8], at: usize) -> usize {
    run(text, at, blank)
}

/// `Authorization: Bearer <token>`, also `Basic` and `Token`, `:` or `=`,
/// with a credential of at least 20 characters, or 8 with a digit: never
/// `Bearer <token>` itself, `Bearer $VAR` or `Bearer YOUR_…`.
fn authorization(text: &[u8], found: &mut Vec<Match>) {
    const NAME: &[u8] = b"authorization";
    fn credential(b: u8) -> bool {
        alnum(b) || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
    }
    for at in occurrences(text, NAME, true) {
        let mut next = past_blanks(text, past_quote(text, at + NAME.len()));
        if !matches!(text.get(next), Some(b':' | b'=')) {
            continue;
        }
        next = past_quote(text, past_blanks(text, next + 1));
        let Some(scheme) = [&b"bearer"[..], b"basic", b"token"].into_iter().find(|scheme| {
            text.get(next..next + scheme.len()).is_some_and(|word| word.eq_ignore_ascii_case(scheme))
                && text.get(next + scheme.len()).is_some_and(|b| blank(*b))
        }) else {
            continue;
        };
        let start = past_blanks(text, next + scheme.len());
        let end = run(text, start, credential);
        let value = &text[start..end];
        let long_enough = value.len() >= 20 || value.len() >= 8 && value.iter().any(u8::is_ascii_digit);
        if long_enough && !placeholder(value) {
            found.push(Match { rule: "authorization-header", at: at..end, secret: start..end });
        }
    }
}

/// `scheme://user:password@host`, with a password of 3 to 256 characters
/// that is no placeholder or template (`${VAR}`).
fn url_credentials(text: &[u8], found: &mut Vec<Match>) {
    fn scheme(b: u8) -> bool {
        alnum(b) || matches!(b, b'+' | b'.' | b'-')
    }
    fn password(b: u8) -> bool {
        (b.is_ascii_graphic() || b >= 0x80)
            && !matches!(b, b'/' | b'@' | b'"' | b'\'' | b'<' | b'>' | b'`' | b'\\')
    }
    fn user(b: u8) -> bool {
        password(b) && b != b':'
    }
    for at in occurrences(text, b"://", false) {
        let start = at - text[at.saturating_sub(16)..at].iter().rev().take_while(|b| scheme(**b)).count();
        if start == at || !text[start].is_ascii_alphabetic() {
            continue;
        }
        let name_end = run(text, at + 3, user);
        if name_end == at + 3 || name_end - (at + 3) > 128 || text.get(name_end) != Some(&b':') {
            continue;
        }
        let secret = name_end + 1..run(text, name_end + 1, password);
        let host = text.get(secret.end) == Some(&b'@')
            && text.get(secret.end + 1).is_some_and(|b| alnum(*b) || *b == b'[');
        let value = &text[secret.clone()];
        let template = matches!(value.first(), Some(b'$' | b'%' | b'{' | b'['));
        if host && (3..=256).contains(&value.len()) && !template && !placeholder(value) {
            found.push(Match { rule: "url-credentials", at: start..secret.end + 1, secret });
        }
    }
}

/// The names whose assignment is looked at: a name that ends with one.
const SECRET_NAMES: [&str; 10] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "secret_key",
    "access_key",
    "private_key",
];
/// Longest quoted value looked through for its closing quote.
const MAX_QUOTED: usize = 512;

/// `password = "…"`, `API_KEY=…`, `"token": "…"`: a name that ends with one
/// of [`SECRET_NAMES`], then `=`, `:`, `:=` or `=>`, then a value that is one
/// word: quoted, of 8 or more characters without whitespace, or unquoted, of
/// 8 or more holding a letter and a digit. Never `max_tokens`, `token_count`
/// or `password_hash`, nor a template, a placeholder, or an unquoted value
/// that is code or a path.
fn assignments(text: &[u8], found: &mut Vec<Match>) {
    fn name(b: u8) -> bool {
        alnum(b) || matches!(b, b'_' | b'.' | b'-')
    }
    // Without `=` and `:`, so that no unquoted value holds the next assignment.
    fn bare(b: u8) -> bool {
        alnum(b) || matches!(b, b'_' | b'+' | b'/' | b'~' | b'-')
    }
    for word in SECRET_NAMES {
        for at in occurrences(text, word.as_bytes(), true) {
            let after = at + word.len();
            if text.get(after).is_some_and(|b| name(*b)) {
                continue;
            }
            let mut next = past_blanks(text, past_quote(text, after));
            next += match text.get(next..) {
                Some([b':', b'=', ..] | [b'=', b'>', ..]) => 2,
                Some([b'=' | b':', ..]) => 1,
                _ => continue,
            };
            next = past_blanks(text, next);
            let quoted = past_quote(text, next);
            let secret = if quoted > next {
                let close = &text[next..quoted];
                let line = &text[quoted..text.len().min(quoted + MAX_QUOTED)];
                let line = &line[..line.iter().position(|b| *b == b'\n').unwrap_or(line.len())];
                let Some(length) = occurrences(line, close, false).next() else { continue };
                let value = &line[..length];
                let template = matches!(value.first(), Some(b'$' | b'%' | b'{' | b'<'));
                if value.len() < 8 || value.iter().any(u8::is_ascii_whitespace) || template {
                    continue;
                }
                quoted..quoted + length
            } else {
                let end = run(text, next, bare);
                // Base64 padding belongs to the value.
                let end = run_within(text, end, |b| b == b'=', 2);
                let value = &text[next..end];
                // A call, a field or an index is code, and a path is no secret.
                let code = matches!(text.get(end), Some(b'(' | b'.' | b'[')) || value.first() == Some(&b'/');
                if value.len() < 8 || !mixed(value) || code {
                    continue;
                }
                next..end
            };
            if !placeholder(&text[secret.clone()]) {
                let start =
                    at - text[at.saturating_sub(40)..at].iter().rev().take_while(|b| name(**b)).count();
                found.push(Match { rule: "secret-assignment", at: start..secret.end, secret });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The rules that match `text`, each with the secret it found.
    fn found(text: &str) -> Vec<(&'static str, &str)> {
        let mut matches: Vec<(&'static str, &str)> =
            scan(text.as_bytes()).into_iter().map(|m| (m.rule, &text[m.secret])).collect();
        matches.sort_unstable();
        matches.dedup();
        matches
    }

    /// Every rule finds its secret, and exactly it: the secret span is what
    /// a preview masks and a fingerprint is made of.
    #[test]
    fn each_rule_finds_its_secret() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB\n-----END OPENSSH PRIVATE KEY-----";
        let body = "\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB\n";
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let github = format!("ghp_{}", "a1B2".repeat(9));
        let fine_grained = format!("github_pat_{}", "11ABCDEFG0_abcdefghijk");
        let google = format!("AIza{}", "Sy0123456789abcdefghijklmnopqrstuvw");
        let npm = format!("npm_{}", "abcdefghijklmnopqrstuvwxyz0123456789");
        let pypi = format!("pypi-AgEIcHlwaS5vcmc{}", "A1b2".repeat(13));
        for (text, rule, secret) in [
            (pem, "private-key", body),
            ("aws_id AKIAIOSFODNN7ABCDEFG here", "aws-access-key", "AKIAIOSFODNN7ABCDEFG"),
            (github.as_str(), "github-token", github.as_str()),
            (fine_grained.as_str(), "github-token", fine_grained.as_str()),
            ("use glpat-AbCdEfGhIjKlMnOpQrSt now", "gitlab-token", "glpat-AbCdEfGhIjKlMnOpQrSt"),
            ("key: sk-proj-Ab12Cd34Ef56Gh78Ij90", "sk-key", "sk-proj-Ab12Cd34Ef56Gh78Ij90"),
            ("sk-0123456789abcdef0123456789abcdef", "sk-key", "sk-0123456789abcdef0123456789abcdef"),
            ("xoxb-123456789012-abcdefABCDEF", "slack-token", "xoxb-123456789012-abcdefABCDEF"),
            (google.as_str(), "google-api-key", google.as_str()),
            ("sk_live_4eC39HqLyjWDarjtT1zdp7dc", "stripe-key", "sk_live_4eC39HqLyjWDarjtT1zdp7dc"),
            (
                "hf_AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGh",
                "huggingface-token",
                "hf_AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGh",
            ),
            (npm.as_str(), "npm-token", npm.as_str()),
            (pypi.as_str(), "pypi-token", pypi.as_str()),
            (jwt, "jwt", jwt),
            ("nuc_sess_AbCdEfGh01234567", "nucleus-session", "nuc_sess_AbCdEfGh01234567"),
            ("curl -H 'Authorization: Bearer abc123def456ghi'", "authorization-header", "abc123def456ghi"),
            (
                r#"{"headers":{"authorization":"Basic dXNlcjpodW50ZXIy"}}"#,
                "authorization-header",
                "dXNlcjpodW50ZXIy",
            ),
            (r#"{\"Authorization\": \"Bearer tok3n-value-1\"}"#, "authorization-header", "tok3n-value-1"),
            ("postgres://admin:s3cr3t-pw@db.internal:5432/app", "url-credentials", "s3cr3t-pw"),
            ("https://ci:9f8e7d6c5b@git.example.org/repo.git", "url-credentials", "9f8e7d6c5b"),
            ("DB_PASSWORD=hunter2hunter2", "secret-assignment", "hunter2hunter2"),
            ("password = \"correct-horse-battery\"", "secret-assignment", "correct-horse-battery"),
            (r#"{"client_secret": "Zx81kq0PmmT4"}"#, "secret-assignment", "Zx81kq0PmmT4"),
            (r#"{\"api_key\":\"Zx81kq0PmmT4\"}"#, "secret-assignment", "Zx81kq0PmmT4"),
            ("auth_token: 3f2a9c1b7d4e5f60718293a4", "secret-assignment", "3f2a9c1b7d4e5f60718293a4"),
            ("export API-KEY := 'q7w8e9r0t1y2'", "secret-assignment", "q7w8e9r0t1y2"),
        ] {
            assert!(found(text).contains(&(rule, secret)), "{rule} in {text:?}: {:?}", found(text));
        }
    }

    /// What only looks like a secret: a public key, a hash, a word that
    /// stands for a value, code that reads one. None is a finding, or every
    /// project would be withheld whole.
    #[test]
    fn near_misses_are_not_findings() {
        let short_github = format!("ghp_{}", "a1B2".repeat(8));
        let glued = format!("xghp_{}", "a1B2".repeat(9));
        for text in [
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA\n-----END PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIEAgAAuTANBgkqhkiG9w0BAQUFADBa\n-----END CERTIFICATE-----",
            "-----BEGIN RSA PRIVATE KEY-----\n<paste your key here>\n-----END RSA PRIVATE KEY-----",
            "a key starts with BEGIN PRIVATE KEY and ends with END PRIVATE KEY",
            // A commit, a checksum, a UUID: hexadecimal that names no secret.
            "commit 3f2a9c1b7d4e5f60718293a4b5c6d7e8f9012345",
            "sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "id = 123e4567-e89b-12d3-a456-426614174000",
            "AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7ABCDEFGH is one character too long",
            short_github.as_str(),
            glued.as_str(),
            "risk-averse-and-task-oriented-planning-2024",
            "sk-learn-is-not-the-name-of-the-package",
            "hf_hub_download(repo_id, filename)",
            "eyJhbGciOiJIUzI1NiJ9 alone is one part",
            "Authorization: Bearer <token>",
            "Authorization: Bearer $OPENAI_API_KEY",
            "Authorization: Bearer YOUR_API_KEY_HERE1",
            "the authorization header carries a bearer token",
            "http://localhost:8080/path?mail=a@b.org",
            "git@github.com:org/repo.git",
            "https://user:password@host.example/",
            "https://user:${PASSWORD}@host/",
            "mailto:someone@example.org",
            "max_tokens: 4096abcd",
            "token_count = 1024abcd9",
            "password_hash = \"5f4dcc3b5aa765d61d8327deb882cf99\"",
            "password = getpass.getpass()",
            "token = response.json()[\"token\"]",
            "api_key = os.environ[\"OPENAI_API_KEY\"]",
            "secret: Optional[str] = None",
            "password = \"Enter your password\"",
            "password: \"${DB_PASSWORD}\"",
            "PASSWORD=changeme",
            "token = \"xxxxxxxxxxxx\"",
            "if password == stored1234:",
            "PRIVATE_KEY=/etc/ssl/private/server1.key",
            "the password is long and the secret is kept",
        ] {
            assert_eq!(found(text), [], "{text:?}");
        }
    }

    /// Hostile text costs no more than its length: nothing here loops on it.
    #[test]
    fn a_text_of_nothing_but_starts_of_matches_is_scanned_once() {
        let mut text = "-----BEGIN PRIVATE KEY-----".repeat(20_000);
        text.push_str(&"token=".repeat(50_000));
        text.push_str(&"sk-".repeat(50_000));
        text.push_str(&"://a:".repeat(50_000));
        text.push_str(&" eyJ".repeat(50_000));
        let started = std::time::Instant::now();
        let found = scan(text.as_bytes());
        assert!(found.iter().all(|m| m.rule == "private-key"), "only the headers look like anything");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    }
}
