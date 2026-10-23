//! Entry point of `nucleus-as`. See the library for what it checks.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Err(why) = nucleus_as::run(std::env::args_os().collect());
    eprintln!("nucleus-as: {why}");
    ExitCode::from(126)
}
