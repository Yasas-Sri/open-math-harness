//! Provider of the `publish` interface (`nucleus-publish-interface`): the
//! secret scan of every session's records, which the platform's mirror reads
//! beside the records. The platform keeps one mirror of every session and
//! decides what is public; what it learns here is where a record holds
//! something that looks like a secret.
//!
//! ## Authority
//!
//! It holds `kernel provide` for its endpoint and `conversation history` on
//! every session, not delegable: one action that reads. No network, no model, no shell, no files
//! of any project, no state of its own, and no action that writes anything.
//! It runs as the runtime's own user in its sandbox.
//!
//! A compromised instance can read every session;
//! it cannot change any of them, and has no network to send them anywhere.
//! What it can do is answer the platform wrong findings: say nothing of a
//! secret, which the platform then serves to the public of a public project.
//! So it is trusted for the scan. Only the UI service holds its action, for
//! the platform's mirror connection.
//!
//! ## One page at a time
//!
//! One kernel connection and one thread: a `findings` call reads one page of
//! the session's log from the conversation service and scans it, within its
//! caller's deadline. Calls that arrive meanwhile wait their turn. A lost
//! kernel channel ends the process: fail and restart clean.

mod rules;
mod scan;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use nucleus_conversation_interface::{self as conversation, HistoryRequest, HistoryResponse};
use nucleus_projects_interface::valid_name;
use nucleus_protocol::{
    ACTION_PROVIDE, EndpointName, Error, ErrorCode, HandleId, KERNEL_ENDPOINT, Payload, ResourcePath,
};
use nucleus_publish_interface::{
    self as publish, Findings, FindingsRequest, MAX_PAGE_FINDINGS, RULES_VERSION, RecordFindings,
};
use nucleus_sdk::{ChannelError, Connection, Dispatch, Event};

pub use scan::Scanner;

/// Kept back from the caller's deadline to answer in time.
const REPLY_MARGIN: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub endpoint: EndpointName,
}

impl Options {
    /// Parses `[--endpoint <name>]`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut endpoint = publish::ENDPOINT.to_owned();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--endpoint" => endpoint = value,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self { endpoint: EndpointName::new(endpoint).map_err(|e| format!("--endpoint: {}", e.message))? })
    }
}

/// Where a page of a session's log comes from: the conversation service,
/// through the kernel.
pub trait Sources {
    /// One page of a session's log (`conversation history`).
    fn history(&mut self, resource: &ResourcePath, after: Option<u64>) -> Result<HistoryResponse, Error>;
}

/// The session a call names: its resource, `[project, session]`.
fn session_of(resource: &ResourcePath) -> Result<(), Error> {
    match resource.segments() {
        [project, session] if valid_name(project) && ResourcePath::new([session]).is_ok() => Ok(()),
        _ => Err(Error::new(ErrorCode::InvalidInput, "the resource names one session: [project, session]")),
    }
}

/// Serves one call.
pub fn handle(
    sources: &mut dyn Sources,
    action: &str,
    resource: &ResourcePath,
    payload: &Payload,
) -> Result<Payload, Error> {
    session_of(resource)?;
    match action {
        publish::ACTION_FINDINGS => {
            let request: FindingsRequest = payload.decode()?;
            request.validate()?;
            Payload::encode(&findings(sources, resource, &request)?)
        }
        _ => Err(Error::new(ErrorCode::InvalidInput, "unknown action")),
    }
}

/// One page of a session's findings (interface module doc).
pub fn findings(
    sources: &mut dyn Sources,
    resource: &ResourcePath,
    request: &FindingsRequest,
) -> Result<Findings, Error> {
    let page = sources.history(resource, request.after)?;
    let mut next = page.continuation(request.after)?;
    let scanner = Scanner { salt: request.salt.clone() };
    let (mut records, mut through, mut listed) = (Vec::new(), None, 0usize);
    for record in &page.records {
        let seq = record["seq"].as_u64().ok_or_else(|| {
            Error::new(ErrorCode::ServiceError, "the conversation service answered a record without its seq")
        })?;
        let (found, more) = scanner.record(record);
        // A page ends at the last record whose findings it holds whole.
        if listed + found.len() > MAX_PAGE_FINDINGS && through.is_some() {
            next = through;
            break;
        }
        listed += found.len();
        if !found.is_empty() {
            records.push(RecordFindings { seq, findings: found, more });
        }
        through = Some(seq);
    }
    Ok(Findings { records, through, next, rules: RULES_VERSION })
}

/// The kernel as one call of this service reads through it.
struct Reader<'a, R: Read, W: Write> {
    kernel: &'a mut Connection<R, W>,
    history: HandleId,
    deadline: Instant,
    /// The channel failed during a call: the process ends.
    lost: Option<ChannelError>,
}

impl<R: Read, W: Write> Sources for Reader<'_, R, W> {
    fn history(&mut self, resource: &ResourcePath, after: Option<u64>) -> Result<HistoryResponse, Error> {
        let left = self.deadline.saturating_duration_since(Instant::now()).saturating_sub(REPLY_MARGIN);
        let left = u64::try_from(left.as_millis()).unwrap_or(u64::MAX).max(1);
        let request = HistoryRequest { after_seq: after };
        match self.kernel.call(self.history, conversation::ACTION_HISTORY, resource, &request, Some(left)) {
            Ok(reply) => reply?.decode(),
            Err(lost) => {
                self.lost = Some(lost);
                Err(Error::new(ErrorCode::Unavailable, "the kernel channel is gone"))
            }
        }
    }
}

/// Provides the endpoint and serves it until the kernel terminates this
/// process.
pub fn run(options: &Options) -> Result<(), String> {
    let mut kernel = Connection::from_stdio().map_err(|e| e.to_string())?;
    let held = |endpoint: &str, action: &str| {
        kernel.find_handle(endpoint, action).ok_or_else(|| format!("started without {endpoint} {action}"))
    };
    let history = held(conversation::ENDPOINT, conversation::ACTION_HISTORY)?;
    let authority = held(KERNEL_ENDPOINT, ACTION_PROVIDE)?;
    let interface = publish::interface(options.endpoint.clone()).map_err(|e| e.message)?;
    kernel.provide(authority, interface).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    loop {
        let dispatch = match kernel.next_event() {
            Ok(Event::Dispatch(dispatch)) => dispatch,
            Ok(Event::Terminate) | Err(ChannelError::Terminated) => return Ok(()),
            Ok(_) => continue,
            Err(e) => return Err(e.to_string()),
        };
        match answer(&mut kernel, history, &dispatch) {
            Ok(()) => {}
            Err(ChannelError::Terminated) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Serves one call and answers it. A channel that failed under it is the
/// error: the process ends.
fn answer<R: Read, W: Write>(
    kernel: &mut Connection<R, W>,
    history: HandleId,
    dispatch: &Dispatch,
) -> Result<(), ChannelError> {
    let deadline = Instant::now() + Duration::from_millis(dispatch.remaining_ms);
    let mut reader = Reader { kernel, history, deadline, lost: None };
    let result = handle(&mut reader, dispatch.action.as_str(), &dispatch.resource, &dispatch.payload);
    if let Some(lost) = reader.lost {
        return Err(lost);
    }
    kernel.reply(dispatch.id, result)
}

#[cfg(test)]
mod tests;
