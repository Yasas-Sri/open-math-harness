//! Entry point for the discovery client. See the library for its contract.

use std::process::ExitCode;

use nucleus_discovery_client::{Exit, Options, run};
use nucleus_sdk::Connection;

fn main() -> ExitCode {
    let exit = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => match Connection::from_stdio() {
            Ok(kernel) => run(&options, kernel, &mut std::io::stderr()),
            Err(e) => Exit::Failed(e.to_string()),
        },
        Err(why) => Exit::Usage(why),
    };
    if exit != Exit::Done {
        eprintln!("nucleus-discovery-client: {exit}");
    }
    ExitCode::from(exit.code())
}
