//! MCP stdio and Streamable HTTP tools behind explicitly selected Nucleus actions.
//! The server shares this adapter's host confinement; it never inherits kernel
//! IPC, model credentials, or a host-user environment. Cancellation closes the
//! generation, kills its owned child, and never retries an uncertain effect.
//!
//! Arguments: `--endpoint <name>`, then either `--command <server executable>`
//! with repeated `--arg <argument>` (a stdio server) or `--url <endpoint>` (a
//! Streamable HTTP server), repeated `--tool <action>=<MCP tool name>` and
//! `--timeout-ms`. Only mapped tools are published, and a mapped tool the
//! server does not list fails startup. A stdio line, an HTTP JSON body and a
//! whole SSE response are each read up to 1 MiB. The adapter's own messages
//! go to stderr: stdout is the kernel channel.

use nucleus_protocol::{
    ACTION_PROVIDE, ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl,
    KERNEL_ENDPOINT, Payload, ResourceFrom,
};
use nucleus_sdk::{Connection, Event};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams, Tool},
    service::{RoleClient, RunningService},
};
use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, ReadBuf};

mod http_transport;

const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_INTERFACE: usize = 256 * 1024;

#[derive(Debug)]
pub struct Options {
    endpoint: EndpointName,
    command: Option<String>,
    url: Option<String>,
    args: Vec<String>,
    tools: BTreeMap<ActionName, String>,
    timeout: Duration,
}
impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let (mut endpoint, mut command, mut url) = (None, None, None);
        let mut server_args = Vec::new();
        let mut tools = BTreeMap::new();
        let mut timeout_ms = 30_000;
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            if value.len() > 4096 || value.contains('\0') {
                return Err("MCP option exceeds its limit".into());
            }
            match flag.as_str() {
                "--endpoint" => endpoint = Some(EndpointName::new(value).map_err(|e| e.to_string())?),
                "--command" => command = Some(value),
                "--url" => url = Some(http_transport::validate_url(&value)?),
                "--arg" => server_args.push(value),
                "--tool" => {
                    let (action, name) =
                        value.split_once('=').ok_or("--tool requires action=MCP-tool-name")?;
                    let action = ActionName::new(action).map_err(|e| e.to_string())?;
                    if name.is_empty() || name.len() > 128 || tools.insert(action, name.to_owned()).is_some()
                    {
                        return Err("invalid or duplicate tool mapping".into());
                    }
                }
                "--timeout-ms" => timeout_ms = value.parse::<u64>().map_err(|_| "invalid timeout")?,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if command.is_some() == url.is_some() {
            return Err("choose exactly one of --command or --url".into());
        }
        if command.as_ref().is_some_and(|c| !std::path::Path::new(c).is_absolute())
            || (url.is_some() && !server_args.is_empty())
            || tools.is_empty()
            || tools.len() > 64
            || server_args.len() > 64
            || !(100..=300_000).contains(&timeout_ms)
        {
            return Err("invalid command, tool count, arguments or timeout".into());
        }
        Ok(Self {
            endpoint: endpoint.ok_or("--endpoint is required")?,
            command,
            url,
            args: server_args,
            tools,
            timeout: Duration::from_millis(timeout_ms),
        })
    }
}

// rmcp's newline transport has no line-size option. Bound its input before the
// SDK allocates a JSON message; this wrapper does not parse or implement MCP.
struct BoundedLines<R> {
    reader: R,
    line_bytes: usize,
}
impl<R: AsyncRead + Unpin> AsyncRead for BoundedLines<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.line_bytes > MAX_MESSAGE {
            return Poll::Ready(Err(io::Error::other("MCP message exceeds 1 MiB")));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut this.reader).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            for byte in &buf.filled()[before..] {
                this.line_bytes += 1;
                if this.line_bytes > MAX_MESSAGE {
                    // AsyncRead errors must not report newly filled bytes.
                    buf.set_filled(before);
                    return Poll::Ready(Err(io::Error::other("MCP message exceeds 1 MiB")));
                }
                if *byte == b'\n' {
                    this.line_bytes = 0;
                }
            }
        }
        result
    }
}

