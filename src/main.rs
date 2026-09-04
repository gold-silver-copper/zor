use clap::Parser;
use std::process::ExitCode;

mod cli;

fn main() -> ExitCode {
    match run(cli::Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("zor: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: cli::Cli) -> anyhow::Result<u8> {
    if let Some(action) = cli.action {
        return match action {
            cli::Action::Agents => {
                println!("no bundled agent rule sets");
                Ok(0)
            }
            cli::Action::Check { fixture, agent } => anyhow::bail!(
                "fixture checking is not yet available for {}{}",
                fixture.display(),
                agent.map_or_else(String::new, |id| format!(" using {id}"))
            ),
        };
    }
    let command = cli
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()));
    let argv = if cli.command.is_empty() {
        vec!["-l".to_owned()]
    } else {
        cli.command.into_iter().skip(1).collect()
    };
    if std::env::var_os("ZOR_PID").is_some() {
        zor::pty::run_transparent(&command, &argv)
    } else {
        zor::pty::run(&command, &argv)
    }
}
