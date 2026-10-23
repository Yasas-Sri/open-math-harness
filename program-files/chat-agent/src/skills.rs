//! Skills (the `skills` service): what the instructions say of them, and the
//! person's `/skill` command.
//!
//! The list is read at the start, like what is remembered, and again before
//! each turn while it could not be read: the project's skills and the
//! published ones. An agent that holds no
//! `skills list`, or whose list is empty or could not be read, has no skills
//! section; the list is short-deadlined, since it is on the path to the
//! first answer.
//!
//! A person's message that begins with `/skill NAME` or `/skill URL` loads
//! that skill: this agent makes the `skills_load` call itself, after the
//! message and before the model's first step, posted and remembered as any
//! tool call is, so the model reads the skill as a result and a later agent
//! of the session restores it from the log (module `restore`). `/skill`
//! alone lists them (`skills_list`). The message itself stays as the person
//! wrote it.

use nucleus_conversation_interface::Entry;
use nucleus_model_interface::{Message, Role, ToolCall};
use nucleus_protocol::{Error, ErrorCode};
use nucleus_sdk::ChannelError;
use nucleus_skills_interface as skills;
use serde_json::{Value, json};

/// How long the list read at the start may take.
const LIST_DEADLINE_MS: u64 = 5_000;
/// How long a load may take: it may download a skill first.
const LOAD_DEADLINE_MS: u64 = 120_000;

/// The list of skills, from the list read now: `Ok(None)` when this agent holds no skills
/// or there are none, `Err(())` when they could not be listed (another call held the
/// service, for one), to be read again before a later turn.
pub(crate) fn section(kernel: &mut crate::Kernel) -> Result<Option<String>, ()> {
    let Some(handle) = kernel.find_handle(skills::ENDPOINT, skills::ACTION_LIST) else {
        return Ok(None);
    };
    let Some(resource) = kernel.handles().iter().find(|h| h.handle == handle).map(|h| h.resource.clone())
    else {
        return Ok(None);
    };
    let reply =
        kernel.call(handle, skills::ACTION_LIST, &resource, &skills::ListRequest {}, Some(LIST_DEADLINE_MS));
    let listed = match reply.map_err(|e| e.to_string()).and_then(|r| {
        r.and_then(|payload| payload.decode::<skills::ListResponse>())
            .map_err(|e| format!("{:?}: {}", e.code, e.message))
    }) {
        Ok(listed) => listed,
        Err(e) => {
            eprintln!("nucleus-chat-agent: the skills could not be listed: {e}");
            return Err(());
        }
    };
    let shown: Vec<nucleus_prompts::skills::Listed<'_>> = listed
        .skills
        .iter()
        .map(|s| nucleus_prompts::skills::Listed {
            name: &s.name,
            description: &s.description,
            installed: s.installed,
        })
        .collect();
    Ok(nucleus_prompts::skills::catalog(&shown))
}

/// The `/skill` command a message begins with: `Some(None)` for `/skill`
/// alone, `Some(Some(argument))` with a skill's name or URL, `None` for any
/// other message.
pub(crate) fn command(text: &str) -> Option<Option<String>> {
    let rest = text.trim_start().strip_prefix("/skill")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some(rest.split_whitespace().next().map(str::to_owned))
}

/// The skills a message's leading `/skill NAME` lines name (the composer's
/// tags), up to four and each once; without such lines, the one [`command`]
/// the message begins with, a URL load or `/skill` listing included. Plain
/// mentions in prose are never commands.
pub(crate) fn commands(text: &str) -> Vec<Option<String>> {
    let mut rest = text;
    let mut selected = Vec::new();
    for _ in 0..4 {
        let Some((line, tail)) = rest.split_once('\n') else { break };
        let Some(name) = line.trim_end_matches('\r').strip_prefix("/skill ") else { break };
        if !skills::valid_name(name) {
            break;
        }
        let name = Some(name.to_owned());
        if !selected.contains(&name) {
            selected.push(name);
        }
        rest = tail;
    }
    if selected.is_empty() { command(text).into_iter().collect() } else { selected }
}

