//! A confined web service: the provider of the `web` interface.
//!
//! This small process, granted the network and nothing else, fetches for the
//! agents that hold `web`. `fetch` GETs a URL (following redirects) and returns
//! the page as text; `curl` makes an arbitrary request and returns the raw
//! response. That is all it is — a fetcher behind a capability, not a policy
//! engine. In the VM it has no binds and no grant but `provide` on its own
//! endpoint: a compromised instance holds no file and no credential, but can
//! reach and disclose to any public host. `curl` also sends POST, PUT, PATCH
//! and DELETE, so a caller granted it can change remote state.
//!
//! Three plain safeguards, each because it is cheap and prevents an obvious
//! mishap, not because the interface demands it:
//! - a size cap and a time cap, so a huge page or a stuck server cannot take
//!   the process down;
//! - a refusal to reach loopback/private/link-local addresses, so the agent
//!   cannot be pointed at services on this same machine (there is no egress
//!   proxy, so a granted network otherwise reaches anything). Redirects are
//!   followed by hand, each hop re-admitted, and every resolved address is
//!   checked before connecting.
//!
//! **A site's refusal is reported as a refusal.** A human check served in place
//! of the page (a reCAPTCHA challenge page, a Cloudflare challenge, a captcha
//! such as hCaptcha on a 403 or 429), a redirect to a human gate (a path
//! segment named `gate`, such as LMFDB's `/gate.html`, or ending in `captcha`)
//! and any 429 come back as a `permission_denied` error that names the site and
//! says not to retry or work around it, or, for a 429 with a `Retry-After`, to
//! wait that long once and try again; never as the page. Returned as content, a challenge page reads as data, and a model
//! retries it with a forged user agent or cookie. Recognition is Cloudflare's
//! `cf-mitigated` header and a few marks (`CHALLENGE_PAGES`, `CAPTCHAS`)
//! searched only in small HTML pages, so an ordinary page that discusses
//! captchas or has one on a form is content. This is a courtesy to the sites,
//! not a boundary: an agent's shell may have network of its own, and the
//! agents' instructions are what keep them from working around a refusal.
//!
//! It fetches any public host; a capability scoped to a host (its resource
//! path) is honoured. Page content is untrusted input.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, KERNEL_ENDPOINT, MAX_FRAME_BYTES, Payload,
};
use nucleus_sdk::{Connection, Event};
use nucleus_web_interface::{
    ACTION_CURL, ACTION_FETCH, CurlRequest, CurlResponse, ENDPOINT, FetchRequest, FetchResponse, host_of,
    interface,
};
use serde::Serialize;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
const HARD_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MAX_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 500;
const REPLY_MARGIN_MS: u64 = 250;
const MAX_REDIRECTS: u32 = 5;
const MAX_ERROR_CHARS: usize = 300;
/// Room kept in a reply frame for everything around the payload.
const REPLY_ENVELOPE_BYTES: usize = 64 * 1024;

/// A challenge page is small. A larger page is content, whatever it mentions.
const CHALLENGE_PAGE_MAX_BYTES: usize = 64 * 1024;
/// Marks only a page served to stop automated clients carries, with the check
/// each names. Lower-case, as the page is searched.
const CHALLENGE_PAGES: &[(&str, &str)] = &[
    ("recaptcha/challengepage", "reCAPTCHA"),
    ("recaptchachallengepageui", "reCAPTCHA"),
    ("<title>just a moment...</title>", "a Cloudflare challenge"),
    ("<title>attention required! | cloudflare</title>", "a Cloudflare challenge"),
];
/// Marks of a captcha. They make a 403 or 429 a human check; on any other
/// page they are content (a form may carry a captcha, an article discuss one).
const CAPTCHAS: &[(&str, &str)] =
    &[("hcaptcha", "hCaptcha"), ("recaptcha", "reCAPTCHA"), ("captcha", "a CAPTCHA")];
