//! Entry point for the UI service. See the library for its contract.
//!
//! `nucleus-ui github <platform.json>` is the GitHub broker (module `github`).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["github", config] => nucleus_ui::github::run(config.into()),
        _ => nucleus_ui::Options::parse(args).and_then(|o| nucleus_ui::run(&o)),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus-ui: {why}");
            ExitCode::FAILURE
        }
    }
}
