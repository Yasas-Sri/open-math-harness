//! The `web` service interface, version 1: fetch a web page, or make an
//! arbitrary HTTP request.
//!
//! | Action | Payload | Result | Effect |
//! | --- | --- | --- | --- |
//! | `fetch` | [`FetchRequest`] | [`FetchResponse`] | observation (a GET) |
//! | `curl` | [`CurlRequest`] | [`CurlResponse`] | irreversible (any method) |
//!
//! `fetch` is a GET that follows redirects and returns the page as text.
//! `curl` is the general form: a method, headers and an optional body, with the
//! raw status, headers and body returned. `curl` is classified irreversible
//! because a `POST`/`PUT`/`DELETE` can change a remote system with no inverse,
//! so an interrupted `curl` is reported as `unknown_outcome`.
//!
//! A capability may be scoped to a host by its resource path (`["example.com"]`),
//! or left at the root (`[]`) to reach any host. The kernel checks that before
//! the provider is reached.
//!
//! A site that refuses automated access is not answering: a human check served
//! in place of the page, a redirect to one, or a 429 is a `permission_denied`
//! error naming the site (with a 429's `Retry-After`), never a result. The
//! provider documents what it recognises as one.
//!
//! Everything a server returns is untrusted input; it carries no authority.
//! This crate defines data only.

use nucleus_protocol::{
    ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl, ResourceFrom,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "web";
pub const ACTION_FETCH: &str = "fetch";
pub const ACTION_CURL: &str = "curl";
pub const VERSION: u32 = 1;

/// Payload of `fetch`: a URL to GET.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub url: String,
}

/// Result of `fetch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub truncated: bool,
    /// Where a redirect to another host points. Such a redirect is returned,
    /// not followed: the call was authorised for its URL's host only, so the
    /// caller fetches this URL itself and its own capability decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Payload of `curl`: an arbitrary request. `method` defaults to GET.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurlRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    /// Request headers as name/value pairs.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// Result of `curl`: the raw response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurlResponse {
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub truncated: bool,
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    // The URL stays in the payload (the provider fetches it); its host also
    // forms the resource, so a capability can be scoped to a host.
    let from_url = ResourceFrom::Host { param: "url".to_owned() };
    let url = || json!({"type": "string", "description": "Absolute http or https URL"});
    Ok(InterfaceDecl {
        endpoint,
        version: VERSION,
        description: String::new(),
        actions: BTreeMap::from([
            (
                ActionName::new(ACTION_FETCH)?,
                ActionDecl::tool(
                    EffectClass::Observation,
                    "Fetch a web page over HTTP(S) and return its text. Follows redirects within the URL's host; a redirect to another host is returned with its `location` for you to fetch. A human check (a CAPTCHA, a bot challenge or a redirect to one) or a 429 is the site refusing automated access: the call fails with `permission_denied` instead of returning the page. Stop using the site (a 429's `Retry-After` may be waited out once) and tell the person; never work around it.",
                    json!({"type": "object", "properties": {"url": url()}, "required": ["url"]}),
                    from_url.clone(),
                ),
            ),
            (
                ActionName::new(ACTION_CURL)?,
                ActionDecl::tool(
                    EffectClass::Irreversible,
                    "Make an HTTP request (any method, headers, body) and return the raw response. Follows redirects within the URL's host; a redirect to another host is returned as is (see its Location header). A non-GET request may change a remote system. A human check (a CAPTCHA, a bot challenge or a redirect to one) or a 429 is the site refusing automated access: the call fails with `permission_denied` instead of returning the response. Stop using the site (a 429's `Retry-After` may be waited out once) and tell the person; never work around it.",
                    json!({"type": "object", "properties": {
                        "url": url(),
                        "method": {"type": "string", "description": "GET, POST, PUT, DELETE, HEAD or PATCH; defaults to GET"},
                        "headers": {"type": "object", "description": "Request headers as name/value pairs"},
                        "body": {"type": "string", "description": "Request body, for POST/PUT/PATCH"}
                    }, "required": ["url"]}),
                    from_url,
                ),
            ),
        ]),
    })
}

/// The host of an `http`/`https` URL, lower-cased and without any port. Used to
/// scope a capability to a host and to keep a fetch off the local machine.
pub fn host_of(url: &str) -> Result<String, Error> {
    let invalid = |why: &str| Error::new(ErrorCode::InvalidInput, format!("url: {why}"));
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| invalid("must be an http or https URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid("userinfo in the authority is not allowed"));
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // IPv6 literal "[addr]:port"
    } else {
        authority.split(':').next().unwrap_or("") // host or host:port
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return Err(invalid("no host"));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn host_is_extracted_lowercased_without_port_or_scheme() {
        assert_eq!(host_of("https://Example.com/path?q=1").unwrap(), "example.com");
        assert_eq!(host_of("http://example.com:8080/").unwrap(), "example.com");
        assert_eq!(host_of("https://[2001:db8::1]:443/x").unwrap(), "2001:db8::1");
    }

    #[test]
    fn non_http_schemes_and_userinfo_are_rejected() {
        for bad in
            ["ftp://example.com", "file:///etc/passwd", "example.com", "https://u:p@example.com", "https://"]
        {
            assert_eq!(host_of(bad).map_err(|e| e.code), Err(ErrorCode::InvalidInput), "{bad}");
        }
    }
}
