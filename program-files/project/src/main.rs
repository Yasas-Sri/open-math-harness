//! Entry point of a project's host. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_project::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_project::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-project: {why}");
            ExitCode::FAILURE
        }
    }
}
