//! Entry point of the projects service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_projects::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_projects::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-projects: {why}");
            ExitCode::FAILURE
        }
    }
}
