//! Reminders the agent sets about its own conversation (conversation
//! interface, "Reminders").
//!
//! A turn starts when `receive` returns the person's messages or a due
//! reminder (`queued_prompt`). The agent comes back to long work, such as a
//! discovery run, by setting a reminder, not by blocking in a tool call.
//!
//! The model is offered `conversation_remind`, `conversation_reminders` and
//! `conversation_cancel_reminder` (the interface's `agent_tools`), each only
//! when this agent holds that action on its own session and waits for
//! messages there: a one-shot agent would never receive what it set, so it is
//! offered none, and a call to one is refused as a tool it does not have.
//! Every call goes to the agent's own session, which is never a model
//! argument, and the service records who set a reminder from what the kernel
//! checked. The agent hands no conversation action on: its grants on the
//! conversation are not delegable (`sessions`), and a plugin gets none
//! (`plugins::delegated`).
//!
//! A due reminder arrives through `receive` and starts a turn like a message
//! (the conversation service holds it until a running turn ends). The model
//! sees it inside the frame `reminder_for_model` draws, which names the
//! source and marks the text as untrusted, never as the person's words.

use nucleus_conversation_interface::{self as conversation, ReceiveResponse};
use nucleus_model_interface::{ReasoningMode, ToolSpec};
use nucleus_protocol::{Error, ErrorCode, ResourcePath};
use nucleus_sdk::ChannelError;
use serde_json::Value;

const PREFIX: &str = "conversation_";

/// Whether the reminder tools are offered, and so whether a call to one
/// runs: only an agent that receives its session's messages would get a
/// reminder back, so a one-shot agent has none.
fn offers(options: &crate::Options) -> bool {
    options.task.is_none()
}

/// The reminder tools this agent offers its model: those it holds on its own
/// `session`, when it [`offers`] any.
pub(crate) fn offer(
    options: &crate::Options,
    kernel: &crate::Kernel,
    session: &ResourcePath,
) -> Vec<ToolSpec> {
    if !offers(options) {
        return Vec::new();
    }
    specs(|action| kernel.find_handle_for(conversation::ENDPOINT, action, session).is_some())
}

/// The conversation action a model's tool call names, when it is a reminder
/// tool this agent offers. One that was not offered is a tool this agent does
/// not have.
pub(crate) fn offered(options: &crate::Options, name: &str) -> Option<&'static str> {
    action(name).filter(|_| offers(options))
}

/// The reminder tools for the actions `holds` says this agent may call on
/// its own session. `wake` is never among them.
pub fn specs(holds: impl Fn(&str) -> bool) -> Vec<ToolSpec> {
    conversation::agent_tools()
        .into_iter()
        .filter(|tool| holds(tool.action))
        .map(|tool| ToolSpec {
            name: format!("{PREFIX}{}", tool.action),
            description: tool.description.to_owned(),
            parameters: tool.params,
        })
        .collect()
}

/// The conversation action a reminder tool calls, when `name` is one.
pub fn action(name: &str) -> Option<&'static str> {
    let wanted = name.strip_prefix(PREFIX)?;
    conversation::agent_tools().into_iter().map(|tool| tool.action).find(|action| *action == wanted)
}

/// Calls `action` on the agent's own `session` with the model's arguments as
/// the payload; the service validates them and refuses unknown fields.
pub(crate) fn call(
    kernel: &mut crate::Kernel,
    session: &ResourcePath,
    action: &str,
    arguments: &Value,
) -> Result<Result<String, Error>, ChannelError> {
    let Some(handle) = kernel.find_handle_for(conversation::ENDPOINT, action, session) else {
        return Ok(Err(Error::new(ErrorCode::PermissionDenied, "this agent holds no such capability")));
    };
    let payload = if arguments.is_null() { &Value::Object(serde_json::Map::new()) } else { arguments };
    let reply = kernel.call(handle, action, session, payload, None)?;
    Ok(reply.and_then(|payload| payload.decode::<Value>()).map(|value| crate::truncate(&value.to_string())))
}

