//! Entry point for the git diff service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_diffs::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_diffs::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-diffs: {why}");
            ExitCode::FAILURE
        }
    }
}
