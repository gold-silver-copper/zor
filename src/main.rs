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
    let sets = zor::rules::bundle::load_all(&cli.rules)?;
    let agent = cli
        .agent
        .as_deref()
        .map(zor::osc::AgentId::new)
        .transpose()?;
    let rule_set = agent
        .as_ref()
        .and_then(|id| sets.iter().find(|set| set.id == id.as_str()))
        .cloned();
    let title = match cli.title {
        cli::TitleMode::Never => zor::emit::title::Mode::Never,
        cli::TitleMode::Prefix => zor::emit::title::Mode::Prefix,
        cli::TitleMode::Replace => zor::emit::title::Mode::Replace,
    };
    if std::env::var_os("ZOR_PID").is_some() {
        zor::pty::run_transparent(&command, &argv)
    } else {
        zor::pty::run(
            &command,
            &argv,
            zor::pty::Options {
                rule_set,
                agent,
                no_osc: cli.no_osc,
                title,
                events: cli.events,
                debug: cli.debug,
            },
        )
    }
}