/// A `Retry-After` longer than this is not a wait the site named; it is dropped.
const MAX_RETRY_AFTER_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub endpoint: EndpointName,
    pub max_bytes: u64,
    pub max_timeout_ms: u64,
}

impl Options {
    /// Parses `[--endpoint <name>] [--max-bytes <n>] [--max-timeout-ms <n>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut endpoint = ENDPOINT.to_owned();
        let mut max_bytes = DEFAULT_MAX_BYTES;
        let mut max_timeout_ms = DEFAULT_MAX_TIMEOUT_MS;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--endpoint" => endpoint = value,
                "--max-bytes" => max_bytes = value.parse().map_err(|_| "--max-bytes must be a number")?,
                "--max-timeout-ms" => {
                    max_timeout_ms = value.parse().map_err(|_| "--max-timeout-ms must be a number")?;
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let max_bytes = max_bytes.clamp(1, HARD_MAX_BYTES);
        if max_timeout_ms < MIN_TIMEOUT_MS {
            return Err(format!("--max-timeout-ms must be at least {MIN_TIMEOUT_MS}"));
        }
        Ok(Self {
            endpoint: EndpointName::new(&endpoint).map_err(|e| e.to_string())?,
            max_bytes,
            max_timeout_ms,
        })
    }
}

const INTERNAL: &str = "refusing to reach a local or private address";

/// A loopback, private, link-local, shared (CGNAT), unspecified, broadcast or
/// multicast address, including IPv4 addresses embedded in IPv6.
fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [first, second, ..] = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_multicast()
                || first == 0
                || (first == 100 && second & 0xc0 == 64)
        }
        IpAddr::V6(a) => match a.to_ipv4() {
            Some(v4) => is_internal_ip(IpAddr::V4(v4)),
            None => {
                let s = a.segments()[0];
                a.is_multicast() || s & 0xfe00 == 0xfc00 || s & 0xffc0 == 0xfe80
            }
        },
    }
}

/// A local name or a canonical IP literal of an internal address. Other forms
/// (`127.1`, `2130706433`, a public name pointing inward) are caught when
/// [`GuardedResolver`] resolves them.
fn is_internal_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.parse::<IpAddr>().is_ok_and(is_internal_ip)
}

/// Resolves as ureq normally would, then refuses if any resolved address is
/// internal. ureq connects only to the addresses returned here, so the address
/// checked is the address used: there is no second lookup to rebind.
#[derive(Debug)]
struct GuardedResolver {
    /// Tests only: one loopback socket treated as public.
    exempt: Option<SocketAddr>,
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = DefaultResolver::default().resolve(uri, config, timeout)?;
        if addrs.iter().any(|a| Some(*a) != self.exempt && is_internal_ip(a.ip())) {
            return Err(ureq::Error::Io(io::Error::new(io::ErrorKind::PermissionDenied, INTERNAL)));
        }
        Ok(addrs)
    }
}

fn refuse(code: ErrorCode, message: &str) -> Error {
    Error::new(code, message.to_owned())
}

/// The service's whole ability to reach the web.
pub struct WebService {
    agent: ureq::Agent,
    max_bytes: u64,
    max_timeout_ms: u64,
    exempt: Option<SocketAddr>,
}

impl WebService {
    pub fn new(options: &Options) -> Self {
        Self::with_exempt(options, None)
    }

    fn with_exempt(options: &Options, exempt: Option<SocketAddr>) -> Self {
        // Redirects are followed by hand in `send`, so every hop is admitted. No
        // proxy, so the resolver sees the real destination.
        let config =
            ureq::Agent::config_builder().max_redirects(0).http_status_as_error(false).proxy(None).build();
        Self {
            agent: ureq::Agent::with_parts(config, DefaultConnector::default(), GuardedResolver { exempt }),
            max_bytes: options.max_bytes,
            max_timeout_ms: options.max_timeout_ms,
            exempt,
        }
    }

