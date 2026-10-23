use std::process::ExitCode;
fn main() -> ExitCode {
    match nucleus_mcp::Options::parse(std::env::args().skip(1)).and_then(nucleus_mcp::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("nucleus-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}
