//! Entry point for the PDF text service. See the library for its contract.

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = nucleus_pdf::Options::parse(std::env::args().skip(1)).and_then(|o| nucleus_pdf::run(&o));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-pdf: {why}");
            ExitCode::FAILURE
        }
    }
}
