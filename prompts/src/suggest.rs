//! The texts of the suggestions service's model calls: the next message the
//! person might send, which a window offers in an empty composer, and the
//! short title of a session. The service sends them, outside any session's
//! agent.

/// After a message: the system text, followed by the conversation so far as
/// one user message holding a transcript (the model is not a party to it).
pub const NEXT: &str = include_str!("../templates/suggest/next.md");

/// Before the first message: a likely opening, with nothing to go on.
pub const START: &str = include_str!("../templates/suggest/start.md");

/// A next-message suggestion with optional choices from a supplied skill catalog.
pub const SKILLS: &str = include_str!("../templates/suggest/skills.md");

/// The session's title: the system text, followed by only the first user message,
/// enclosed in transcript tags. It does not wait for the session's agent to answer.
pub const TITLE: &str = include_str!("../templates/suggest/title.md");
