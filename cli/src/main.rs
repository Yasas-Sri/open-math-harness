//! `nucleus run <config.json>`,
//! `nucleus serve <config.json> [--parameters <values.json>]`.
//!
//! With `--parameters`, the platform's values file fills the configuration's
//! parameters.

use std::path::Path;
use std::process::ExitCode;

use nucleus_cli::{LauncherConfig, run};

const USAGE: &str =
    "usage: nucleus run <config.json>\n       nucleus serve <config.json> [--parameters <values.json>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["serve", config] => nucleus_cli::serve::serve(Path::new(config), None),
        ["serve", config, "--parameters", values] => {
            nucleus_cli::serve::serve(Path::new(config), Some(Path::new(values)))
        }
        ["run", config] => LauncherConfig::load(Path::new(config)).and_then(run),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nucleus: {why}");
            ExitCode::FAILURE
        }
    }
}
