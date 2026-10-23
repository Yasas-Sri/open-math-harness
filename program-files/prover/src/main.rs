//! Entry point for the prover agent. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_prover::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_prover::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-prover: {why}");
            ExitCode::FAILURE
        }
    }
}
