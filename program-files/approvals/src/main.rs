//! Entry point for the approvals guard. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome =
        nucleus_approvals::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_approvals::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-approvals: {why}");
            ExitCode::FAILURE
        }
    }
}
