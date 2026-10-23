//! An ordinary service that returns a plugin development guide from its package.
//!
//! It reads the guide (`--guide`, `resources/guide.md` of its own package)
//! once, at startup, so a changed guide takes effect when the program
//! restarts, without a rebuild. A guide that is missing, not UTF-8 or longer
//! than `MAX_GUIDE_RESULT_CHARS` as its JSON result fails startup: there is no
//! embedded copy to fall back on that could hide a broken package. `guide`
//! takes `{}` and answers `{"guide": "..."}`, which reaches the model as an
//! ordinary tool result (`plugin-development_guide`), never as a system
//! instruction. Instructions grant no authority: a compromised guide can
//! mislead its reader but gains no capability.
//!
//! It needs authority to provide its endpoint and read access to its guide file,
//! but no credentials or network. In the VM its only grant publishes its
//! endpoint, and its package directory is bound read-only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use nucleus_protocol::{
    ACTION_PROVIDE, ActionDecl, ActionName, EffectClass, EndpointName, Error, ErrorCode, InterfaceDecl,
    KERNEL_ENDPOINT, Payload, ResourceFrom, ResourcePath,
};
use nucleus_sdk::{Connection, Event};
use serde_json::{Map, Value, json};

pub const ENDPOINT: &str = "plugin-development";
pub const ACTION_GUIDE: &str = "guide";
// The chat tool-result budget includes JSON escaping. Keep this program
// independent of the agent implementation, and reject oversized resources.
const MAX_GUIDE_RESULT_CHARS: usize = 8_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub endpoint: EndpointName,
    pub guide: PathBuf,
}

impl Options {
    /// Parses required `--guide <path>` and optional `--endpoint <name>`.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut endpoint = ENDPOINT.to_owned();
        let mut guide = None;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--endpoint" => endpoint = args.next().ok_or("--endpoint needs a value")?,
                "--guide" => guide = Some(PathBuf::from(args.next().ok_or("--guide needs a value")?)),
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        Ok(Self {
            endpoint: EndpointName::new(endpoint).map_err(|error| error.to_string())?,
            guide: guide.ok_or("--guide <path> is required")?,
        })
    }
}

fn read_guide(path: &Path) -> Result<String, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("read guide {}: {error}", path.display()))?;
    // UTF-8 uses at most four bytes per character. Bound reads before decoding
    // so a replaced resource cannot cause unbounded startup allocation.
    let max_bytes = MAX_GUIDE_RESULT_CHARS * 4;
    let mut bytes = Vec::new();
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read guide {}: {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err("guide exceeds the tool-result budget".into());
    }
    let guide = String::from_utf8(bytes).map_err(|_| "guide must be valid UTF-8")?;
    if json!({"guide":guide}).to_string().chars().count() > MAX_GUIDE_RESULT_CHARS {
        return Err("guide exceeds the tool-result budget".into());
    }
    Ok(guide)
}

pub fn interface(endpoint: EndpointName) -> Result<InterfaceDecl, Error> {
    Ok(InterfaceDecl {
        endpoint,
        version: 1,
        description: "Plugin development instructions, returned by an ordinary service. Instructions do not grant capabilities.".into(),
        actions: BTreeMap::from([(
            ActionName::new(ACTION_GUIDE)?,
            ActionDecl::tool(
                EffectClass::Observation,
                "Read the guide before creating, starting or updating an Open Math OS plugin.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
                ResourceFrom::Handle,
            ),
        )]),
    })
}

fn serve(guide: &str, action: &str, payload: &Payload) -> Result<Payload, Error> {
    if action != ACTION_GUIDE {
        return Err(Error::new(ErrorCode::InvalidInput, "unknown action"));
    }
    if !payload.decode::<Map<String, Value>>()?.is_empty() {
        return Err(Error::new(ErrorCode::InvalidInput, "guide takes no arguments"));
    }
    Payload::encode(&json!({"guide":guide}))
}

/// Publishes the guide and replies through the same SDK channel as any service.
pub fn run(options: &Options) -> Result<(), String> {
    let guide = read_guide(&options.guide)?;
    let mut kernel = Connection::from_stdio().map_err(|error| error.to_string())?;
    let resource = ResourcePath::new(["endpoint", options.endpoint.as_str()]).map_err(|e| e.to_string())?;
    let authority = kernel
        .find_handle_for(KERNEL_ENDPOINT, ACTION_PROVIDE, &resource)
        .ok_or("started without authority to provide this endpoint")?;
    kernel
        .provide(authority, interface(options.endpoint.clone()).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    loop {
        match kernel.next_event().map_err(|error| error.to_string())? {
            Event::Dispatch(dispatch) => {
                kernel
                    .reply(dispatch.id, serve(&guide, dispatch.action.as_str(), &dispatch.payload))
                    .map_err(|error| error.to_string())?;
            }
            Event::CancelDispatch(_) | Event::ChildExited(_) | Event::ChildReaped { .. } => {}
            Event::Terminate => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn requires_guide_and_accepts_an_optional_endpoint() {
        let options = Options::parse(["--guide", "resources/guide.md"].map(str::to_owned)).unwrap();
        assert_eq!(options.endpoint.as_str(), ENDPOINT);
        assert_eq!(options.guide, Path::new("resources/guide.md"));
        let custom =
            Options::parse(["--guide", "guide.md", "--endpoint", "dynamic.7"].map(str::to_owned)).unwrap();
        assert_eq!(custom.endpoint.as_str(), "dynamic.7");
        for args in [
            vec![],
            vec!["--guide"],
            vec!["--endpoint"],
            vec!["--guide", "guide.md", "--endpoint", "bad/name"],
            vec!["--root", "/data"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn guide_is_complete_and_small_enough_for_a_chat_tool_result() {
        let guide = read_guide(&Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/guide.md")).unwrap();
        assert!(!guide.trim().is_empty());
        let result = serve(&guide, ACTION_GUIDE, &Payload::encode(&json!({})).unwrap()).unwrap();
        let value: Value = result.decode().unwrap();
        assert_eq!(value, json!({"guide":guide}));
        // The chat tool-result budget is 64,000 characters, including JSON
        // escaping. Keep this service independent of the agent implementation.
        assert!(
            value.to_string().chars().count() <= MAX_GUIDE_RESULT_CHARS,
            "chat must return the entire guide"
        );
    }

    /// A guide that is missing, not UTF-8 or too large once escaped as a tool
    /// result is refused before the service publishes anything.
    #[test]
    fn missing_invalid_or_oversized_guide_fails_before_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let guide = directory.path().join("guide.md");
        assert!(read_guide(&guide).is_err(), "missing");
        // Each newline needs JSON escaping: the escaped result counts, not the source text.
        for content in [vec![0xff], b"\n".repeat(4_000), b"x".repeat(32_001)] {
            std::fs::write(&guide, &content).unwrap();
            assert!(read_guide(&guide).is_err(), "{} bytes", content.len());
        }
    }

    #[test]
    fn rejects_unknown_actions_and_nonempty_or_nonobject_arguments() {
        for args in [json!({"topic":"native"}), json!([]), json!(null), json!("guide")] {
            let error = serve("guide", ACTION_GUIDE, &Payload::encode(&args).unwrap()).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidInput);
        }
        assert_eq!(
            serve("guide", "list", &Payload::encode(&json!({})).unwrap()).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }
}