    /// Checks one URL, the request's or a redirect's, against the capability's
    /// host scope and the address guard.
    fn check_url(&self, url: &str, resource_host: Option<&str>) -> Result<(), Error> {
        let host = host_of(url)?;
        if let Some(scoped) = resource_host
            && scoped != host
        {
            return Err(refuse(
                ErrorCode::PermissionDenied,
                "the URL's host is outside this capability's scope",
            ));
        }
        if is_internal_host(&host) && self.exempt.is_none_or(|a| a.ip().to_string() != host) {
            return Err(refuse(ErrorCode::PermissionDenied, INTERNAL));
        }
        Ok(())
    }

    /// Checks the URL is one this service and this caller may reach, and returns
    /// the request's time and size budget.
    fn admit(
        &self,
        url: &str,
        resource_host: Option<&str>,
        max_bytes: Option<u64>,
        remaining_ms: u64,
    ) -> Result<(u64, Duration), Error> {
        self.check_url(url, resource_host)?;
        let budget = remaining_ms.saturating_sub(REPLY_MARGIN_MS).min(self.max_timeout_ms);
        if budget < MIN_TIMEOUT_MS {
            return Err(refuse(ErrorCode::InvalidInput, "deadline is too short for a request"));
        }
        Ok((max_bytes.map_or(self.max_bytes, |n| n.clamp(1, self.max_bytes)), Duration::from_millis(budget)))
    }

