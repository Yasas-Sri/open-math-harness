//! Entry point for the chat agent. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    // The swarm service is a mode of its own, with its own options.
    let outcome = if args.peek().is_some_and(|first| first == "--swarm-service") {
        nucleus_chat_agent::swarm::service::run(args.skip(1))
    } else {
        nucleus_chat_agent::Options::parse(args).and_then(|o| nucleus_chat_agent::run(&o))
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-chat-agent: {why}");
            ExitCode::FAILURE
        }
    }
}
