//! Entry point for the memory service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_memory::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_memory::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-memory: {why}");
            ExitCode::FAILURE
        }
    }
}