/// One turn's prompt from what `receive` returned: the person's messages in the
/// order they sent them, each as `prompt` makes it, then each due reminder
/// inside the frame that says it is untrusted data from its source, not the
/// person. Once more than one person has written in the session (`authors`,
/// which this adds to), each message begins with the line naming its author.
/// Each author's delivered mode is tracked here, also during history replay. Only
/// their first message switching Ultramath off carries the explicit stop instruction;
/// reminders and later standard messages do not repeat it. No new log format is needed:
/// replay derives the same instruction from delivered messages' saved settings.
/// `None` when it returned nothing.
pub(crate) fn queued_prompt<E>(
    received: ReceiveResponse,
    authors: &mut std::collections::BTreeMap<Option<String>, ReasoningMode>,
    mut prompt: impl FnMut(conversation::UserMessage) -> Result<String, E>,
) -> Result<Option<String>, E> {
    let ReceiveResponse { message, rest, reminders } = received;
    let messages: Vec<conversation::UserMessage> = message.into_iter().chain(rest).collect();
    for message in &messages {
        authors.entry(message.author.clone()).or_default();
    }
    let several = authors.len() > 1;
    let mut parts = Vec::with_capacity(messages.len() + reminders.len());
    for message in messages {
        let line = several.then(|| crate::author_line(message.author.as_deref()));
        let mode = message.settings.as_ref().and_then(|s| s.reasoning_mode).unwrap_or_default();
        let previous = authors.insert(message.author.clone(), mode).unwrap_or_default();
        let mut text = prompt(message)?;
        if previous == ReasoningMode::Ultramath && mode == ReasoningMode::Standard {
            text.push_str("\n\n[The user has explicitly requested turning off Ultramath. Stop any active Ultramath run with ultramath_stop and continue without Ultramath. Do not start or resume Ultramath unless the user explicitly requests it again or re-enables Ultramath mode.]");
        }
        parts.push(match line {
            Some(line) => format!("{line}\n{text}"),
            None => text,
        });
    }
    parts.extend(reminders.iter().map(conversation::reminder_for_model));
    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt of messages without files: their text and inline attachments.
    fn queued_prompt(
        received: ReceiveResponse,
        authors: &mut std::collections::BTreeMap<Option<String>, ReasoningMode>,
    ) -> Option<String> {
        let inline = |message: conversation::UserMessage| {
            Ok::<_, std::convert::Infallible>(crate::attachments::inline(&message))
        };
        match super::queued_prompt(received, authors, inline) {
            Ok(prompt) => prompt,
            Err(never) => match never {},
        }
    }

    fn from(author: Option<&str>, text: &str) -> conversation::UserMessage {
        conversation::UserMessage {
            id: text.into(),
            text: text.into(),
            attachments: Vec::new(),
            files: Vec::new(),
            images: Vec::new(),
            author: author.map(str::to_owned),
            settings: None,
        }
    }

    #[test]
    fn queued_messages_and_due_reminders_are_one_prompt_in_order() {
        let message = |text: &str| from(None, text);
        let received = |message, rest, reminders| ReceiveResponse { message, rest, reminders };
        let mut authors = std::collections::BTreeMap::new();
        let mut queued_prompt = |received| queued_prompt(received, &mut authors);
        assert_eq!(
            queued_prompt(received(Some(message("one")), vec![message("two"), message("three")], vec![])),
            Some("one\n\ntwo\n\nthree".into())
        );
        assert_eq!(queued_prompt(received(Some(message("only")), vec![], vec![])), Some("only".into()));
        assert_eq!(queued_prompt(received(None, vec![], vec![])), None);
        // A reminder alone is a turn, and never reads as the person's words.
        let reminder = conversation::Reminder {
            id: "r4".into(),
            text: "check run gen2".into(),
            source: conversation::ReminderSource {
                action: conversation::ReminderAction::Remind,
                process: nucleus_protocol::ProcessId(7),
            },
            set_at_ms: 1,
            due_at_ms: 60_001,
        };
        let framed = conversation::reminder_for_model(&reminder);
        assert_eq!(queued_prompt(received(None, vec![], vec![reminder.clone()])), Some(framed.clone()));
        assert_eq!(
            queued_prompt(received(Some(message("hi")), vec![], vec![reminder])),
            Some(format!("hi\n\n{framed}"))
        );
    }

    /// One person's messages read as before; once a second person has
    /// written in the session, every message names its author, from then on.
    #[test]
    fn messages_name_their_authors_once_several_people_have_written() {
        let received = |message, rest| ReceiveResponse { message, rest, reminders: vec![] };
        let mut authors = std::collections::BTreeMap::new();
        assert_eq!(
            queued_prompt(received(Some(from(Some("Ada"), "hi")), vec![]), &mut authors),
            Some("hi".into())
        );
        assert_eq!(
            queued_prompt(
                received(Some(from(Some("Ada"), "one")), vec![from(Some("Grace \"Hopper\""), "two")]),
                &mut authors
            ),
            Some("[from \"Ada\"]\none\n\n[from \"Grace \\\"Hopper\\\"\"]\ntwo".into())
        );
        assert_eq!(
            queued_prompt(received(Some(from(None, "three")), vec![]), &mut authors),
            Some("[from a connection without an account]\nthree".into())
        );
        assert_eq!(
            queued_prompt(received(Some(from(Some("Ada"), "four")), vec![]), &mut authors),
            Some("[from \"Ada\"]\nfour".into())
        );
    }

    #[test]
    fn only_held_reminder_actions_are_offered_and_wake_never_is() {
        let names = |held: &[&str]| -> Vec<String> {
            specs(|action| held.contains(&action)).into_iter().map(|spec| spec.name).collect()
        };
        let all = [
            conversation::ACTION_REMIND,
            conversation::ACTION_REMINDERS,
            conversation::ACTION_CANCEL_REMINDER,
            conversation::ACTION_WAKE,
        ];
        assert_eq!(
            names(&all),
            ["conversation_remind", "conversation_reminders", "conversation_cancel_reminder"]
        );
        assert!(names(&[conversation::ACTION_RECEIVE, conversation::ACTION_POST]).is_empty());
        for spec in specs(|_| true) {
            assert_eq!(action(&spec.name), Some(&spec.name[PREFIX.len()..]));
        }
        for other in ["conversation_wake", "conversation_post", "remind", "fs_read"] {
            assert_eq!(action(other), None, "{other}");
        }
    }
}
