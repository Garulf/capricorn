mod cli;
mod client;
mod commands;
mod error;
mod password;
mod render;
mod waybar;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::error::EXIT_FAILURE;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(if err.use_stderr() { EXIT_FAILURE } else { 0 });
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("capricorn: cannot start runtime: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    match runtime.block_on(commands::run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("capricorn: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
