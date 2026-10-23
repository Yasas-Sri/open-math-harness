//! Entry point for the OpenRouter adapter. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = nucleus_openrouter::Options::parse(std::env::args().skip(1))
        .and_then(|o| nucleus_openrouter::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-openrouter: {why}");
            ExitCode::FAILURE
        }
    }
}
