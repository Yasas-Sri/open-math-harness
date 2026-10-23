//! Entry point for the publish service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_publish::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_publish::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-publish: {why}");
            ExitCode::FAILURE
        }
    }
}
