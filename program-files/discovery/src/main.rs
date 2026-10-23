//! Entry point of the discovery program: the service, or, when the first
//! argument is `run`, the process of one run. See the library for their
//! contract.

use std::process::ExitCode;

use nucleus_discovery::{Options, run_process, service, wire};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = if args.first().is_some_and(|mode| mode == wire::RUN_MODE) {
        match wire::RunCommand::parse(args) {
            Ok(run) => run_process::serve(&run),
            Err(wire::CommandError::Protocol { service }) => run_process::refuse_protocol(service),
            Err(e) => Err(e.to_string()),
        }
    } else {
        Options::parse(args).and_then(|options| service::serve(&options))
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-discovery: {why}");
            ExitCode::FAILURE
        }
    }
}