    /// Sends a request, following up to [`MAX_REDIRECTS`] redirects by hand so
    /// every hop is checked exactly as the first was. Returns the final URL.
    /// A call scoped to a host does not follow a redirect to another host: it
    /// returns that redirect, and the caller asks for the new URL itself, so the
    /// kernel checks the caller's own authority for that host.
    /// With `https_only`, every hop, the first and each redirect's, must be https.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        resource_host: Option<&str>,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        mut body: Option<&str>,
        timeout: Duration,
        https_only: bool,
    ) -> Result<(String, ureq::http::Response<ureq::Body>), Error> {
        let deadline = Instant::now() + timeout;
        let (mut method, mut url, mut headers) = (method.to_owned(), url.to_owned(), headers.clone());
        for _ in 0..=MAX_REDIRECTS {
            self.check_url(&url, resource_host)?;
            if https_only && !url.starts_with("https://") {
                return Err(refuse(
                    ErrorCode::InvalidInput,
                    &format!("{url} is not https: this download is https only"),
                ));
            }
            let mut builder = ureq::http::Request::builder().method(method.as_str()).uri(&url);
            for (name, value) in &headers {
                builder = builder.header(name, value);
            }
            let invalid = |e: ureq::http::Error| Error::new(ErrorCode::InvalidInput, format!("request: {e}"));
            let left = Some(deadline.saturating_duration_since(Instant::now()));
            // A body-less method must be sent with `()`, or ureq adds an empty body.
            let response = match body {
                Some(b) => {
                    let request = builder.body(b.as_bytes()).map_err(invalid)?;
                    self.agent.run(self.agent.configure_request(request).timeout_global(left).build())
                }
                None => {
                    let request = builder.body(()).map_err(invalid)?;
                    self.agent.run(self.agent.configure_request(request).timeout_global(left).build())
                }
            }
            .map_err(|e| transport("request failed", &e))?;
            let status = response.status().as_u16();
            let (Some(location), 301 | 302 | 303 | 307 | 308) = (header(&response, "location"), status)
            else {
                return Ok((url, response));
            };
            let next = join_url(&url, &location);
            if is_human_gate(&next) {
                return Err(refused(&url, "with a human check (a redirect to a human gate)", None));
            }
            if resource_host.is_some_and(|scoped| host_of(&next).is_ok_and(|h| h != scoped)) {
                return Ok((url, response));
            }
            if host_of(&next)? != host_of(&url)? {
                headers.retain(|n, _| {
                    !["authorization", "cookie", "proxy-authorization"]
                        .contains(&n.to_ascii_lowercase().as_str())
                });
            }
            if status <= 303 && method != "HEAD" {
                method = "GET".to_owned();
                body = None;
            }
            url = next;
        }
        Err(refuse(ErrorCode::ServiceError, "too many redirects"))
    }

    pub fn fetch(
        &self,
        resource_host: Option<&str>,
        request: &FetchRequest,
        remaining_ms: u64,
    ) -> Result<FetchResponse, Error> {
        let (limit, timeout) = self.admit(&request.url, resource_host, None, remaining_ms)?;
        let (url, response) =
            self.send(resource_host, "GET", &request.url, &BTreeMap::new(), None, timeout, false)?;
        let status = response.status().as_u16();
        let content_type = header(&response, "content-type").unwrap_or_default();
        let location = matches!(status, 301 | 302 | 303 | 307 | 308)
            .then(|| header(&response, "location").map(|l| join_url(&url, &l)))
            .flatten();
        let headers = response.headers().clone();
        let (body, truncated) = read_body(response, limit)?;
        if let Some(refusal) = refusal(&url, status, &headers, &body) {
            return Err(refusal);
        }
        Ok(FetchResponse { url, status, content_type, body, truncated, location })
    }

    pub fn curl(
        &self,
        resource_host: Option<&str>,
        request: &CurlRequest,
        remaining_ms: u64,
    ) -> Result<CurlResponse, Error> {
        let (limit, timeout) = self.admit(&request.url, resource_host, None, remaining_ms)?;
        let method = request.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
        let body = match method.as_str() {
            "GET" | "DELETE" | "HEAD" => None,
            "POST" | "PUT" | "PATCH" => Some(request.body.as_deref().unwrap_or("")),
            other => return Err(refuse(ErrorCode::InvalidInput, &format!("unsupported method {other}"))),
        };
        let (url, response) =
            self.send(resource_host, &method, &request.url, &request.headers, body, timeout, false)?;
        let status = response.status().as_u16();
        let header_map = response.headers().clone();
        let headers = header_map
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
            .collect();
        // A HEAD response carries no body; reading one would trip content decoding.
        let (body, truncated) =
            if method == "HEAD" { (String::new(), false) } else { read_body(response, limit)? };
        if let Some(refusal) = refusal(&url, status, &header_map, &body) {
            return Err(refusal);
        }
        Ok(CurlResponse { url, status, headers, body, truncated })
    }

    /// A GET whose body is kept as bytes, for a program that links this
    /// library to fetch with the same checks (the skills host fetches
    /// archives); the `web` interface itself returns text. Over https alone,
    /// every redirect too: what it fetches is installed, not only read.
    pub fn get_bytes(&self, url: &str, max_bytes: u64, remaining_ms: u64) -> Result<Fetched, Error> {
        let (limit, timeout) = self.admit(url, None, Some(max_bytes), remaining_ms)?;
        let (url, response) = self.send(None, "GET", url, &BTreeMap::new(), None, timeout, true)?;
        let status = response.status().as_u16();
        let content_type = header(&response, "content-type").unwrap_or_default();
        let headers = response.headers().clone();
        let (bytes, truncated) = read_bytes(response, limit)?;
        let page = String::from_utf8_lossy(&bytes[..bytes.len().min(CHALLENGE_PAGE_MAX_BYTES + 1)]);
        if let Some(refusal) = refusal(&url, status, &headers, &page) {
            return Err(refusal);
        }
        Ok(Fetched { url, status, content_type, bytes, truncated })
    }
}

