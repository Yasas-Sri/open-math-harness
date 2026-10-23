use std::process::ExitCode;

fn main() -> ExitCode {
    let result = nucleus_plugin_development::Options::parse(std::env::args().skip(1))
        .and_then(|options| nucleus_plugin_development::run(&options));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("nucleus-plugin-development: {message}");
            ExitCode::FAILURE
        }
    }
}
