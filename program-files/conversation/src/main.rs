//! Entry point for the conversation service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = nucleus_conversation::Options::parse(std::env::args().skip(1))
        .and_then(|o| nucleus_conversation::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-conversation: {why}");
            ExitCode::FAILURE
        }
    }
}
