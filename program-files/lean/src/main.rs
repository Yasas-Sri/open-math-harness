//! Entry point for the Lean verifier service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = nucleus_lean::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_lean::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-lean: {why}");
            ExitCode::FAILURE
        }
    }
}
