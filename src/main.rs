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
    let sets = zor::rules::bundle::load_all(&cli.rules)?;
    if let Some(action) = cli.action {
        return match action {
            cli::Action::Agents => {
                println!("no bundled agent rule sets");
                Ok(0)
            }
            cli::Action::Check { fixture, agent } => {
                check_fixture(&fixture, agent.as_deref(), &sets)
            }
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
    let agent = cli
        .agent
        .as_deref()
        .map(zor::osc::AgentId::new)
        .transpose()?;
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
                rule_sets: sets,
                agent,
                no_osc: cli.no_osc,
                title,
                events: cli.events,
                debug: cli.debug,
            },
        )
    }
}

fn check_fixture(
    path: &std::path::Path,
    forced: Option<&str>,
    sets: &[zor::rules::RuleSet],
) -> anyhow::Result<u8> {
    let source =
        zor::rules::bundle::read_bounded_utf8(path, zor::rules::bundle::MAX_FIXTURE_BYTES)?;
    let mut agent = forced.map(str::to_owned);
    let mut title = String::new();
    let mut progress = None;
    let mut expected = None;
    let mut matched = None;
    let mut body = Vec::new();
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("# agent: ") {
            if agent.is_none() {
                agent = Some(value.to_owned());
            }
        } else if let Some(value) = line.strip_prefix("# title: ") {
            title = value.to_owned();
        } else if let Some(value) = line.strip_prefix("# progress: ") {
            let mut fields = value.split(':');
            progress = fields
                .next()
                .and_then(|state| state.parse().ok())
                .zip(fields.next().and_then(|percent| percent.parse().ok()))
                .map(|(state, percent)| zor::rules::view::Progress { state, percent });
        } else if let Some(value) = line.strip_prefix("# expect: ") {
            expected = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("# matched: ") {
            matched = Some(value.to_owned());
        } else if !line.starts_with('#') {
            body.push(line.to_owned());
        }
    }
    let id = agent.ok_or_else(|| anyhow::anyhow!("fixture has no agent"))?;
    let set = sets
        .iter()
        .find(|set| set.id == id)
        .ok_or_else(|| anyhow::anyhow!("no loaded rule set for {id}"))?;
    let view = FixtureView::new(body, title, progress);
    let verdict = zor::rules::evaluate(set, &view);
    let state = format!("{:?}", verdict.state).to_lowercase();
    let rule = verdict.rule.as_deref().unwrap_or("none");
    println!("{state} {rule}");
    Ok(
        if expected.as_deref() == Some(state.as_str()) && matched.as_deref() == Some(rule) {
            0
        } else {
            1
        },
    )
}

struct FixtureView {
    lines: Vec<String>,
    text: String,
    title: String,
    progress: Option<zor::rules::view::Progress>,
}
impl FixtureView {
    fn new(
        lines: Vec<String>,
        title: String,
        progress: Option<zor::rules::view::Progress>,
    ) -> Self {
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        Self {
            lines,
            text,
            title,
            progress,
        }
    }
}
impl zor::rules::view::ScreenView for FixtureView {
    fn lines(&self) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
        self.lines
            .iter()
            .map(|line| std::borrow::Cow::Borrowed(line.as_str()))
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn progress(&self) -> Option<zor::rules::view::Progress> {
        self.progress
    }
    fn size(&self) -> (u16, u16) {
        (u16::try_from(self.lines.len()).unwrap_or(u16::MAX), 0)
    }
}