/// What [`WebService::get_bytes`] fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The URL that answered, after redirects.
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Resolves a redirect's `Location` against the URL that returned it. Anything
/// with its own scheme is returned as is, for `host_of` to accept or refuse.
fn join_url(base: &str, location: &str) -> String {
    if location.split(['/', '?', '#']).next().is_some_and(|s| s.contains(':')) {
        return location.to_owned();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    if let Some(net) = location.strip_prefix("//") {
        return format!("{scheme}://{net}");
    }
    let (authority, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    format!("{scheme}://{authority}{dir}{location}")
}

fn header<B>(response: &ureq::http::Response<B>, name: &str) -> Option<String> {
    response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// The site's refusal of automated access, if this response is one: a human
/// check served in place of the page, or a 429 with the wait it names.
fn refusal(url: &str, status: u16, headers: &ureq::http::HeaderMap, body: &str) -> Option<Error> {
    let answered = match human_check(status, headers, body) {
        Some(check) => format!("with a human check ({check})"),
        None if status == 429 => "429 Too Many Requests".to_owned(),
        None => return None,
    };
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .filter(|wait| status == 429 && wait.len() <= MAX_RETRY_AFTER_CHARS);
    Some(refused(url, &answered, retry_after))
}

/// Which human check a response is, if it is one: Cloudflare's challenge
/// header, a challenge page's own marks, or a captcha on a 403 or 429. Only a
/// small HTML page is searched, so a long page or a source file that quotes
/// these marks is content.
fn human_check(status: u16, headers: &ureq::http::HeaderMap, body: &str) -> Option<&'static str> {
    let value =
        |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    if value("cf-mitigated") == "challenge" {
        return Some("a Cloudflare challenge");
    }
    if !value("content-type").contains("html") || body.len() > CHALLENGE_PAGE_MAX_BYTES {
        return None;
    }
    let page = body.to_ascii_lowercase();
    let found = |marks: &[(&str, &'static str)]| {
        marks.iter().find(|(mark, _)| page.contains(mark)).map(|&(_, check)| check)
    };
    found(CHALLENGE_PAGES).or_else(|| if matches!(status, 403 | 429) { found(CAPTCHAS) } else { None })
}

/// A redirect target that is a human gate rather than the page asked for: a
/// path segment named `gate` (LMFDB's `/gate.html`) or ending in `captcha`.
fn is_human_gate(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    rest.split('/').skip(1).any(|segment| {
        let stem = segment.split('.').next().unwrap_or("").to_ascii_lowercase();
        stem == "gate" || stem.ends_with("captcha")
    })
}

/// The error a site's refusal becomes, in words the caller's model acts on.
fn refused(url: &str, answered: &str, retry_after: Option<&str>) -> Error {
    let host = host_of(url).unwrap_or_else(|_| "the site".to_owned());
    let next = match retry_after {
        Some(wait) => format!(
            "It names a wait (Retry-After: {wait}): you may wait that long once and try again. If it \
             refuses again, stop and tell the person. Do not work around it."
        ),
        None => "Do not retry or work around it; tell the person.".to_owned(),
    };
    Error::new(
        ErrorCode::PermissionDenied,
        format!("{host} answered {answered}: the site does not allow automated access now. {next}"),
    )
}

fn transport(what: &str, e: &ureq::Error) -> Error {
    if let ureq::Error::Io(io) = e
        && io.kind() == io::ErrorKind::PermissionDenied
    {
        return refuse(ErrorCode::PermissionDenied, INTERNAL);
    }
    let detail: String = format!("{e}").chars().take(MAX_ERROR_CHARS).collect();
    Error::new(ErrorCode::ServiceError, format!("{what}: {detail}"))
}

fn read_body(response: ureq::http::Response<ureq::Body>, limit: u64) -> Result<(String, bool), Error> {
    let (bytes, truncated) = read_bytes(response, limit)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

/// At most `limit` bytes of the body, and whether there was more.
fn read_bytes(mut response: ureq::http::Response<ureq::Body>, limit: u64) -> Result<(Vec<u8>, bool), Error> {
    let mut bytes = response
        .body_mut()
        .with_config()
        .limit(limit + 1)
        .read_to_vec()
        .map_err(|e| transport("reading the response failed", &e))?;
    let truncated = bytes.len() as u64 > limit;
    bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok((bytes, truncated))
}

/// Encodes a reply, refusing one too large for a frame once JSON-escaped (a body
/// of control bytes grows sixfold), so the service answers instead of exiting.
fn encode_reply<T: Serialize>(value: &T) -> Result<Payload, Error> {
    let payload = Payload::encode(value)?;
    if payload.len() > MAX_FRAME_BYTES - REPLY_ENVELOPE_BYTES {
        return Err(refuse(
            ErrorCode::ServiceError,
            "the response is too large to return once encoded; request fewer bytes",
        ));
    }
    Ok(payload)
}

/// Provides the endpoint and serves requests until told to terminate.
pub fn run(options: &Options) -> Result<(), String> {
    let service = WebService::new(options);
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE)
        .ok_or("started without authority to provide an endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    loop {
        match kernel.next_event().map_err(|e| e.to_string())? {
            Event::Dispatch(d) => {
                let host = match d.resource.segments() {
                    [host] => Some(host.as_str()),
                    _ => None,
                };
                let result = serve(&service, d.action.as_str(), host, &d.payload, d.remaining_ms);
                kernel.reply(d.id, result).map_err(|e| e.to_string())?;
            }
            Event::CancelDispatch(_) | Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

fn serve(
    service: &WebService,
    action: &str,
    host: Option<&str>,
    payload: &Payload,
    remaining_ms: u64,
) -> Result<Payload, Error> {
    match action {
        ACTION_FETCH => encode_reply(&service.fetch(host, &payload.decode()?, remaining_ms)?),
        ACTION_CURL => encode_reply(&service.curl(host, &payload.decode()?, remaining_ms)?),
        _ => Err(refuse(ErrorCode::InvalidInput, "unknown action")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn service(extra: &[&str]) -> WebService {
        WebService::new(&Options::parse(extra.iter().map(|s| (*s).to_owned())).unwrap())
    }

    #[test]
    fn internal_addresses_are_refused_in_every_form_and_public_hosts_are_not() {
        use std::net::ToSocketAddrs;
        #[rustfmt::skip]
        let table = [
            ("localhost", true), ("a.localhost", true), ("printer.local", true), ("127.0.0.1", true),
            ("127.1", true), ("2130706433", true), ("0x7f000001", true), ("0.0.0.0", true),
            ("10.0.0.1", true), ("172.16.0.1", true), ("192.168.0.1", true), ("169.254.169.254", true),
            ("100.64.0.1", true), ("224.0.0.1", true), ("::1", true), ("::", true),
            ("::ffff:127.0.0.1", true), ("::ffff:a9fe:a9fe", true), ("fc00::1", true), ("fd12::1", true),
            ("fe80::1", true), ("ff02::1", true), ("8.8.8.8", false), ("100.128.0.1", false),
            ("172.32.0.1", false), ("2606:4700::1111", false), ("fdic.gov", false), ("fcc.gov", false),
        ];
        for (host, internal) in table {
            // Numeric shorthand is normalised by the system resolver, as `GuardedResolver` sees it.
            let resolved = host.starts_with(|c: char| c.is_ascii_digit())
                && (host, 80).to_socket_addrs().is_ok_and(|mut a| a.any(|a| is_internal_ip(a.ip())));
            assert_eq!(is_internal_host(host) || resolved, internal, "{host}");
        }
        let s = service(&[]);
        assert!(s.admit("https://example.com/x", None, None, 10_000).is_ok());
        assert_eq!(
            s.admit("http://[::ffff:127.0.0.1]/", None, None, 10_000).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn a_download_of_bytes_is_https_only() {
        // `get_bytes` installs what it fetches (skills): plain http is refused before any
        // request, as every redirect hop is.
        let s = service(&[]);
        let refused = s.get_bytes("http://example.com/skill.zip", 1024, 10_000).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidInput, "{}", refused.message);
        assert!(refused.message.contains("https only"), "{}", refused.message);
    }

    #[test]
    fn every_redirect_hop_is_admitted_again() {
        use std::io::{BufRead, BufReader, Write};
        // An unscoped call re-checks every hop and refuses internal targets; a
        // call scoped to a host returns a redirect to another host unfollowed.
        for (target, scope, returned) in [
            ("http://169.254.169.254/latest/meta-data/", None, None),
            ("http://0x7f000001/", None, None),
            ("http://example.com/", Some("127.0.0.1"), Some("http://example.com/")),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = served.clone();
            // A same-host relative redirect is followed; the next hop leaves the scope.
            std::thread::spawn(move || {
                for to in ["/hop", target] {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 2 {
                        line.clear();
                    }
                    let reply = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    // Counted before the reply: the client may finish as soon as it reads it.
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (&stream).write_all(reply.as_bytes()).unwrap();
                }
            });
            let s = WebService::with_exempt(&Options::parse([]).unwrap(), Some(addr));
            let request = FetchRequest { url: format!("http://{addr}/start") };
            match (s.fetch(scope, &request, 10_000), returned) {
                (Ok(reply), Some(location)) => {
                    assert_eq!((reply.status, reply.location.as_deref()), (302, Some(location)), "{target}");
                }
                (Err(err), None) => assert_eq!(err.code, ErrorCode::PermissionDenied, "{target}: {err:?}"),
                (other, _) => panic!("{target}: {other:?}"),
            }
            assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 2, "{target}");
        }
    }

    #[test]
    fn a_human_check_or_a_429_is_a_refusal_and_a_page_about_captchas_is_content() {
        use std::io::{BufRead, BufReader, Write};
        const HTML: &str = "Content-Type: text/html; charset=UTF-8\r\n";
        const RECAPTCHA: &str = "<!DOCTYPE html><html><head><title>www.lmfdb.org</title>\
            <script src=\"https://www.google.com/recaptcha/challengepage/challengepage.js\"></script></head>\
            <body><div id=\"challenge\"></div><script>RecaptchaChallengePageUi.render(\"challenge\");</script>\
            </body></html>";
        const JUST_A_MOMENT: &str = "<!DOCTYPE html><html lang=\"en-US\"><head><title>Just a moment...</title>\
            </head><body><noscript>Enable JavaScript and cookies to continue</noscript><script>\
            window._cf_chl_opt={cvId:'3',cType:'managed'};</script></body></html>";
        const ATTENTION: &str = "<!DOCTYPE html><html><head><title>Attention Required! | Cloudflare</title>\
            </head><body><h1>Sorry, you have been blocked</h1></body></html>";
        const HCAPTCHA: &str = "<html><head><title>Verify you are human</title>\
            <script src=\"https://js.hcaptcha.com/1/api.js\" async defer></script></head><body><form>\
            <div class=\"h-captcha\" data-sitekey=\"10000000-ffff-ffff-ffff-000000000001\"></div></form></body></html>";
        const ARTICLE: &str = "<html><head><title>CAPTCHA - Wikipedia</title></head><body><p>A CAPTCHA is a \
            challenge-response test. reCAPTCHA and hCaptcha are common; Cloudflare shows \"Just a moment...\" \
            while it checks a browser.</p></body></html>";
        const FORM: &str = "<html><head><title>Contact</title>\
            <script src=\"https://www.google.com/recaptcha/api.js\" async defer></script></head><body>\
            <form method=\"post\"><div class=\"g-recaptcha\" data-sitekey=\"6LeIxAcTAAAAAJcZ\"></div></form></body></html>";
        // Status line, headers, body, and what a refusal must say (None: the response is returned).
        #[rustfmt::skip]
        let table = [
            ("200 OK", HTML, RECAPTCHA, Some("human check (reCAPTCHA)")),
            ("403 Forbidden", "cf-mitigated: challenge\r\n", "", Some("human check (a Cloudflare challenge)")),
            ("503 Service Unavailable", HTML, JUST_A_MOMENT, Some("human check (a Cloudflare challenge)")),
            ("403 Forbidden", HTML, ATTENTION, Some("human check (a Cloudflare challenge)")),
            ("403 Forbidden", HTML, HCAPTCHA, Some("human check (hCaptcha)")),
            ("302 Found", "Location: /gate.html?next=%2Fapi%2F\r\n", "", Some("redirect to a human gate")),
            ("429 Too Many Requests", "Retry-After: 120\r\n", "slow down", Some("(Retry-After: 120)")),
            ("200 OK", HTML, ARTICLE, None),
            ("200 OK", HTML, FORM, None),
            ("403 Forbidden", HTML, "<html><title>403 Forbidden</title></html>", None),
        ];
        for (status, headers, body, refusal) in table {
            let reply = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            // One answer for `fetch` and one for `curl`; a gate followed would find no third.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 2 {
                        line.clear();
                    }
                    (&stream).write_all(reply.as_bytes()).unwrap();
                }
            });
            let s = WebService::with_exempt(&Options::parse([]).unwrap(), Some(addr));
            let url = format!("http://{addr}/api/");
            let fetched = s.fetch(Some("127.0.0.1"), &FetchRequest { url: url.clone() }, 10_000);
            let curled = s.curl(
                Some("127.0.0.1"),
                &CurlRequest { url, method: None, headers: BTreeMap::new(), body: None },
                10_000,
            );
            for got in [fetched.map(|r| (r.status, r.body)), curled.map(|r| (r.status, r.body))] {
                match (got, refusal) {
                    (Err(e), Some(says)) => {
                        assert_eq!(e.code, ErrorCode::PermissionDenied, "{status}: {e:?}");
                        assert!(e.message.starts_with("127.0.0.1 answered"), "{status}: {e:?}");
                        assert!(e.message.contains(says) && !e.message.contains('<'), "{status}: {e:?}");
                    }
                    (Ok((code, text)), None) => {
                        assert_eq!((code.to_string(), text.as_str()), (status[..3].to_owned(), body))
                    }
                    (other, _) => panic!("{status} {body}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_reply_too_large_to_frame_is_an_error() {
        let body = "\u{1}".repeat(DEFAULT_MAX_BYTES as usize);
        let reply = FetchResponse {
            url: String::new(),
            status: 200,
            content_type: String::new(),
            body,
            truncated: false,
            location: None,
        };
        assert_eq!(encode_reply(&reply).unwrap_err().code, ErrorCode::ServiceError);
    }

    #[test]
    fn a_scoped_capability_pins_the_host() {
        let open = service(&[]);
        assert_eq!(
            open.admit("https://other.com/", Some("example.com"), None, 10_000).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert!(open.admit("https://example.com/", Some("example.com"), None, 10_000).is_ok());
    }

    #[test]
    fn a_too_short_deadline_and_a_bad_url_are_rejected() {
        let s = service(&[]);
        assert_eq!(
            s.admit("https://example.com/", None, None, 100).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            s.admit("ftp://example.com/", None, None, 10_000).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }

    // Hits the real network; not run by default. `cargo test -p nucleus-web -- --ignored`.
    #[test]
    #[ignore = "requires outbound network"]
    fn fetch_and_curl_reach_a_real_page() {
        let s = service(&[]);
        let got = s
            .fetch(Some("example.com"), &FetchRequest { url: "https://example.com/".into() }, 15_000)
            .unwrap();
        assert_eq!(got.status, 200);
        assert!(got.body.to_lowercase().contains("example domain"), "unexpected body");

        let curl = s
            .curl(
                Some("example.com"),
                &CurlRequest {
                    url: "https://example.com/".into(),
                    method: Some("HEAD".into()),
                    headers: std::collections::BTreeMap::new(),
                    body: None,
                },
                15_000,
            )
            .unwrap();
        assert_eq!(curl.status, 200);
        assert!(curl.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")));
    }
}
