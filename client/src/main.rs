use std::process::ExitCode;

fn main() -> ExitCode {
    match syncengine_client::run(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