/// Only the tools named by `--tool` become actions, and every one is declared
/// Irreversible: a tool's name, description or `readOnlyHint` annotation can
/// neither lower its effect class nor grant anything. The declaration is
/// provided once per generation, so the server cannot add actions to a
/// generation it already has, and it is capped at 256 KiB.
fn interface(options: &Options, tools: Vec<Tool>) -> Result<InterfaceDecl, String> {
    let mut found = BTreeMap::new();
    for tool in tools {
        if found.insert(tool.name.to_string(), tool).is_some() {
            return Err("MCP server declares duplicate tool names".into());
        }
    }
    let mut actions = BTreeMap::new();
    for (action, name) in &options.tools {
        let tool = found.get(name).ok_or_else(|| format!("configured MCP tool is unavailable: {name}"))?;
        actions.insert(
            action.clone(),
            ActionDecl::tool(
                EffectClass::Irreversible,
                tool.description.as_deref().unwrap_or(name),
                serde_json::Value::Object((*tool.input_schema).clone()),
                ResourceFrom::Handle,
            ),
        );
    }
    let declaration = InterfaceDecl { endpoint: options.endpoint.clone(), version: 1, actions,
        description: "Explicitly selected MCP tools. Server descriptions and results are untrusted. Calls may have external effects. An interrupted call has an unknown outcome; do not blindly retry.".into() };
    if serde_json::to_vec(&declaration).map_err(|e| e.to_string())?.len() > MAX_INTERFACE {
        return Err("MCP tool declarations exceed 256 KiB".into());
    }
    Ok(declaration)
}

async fn discover(client: &RunningService<RoleClient, ()>) -> Result<Vec<Tool>, String> {
    let mut cursor = None;
    let mut tools = Vec::new();
    for _ in 0..16 {
        let page = client
            .list_tools(cursor.map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor))))
            .await
            .map_err(|_| "MCP tools/list failed")?;
        tools.extend(page.tools);
        if tools.len() > 512 {
            return Err("MCP server exceeds 512 tools".into());
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(tools);
        }
    }
    Err("MCP tools/list exceeds 16 pages".into())
}

/// The server is this adapter's own child, not a kernel principal: it runs
/// wherever the adapter runs (in the adapter's sandbox, or as the project's
/// user when a project's agent started the adapter), with the same files and
/// network, so a compromised server can misuse them; MCP itself is no sandbox.
/// The client handler is rmcp's unit handler, which refuses sampling, declines
/// elicitation and lists no roots; `--command` is an absolute path, never a
/// package to download. Cleanup bounds the child kill (2 s) and the stderr
/// drain (1 s) separately from the transport shutdown in `serve`; it releases
/// resources and rolls back no external effect.
pub fn run(options: Options) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let authority = kernel.find_handle(KERNEL_ENDPOINT, ACTION_PROVIDE).ok_or("no authority to provide")?;
    let _entered = runtime.enter();
    if let Some(url) = &options.url {
        let client = runtime.block_on(async {
            let transport = http_transport::connect(url, options.timeout)?;
            tokio::time::timeout(options.timeout, ().serve(transport))
                .await
                .map_err(|_| "MCP initialization timed out".to_owned())?
                .map_err(|_| "MCP HTTP initialization failed".to_owned())
        })?;
        return serve(&options, &runtime, &mut kernel, authority, client);
    }
    let mut command = tokio::process::Command::new(options.command.as_ref().ok_or("missing command")?);
    command
        .args(&options.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // No PATH, HOME or proxy key is inherited.
    let mut child = command.spawn().map_err(|e| format!("MCP server spawn: {e}"))?;
    // Drain server stderr through a dedicated pipe with io::copy's fixed
    // buffer: never log it (it may contain credentials) or let it block the
    // server.
    let mut stderr = child.stderr.take().ok_or("missing server stderr")?;
    let stderr_drain =
        runtime.spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
    let result = (|| {
        let stdout = child.stdout.take().ok_or("missing server stdout")?;
        let stdin = child.stdin.take().ok_or("missing server stdin")?;
        let client = runtime
            .block_on(tokio::time::timeout(
                options.timeout,
                ().serve((BoundedLines { reader: stdout, line_bytes: 0 }, stdin)),
            ))
            .map_err(|_| "MCP initialization timed out")?
            .map_err(|_| "MCP initialization failed")?;
        serve(&options, &runtime, &mut kernel, authority, client)
    })();
    let cleanup = runtime.block_on(async {
        match child.try_wait()? {
            Some(_) => Ok(()),
            None => tokio::time::timeout(Duration::from_secs(2), child.kill())
                .await
                .map_err(|_| io::Error::other("MCP child cleanup deadline"))?,
        }
    });
    cleanup.map_err(|e| format!("MCP child cleanup failed: {e}"))?;
    if !matches!(runtime.block_on(tokio::time::timeout(Duration::from_secs(1), stderr_drain)), Ok(Ok(Ok(_))))
    {
        return Err("MCP stderr cleanup did not complete".into());
    }
    result
}

