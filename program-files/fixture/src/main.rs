//! Entry point for the fixture agent. See the library for its contract.

use std::process::ExitCode;

use nucleus_fixture_agent::{Options, Stop, run};

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(why) => {
            eprintln!("nucleus-fixture-agent: {why}");
            return ExitCode::FAILURE;
        }
    };
    match run(&options) {
        Ok(()) | Err(Stop::Terminated) => ExitCode::SUCCESS,
        Err(Stop::Failed(why)) => {
            eprintln!("nucleus-fixture-agent: {why}");
            ExitCode::FAILURE
        }
    }
}
