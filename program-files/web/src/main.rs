//! Entry point for the web fetch service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = nucleus_web::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_web::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-web: {why}");
            ExitCode::FAILURE
        }
    }
}
