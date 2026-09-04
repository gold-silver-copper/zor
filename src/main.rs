use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Command and arguments to wrap.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    let _ = Cli::parse();
    eprintln!("zor: wrapper runtime is not implemented until Phase Z");
    ExitCode::FAILURE
}
