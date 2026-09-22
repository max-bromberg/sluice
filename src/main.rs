use std::process::ExitCode;

use clap::Parser;

use sluice::cli::{self, Cli};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli::run(cli) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("sluice: {e:#}");
            ExitCode::from(1)
        }
    }
}
