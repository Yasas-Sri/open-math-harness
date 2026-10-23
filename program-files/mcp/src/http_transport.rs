//! Bounded HTTP bodies, no redirects, ambient proxies, retries or session recovery.
//! The configured endpoint uses the adapter's existing network authority.
//! No OAuth flow runs and no credential is forwarded: the transport's auth
//! token is ignored, and a header handed in named `authorization` or `cookie`
//! is refused.
use std::{collections::HashMap, io, sync::Arc, time::Duration};

use futures::{StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue};
use reqwest::{Client, Method, Response, Url};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::{
        StreamableHttpClientTransport,
        common::client_side_sse::NeverRetry,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use sse_stream::{Sse, SseStream};

use crate::MAX_MESSAGE;
type Failure = StreamableHttpError<io::Error>;

fn fail(message: &str) -> Failure {
    StreamableHttpError::Client(io::Error::other(message.to_owned()))
}

/// HTTPS, or HTTP to a loopback IP, with no userinfo or fragment. Unlike the
/// web service, any host is admitted, private addresses included: the agent
/// names the endpoint in `mcp_connect`, and the adapter reaches it with its
/// own network authority.
pub(crate) fn validate_url(value: &str) -> Result<String, String> {
    let url = Url::parse(value).map_err(|_| "invalid MCP URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("MCP URL requires HTTPS (HTTP only for a loopback IP), no userinfo or fragment".into());
    }
    Ok(url.into())
}

#[derive(Clone)]
pub(crate) struct BoundedClient(Client);

pub(crate) fn connect(
    url: &str,
    timeout: Duration,
) -> Result<StreamableHttpClientTransport<BoundedClient>, String> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .build()
        .map_err(|_| "cannot build MCP HTTP client")?;
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
    config.retry_config = Arc::new(NeverRetry::default());
    config.reinit_on_expired_session = false;
    config.allow_stateless = true;
    config.max_concurrent_requests = 1;
    config.channel_buffer_capacity = 8;
    config.max_sse_event_size = MAX_MESSAGE;
    config.control_request_timeout = Duration::from_secs(1);
    Ok(StreamableHttpClientTransport::with_client(BoundedClient(client), config))
}

impl BoundedClient {
    async fn request(
        &self,
        method: Method,
        uri: &str,
        session: Option<&str>,
        headers: HashMap<HeaderName, HeaderValue>,
        message: Option<&ClientJsonRpcMessage>,
    ) -> Result<Response, Failure> {
        let mut request = self.0.request(method, uri).header("accept", "application/json, text/event-stream");
        for (name, value) in headers {
            // Transport headers are supplied below; no header handed in may replace them.
            if matches!(
                name.as_str(),
                "host"
                    | "authorization"
                    | "cookie"
                    | "content-length"
                    | "content-type"
                    | "mcp-session-id"
                    | "accept"
            ) {
                return Err(fail("reserved MCP HTTP header"));
            }
            request = request.header(name, value);
        }
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        if let Some(message) = message {
            request = request.json(message);
        }
        let response = request.send().await.map_err(|_| fail("MCP HTTP request failed"))?;
        if response.status().is_redirection() {
            return Err(fail("MCP HTTP redirects are refused; configure the final endpoint"));
        }
        Ok(response)
    }
}

// Cap the entire response before JSON/SSE parsing, including chunked bodies and errors.
// A long-lived server notification stream also has this budget and is never reconnected.
fn events(response: Response) -> BoxStream<'static, Result<Sse, sse_stream::Error>> {
    let bytes = response.bytes_stream().scan(0usize, |total, chunk| {
        let result = chunk.map_err(|_| io::Error::other("MCP HTTP body interrupted")).and_then(|chunk| {
            *total = total.saturating_add(chunk.len());
            if *total > MAX_MESSAGE {
                Err(io::Error::other("MCP HTTP body exceeds 1 MiB"))
            } else {
                Ok(chunk)
            }
        });
        std::future::ready(Some(result))
    });
    SseStream::from_bytes_stream(bytes).boxed()
}

impl StreamableHttpClient for BoundedClient {
    type Error = io::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Failure> {
        let mut response =
            self.request(Method::POST, &uri, session_id.as_deref(), headers, Some(&message)).await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !status.is_success() {
            // Do not buffer or echo error bodies, URLs, or provider credentials.
            return Err(fail(&format!("MCP HTTP status {status}")));
        }
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| fail("invalid MCP session header"))?;
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
        let mime = content_type.split(';').next().unwrap_or("").trim();
        if mime == "text/event-stream" {
            return Ok(StreamableHttpPostResponse::Sse(events(response), session));
        }
        if mime != "application/json" {
            return Err(fail("MCP HTTP response must be JSON or SSE"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| fail("MCP HTTP body interrupted"))? {
            if body.len().saturating_add(chunk.len()) > MAX_MESSAGE {
                return Err(fail("MCP HTTP body exceeds 1 MiB"));
            }
            body.extend_from_slice(&chunk);
        }
        let parsed = serde_json::from_slice(&body).map_err(|_| fail("invalid MCP JSON response"))?;
        Ok(StreamableHttpPostResponse::Json(parsed, session))
    }

    /// Ends the server's session; a 404 or 405 counts as done.
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Failure> {
        let response = self.request(Method::DELETE, &uri, Some(&session_id), headers, None).await?;
        if response.status().is_success() || matches!(response.status().as_u16(), 404 | 405) {
            Ok(())
        } else {
            Err(fail("MCP HTTP session cleanup failed"))
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, sse_stream::Error>>, Failure> {
        let response = self.request(Method::GET, &uri, session_id.as_deref(), headers, None).await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success()
            || !response
                .headers()
                .get("content-type")
                .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"))
        {
            return Err(fail("MCP HTTP event stream failed"));
        }
        Ok(events(response))
    }
}