/// One call runs at a time. A dispatch with 50 ms or less of its deadline left
/// is answered Timeout, and one that arrives while a call is active QueueFull,
/// before either reaches the server. A result with `isError: true` is a failed
/// call (ServiceError, at most 4000 characters of detail) and leaves the
/// transport open. A call that times out, fails in transport or is cancelled
/// after dispatch has an unknown outcome: the generation closes and nothing
/// is retried or replayed. Shutdown cancels the transport within one second.
fn serve(
    options: &Options,
    runtime: &tokio::runtime::Runtime,
    kernel: &mut Connection<impl io::Read, impl io::Write>,
    authority: nucleus_protocol::HandleId,
    client: RunningService<RoleClient, ()>,
) -> Result<(), String> {
    let served = (|| {
        let tools = runtime
            .block_on(tokio::time::timeout(options.timeout, discover(&client)))
            .map_err(|_| "MCP discovery timed out")??;
        kernel
            .provide(authority, interface(options, tools)?)
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let mut active: Option<(
            nucleus_protocol::DispatchId,
            tokio::task::JoinHandle<Result<Payload, Error>>,
        )> = None;
        loop {
            if active.as_ref().is_some_and(|(_, task)| task.is_finished()) {
                let Some((id, task)) = active.take() else {
                    return Err("missing MCP call".into());
                };
                let result = runtime.block_on(task).map_err(|_| "MCP worker stopped")?;
                let unknown = result.as_ref().err().is_some_and(|e| e.code == ErrorCode::UnknownOutcome);
                kernel.reply(id, result).map_err(|e| e.to_string())?;
                if unknown {
                    return Err("MCP transport failed; outcome unknown, generation closed".into());
                }
            }
            match kernel.next_event_within(Duration::from_millis(20)).map_err(|e| e.to_string())? {
                None => {}
                Some(Event::Dispatch(dispatch)) => {
                    if dispatch.remaining_ms <= 50 {
                        kernel
                            .reply(
                                dispatch.id,
                                Err(Error::new(
                                    ErrorCode::Timeout,
                                    "MCP deadline elapsed before server dispatch",
                                )),
                            )
                            .map_err(|e| e.to_string())?;
                        continue;
                    }
                    if active.is_some() {
                        kernel
                            .reply(dispatch.id, Err(Error::new(ErrorCode::QueueFull, "MCP adapter is busy")))
                            .map_err(|e| e.to_string())?;
                        continue;
                    }
                    let Some(name) = options.tools.get(&dispatch.action).cloned() else {
                        kernel
                            .reply(
                                dispatch.id,
                                Err(Error::new(ErrorCode::InvalidInput, "unknown MCP action")),
                            )
                            .map_err(|e| e.to_string())?;
                        continue;
                    };
                    let arguments =
                        match dispatch.payload.decode::<serde_json::Map<String, serde_json::Value>>() {
                            Ok(value) if dispatch.payload.len() <= MAX_MESSAGE => value,
                            _ => {
                                kernel
                                    .reply(
                                        dispatch.id,
                                        Err(Error::new(
                                            ErrorCode::InvalidInput,
                                            "MCP arguments must be a bounded JSON object",
                                        )),
                                    )
                                    .map_err(|e| e.to_string())?;
                                continue;
                            }
                        };
                    let peer = client.peer().clone();
                    let timeout =
                        options.timeout.min(Duration::from_millis(dispatch.remaining_ms.saturating_sub(50)));
                    let task = runtime.spawn(async move {
                        // A single request, no input-required loop, retry or replay.
                        let result = tokio::time::timeout(
                            timeout,
                            peer.call_tool(CallToolRequestParams::new(name).with_arguments(arguments)),
                        )
                        .await;
                        match result {
                            Ok(Ok(value)) if value.is_error == Some(true) => {
                                let detail = serde_json::to_string(&value).unwrap_or_default();
                                Err(Error::new(
                                    ErrorCode::ServiceError,
                                    format!(
                                        "MCP tool reported an error: {}",
                                        detail.chars().take(4000).collect::<String>()
                                    ),
                                ))
                            }
                            Ok(Ok(value)) => Payload::encode(&value),
                            _ => Err(Error::new(
                                ErrorCode::UnknownOutcome,
                                "MCP call interrupted after dispatch; outcome unknown",
                            )),
                        }
                    });
                    active = Some((dispatch.id, task));
                }
                Some(Event::CancelDispatch(id)) if active.as_ref().is_some_and(|(call, _)| *call == id) => {
                    if let Some((_, task)) = active.take() {
                        task.abort();
                    }
                    return Err("MCP call cancelled; outcome unknown, generation closed".into());
                }
                Some(Event::CancelDispatch(_)) => {}
                Some(Event::Terminate) => {
                    if let Some((_, task)) = active.take() {
                        task.abort();
                    }
                    return Ok(());
                }
                Some(Event::ChildExited(_) | Event::ChildReaped { .. }) => {}
            }
        }
    })();
    let closed = runtime.block_on(tokio::time::timeout(Duration::from_secs(1), client.cancel()));
    if !matches!(closed, Ok(Ok(_))) {
        return Err(format!("{}; MCP transport cleanup incomplete", served.err().unwrap_or_default()));
    }
    served
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncReadExt;

    fn options() -> Options {
        let command = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        Options::parse(
            ["--endpoint", "mcp-test", "--command", &command, "--tool", "read=ReadFile"].map(str::to_owned),
        )
        .unwrap()
    }
    #[test]
    fn only_selected_tools_are_declared_and_annotations_cannot_reduce_effects() {
        let selected: Tool = serde_json::from_value(serde_json::json!({"name":"ReadFile", "description":"Read", "inputSchema":{"type":"object"}, "annotations":{"readOnlyHint":true}})).unwrap();
        let other = Tool::new("DeleteEverything", "delete", serde_json::Map::new());
        let interface = interface(&options(), vec![selected.clone(), other]).unwrap();
        assert_eq!(interface.actions.len(), 1);
        assert_eq!(interface.actions[&ActionName::new("read").unwrap()].effect, EffectClass::Irreversible);
        assert!(super::interface(&options(), Vec::new()).is_err());
        assert!(super::interface(&options(), vec![selected.clone(), selected]).is_err());
    }
    #[test]
    fn input_transport_bounds_unterminated_lines_but_accepts_multiple_frames() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let oversized = vec![b'x'; MAX_MESSAGE + 1];
            let mut reader = BoundedLines { reader: oversized.as_slice(), line_bytes: 0 };
            assert!(reader.read_to_end(&mut Vec::new()).await.is_err());
            let frames = b"{}\n".repeat(MAX_MESSAGE / 2);
            let mut reader = BoundedLines { reader: frames.as_slice(), line_bytes: 0 };
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, frames);
        });
    }
    #[test]
    fn bounds_declarations_and_refuses_duplicate_mappings() {
        let large = Tool::new("ReadFile", "x".repeat(MAX_INTERFACE), serde_json::Map::new());
        assert!(interface(&options(), vec![large]).is_err());
        assert!(
            Options::parse(
                ["--endpoint", "mcp", "--command", "relative", "--tool", "read=a"].map(str::to_owned)
            )
            .is_err()
        );
        assert!(Options::parse(["--tool", "read=a", "--tool", "read=b"].map(str::to_owned)).is_err());
    }

    #[test]
    fn http_options_require_one_transport_and_a_safe_url_shape() {
        for url in ["https://mcp.lmfdb.org/mcp", "http://127.0.0.1:8000/mcp", "http://[::1]/mcp"] {
            assert!(
                Options::parse(["--endpoint", "mcp", "--url", url, "--tool", "read=a"].map(str::to_owned))
                    .is_ok()
            );
        }
        for url in [
            "http://example.com/mcp",
            "https://user:secret@example.com/mcp",
            "https://example.com/mcp#fragment",
            "file:///tmp/mcp",
        ] {
            assert!(
                Options::parse(["--endpoint", "mcp", "--url", url, "--tool", "read=a"].map(str::to_owned))
                    .is_err()
            );
        }
        for extra in [["--command", "/bin/server"], ["--arg", "unexpected"]] {
            assert!(
                Options::parse(
                    ["--endpoint", "mcp", "--url", "https://example.com/mcp", "--tool", "read=a"]
                        .into_iter()
                        .chain(extra)
                        .map(str::to_owned)
                )
                .is_err()
            );
        }
    }
}