/// The call that answers a `/skill` command: the tool and its arguments.
fn call_for(argument: Option<&str>) -> (String, Value) {
    let tool = |action| format!("{}_{action}", skills::ENDPOINT);
    match argument {
        None => (tool(skills::ACTION_LIST), json!({})),
        Some(url) if url.starts_with("https://") || url.starts_with("http://") => {
            (tool(skills::ACTION_LOAD), json!({"url": url}))
        }
        Some(name) => (tool(skills::ACTION_LOAD), json!({"name": name})),
    }
}

impl crate::Agent<'_> {
    /// Makes the calls the `/skill` commands of the messages just
    /// remembered stand for, in their order (module doc).
    pub(crate) fn run_skill_commands(&mut self) -> Result<(), ChannelError> {
        for argument in std::mem::take(&mut self.skill_commands) {
            let (name, arguments) = call_for(argument.as_deref());
            let call = ToolCall { id: format!("{}-skill", self.next_message_id()), name, arguments };
            self.remember(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![call.clone()],
                ..Message::text(Role::Assistant, String::new())
            });
            self.post(&Entry::ToolCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: crate::tool_arguments_preview(&call.arguments),
            })?;
            let outcome = self.skills_call(&call)?;
            let (error_code, summary) = match &outcome {
                Ok(result) => (None, crate::tool_result_preview(result)),
                Err(e) => (Some(e.code), crate::tool_result_preview(&e.message)),
            };
            self.remember(crate::tool_message(&call, &outcome));
            self.post(&Entry::ToolResult { call_id: call.id.clone(), error_code, summary })?;
        }
        Ok(())
    }

    /// One `skills` call on the agent's own capability, with the deadline a
    /// download needs.
    fn skills_call(&mut self, call: &ToolCall) -> Result<Result<String, Error>, ChannelError> {
        let action = call.name.strip_prefix("skills_").unwrap_or_default();
        let Some(handle) = self.kernel.find_handle(skills::ENDPOINT, action) else {
            return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent cannot load skills")));
        };
        let Some(resource) =
            self.kernel.handles().iter().find(|h| h.handle == handle).map(|h| h.resource.clone())
        else {
            return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent cannot load skills")));
        };
        let reply = self.kernel.call(handle, action, &resource, &call.arguments, Some(LOAD_DEADLINE_MS))?;
        Ok(reply.and_then(|payload| payload.decode::<Value>()).map(crate::bounded_result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_skills_load_in_order_without_interpreting_prose_mentions() {
        assert_eq!(
            commands("/skill lean-workflow\n/skill lmfdb-query\nCompare @foo"),
            vec![Some("lean-workflow".into()), Some("lmfdb-query".into())]
        );
        assert_eq!(commands("/skill A_b.c\r\nCheck it"), vec![Some("A_b.c".into())]);
        assert_eq!(commands("/skill a\n/skill a\nCheck it"), vec![Some("a".into())]);
        assert!(commands("Use @lean-workflow").is_empty());
        assert_eq!(
            commands("/skill https://example.org/a.zip check it"),
            vec![Some("https://example.org/a.zip".into())]
        );
        assert_eq!(commands("/skill"), vec![None]);
    }

    #[test]
    fn a_message_beginning_with_skill_is_the_command() {
        assert_eq!(command("/skill lean-workflow prove it"), Some(Some("lean-workflow".into())));
        assert_eq!(command("  /skill\n"), Some(None));
        assert_eq!(command("/skills"), None);
        assert_eq!(command("use /skill x"), None);
        assert_eq!(call_for(Some("https://x.org/a.zip")).1, json!({"url": "https://x.org/a.zip"}));
        assert_eq!(call_for(Some("proof")), ("skills_load".to_owned(), json!({"name": "proof"})));
        assert_eq!(call_for(None).0, "skills_list");
    }
}
