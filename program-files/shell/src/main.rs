//! Entry point for the shell service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_shell::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_shell::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-shell: {why}");
            ExitCode::FAILURE
        }
    }
}
