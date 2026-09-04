use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::mpsc,
    thread,
};

use crate::{
    emit::{
        events::{EventLine, Sink, encode, timestamp},
        title::{Mode as TitleMode, Titles},
    },
    osc::Report,
    rules::{RuleSet, evaluate},
    screen::Screen,
    state::{Config, Event, Machine},
};

pub struct Options {
    pub rule_set: Option<RuleSet>,
    pub agent: Option<crate::osc::AgentId>,
    pub no_osc: bool,
    pub title: TitleMode,
    pub events: Option<std::path::PathBuf>,
    pub debug: bool,
}

pub fn run(command: &str, argv: &[String], options: Options) -> Result<u8> {
    let system = NativePtySystem::default();
    let pair = system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;
    let mut builder = CommandBuilder::new(command);
    builder.args(argv);
    builder.env("ZOR_PID", std::process::id().to_string());
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("spawn wrapped command")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut writer = pair.master.take_writer().context("take pty writer")?;
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let Some(chunk) = buffer.get(..count) else {
                        break;
                    };
                    if output_tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut writer);
    });

    // The main loop is the sole output owner: child bytes are flushed before parsing.
    let mut screen = Screen::new(24, 80);
    let mut machine = Machine::new(Config::default());
    let mut titles = Titles::new(options.title);
    let mut sink = options.events.clone().map(Sink::connect);
    let mut queued = Vec::<Vec<u8>>::new();
    let mut stdout = io::stdout().lock();
    for chunk in output_rx {
        stdout.write_all(&chunk).context("write child output")?;
        stdout.flush().context("flush child output")?;
        let ground = screen.process(&chunk);
        if screen.changed() {
            let verdict = options.rule_set.as_ref().map(|set| evaluate(set, &screen));
            if options.debug
                && let Some(value) = &verdict
            {
                eprintln!("zor: verdict {:?} rule={:?}", value.state, value.rule);
            }
            let events = machine.observe(
                verdict,
                options.agent.clone(),
                child.process_id().and_then(|pid| i32::try_from(pid).ok()),
                false,
                std::time::Instant::now(),
            );
            queue_events(
                &events,
                &options,
                &screen,
                &mut titles,
                &mut sink,
                &mut queued,
            );
            screen.clear_changed();
        }
        let timer_events = machine.tick(std::time::Instant::now());
        queue_events(
            &timer_events,
            &options,
            &screen,
            &mut titles,
            &mut sink,
            &mut queued,
        );
        if ground {
            for bytes in queued.drain(..) {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
        }
    }
    let status = child.wait().context("wait for wrapped command")?;
    let _ = reader_thread.join();
    if let Some(bytes) = titles.restore() {
        stdout.write_all(&bytes)?;
        stdout.flush()?;
    }
    if options.debug
        && let Some(value) = sink
    {
        eprintln!("zor: dropped event lines: {}", value.dropped);
    }
    Ok(u8::try_from(status.exit_code()).unwrap_or(u8::MAX))
}

fn queue_events(
    events: &[Event],
    options: &Options,
    screen: &Screen,
    titles: &mut Titles,
    sink: &mut Option<Sink>,
    queued: &mut Vec<Vec<u8>>,
) {
    use crate::rules::view::ScreenView;
    for event in events {
        if options.debug {
            eprintln!("zor: machine event {event:?}");
        }
        let Event::Changed {
            state,
            previous,
            agent,
            seq,
            visible,
            exited,
        } = event
        else {
            continue;
        };
        if let Ok(report) = Report::new(*state, agent.clone(), *seq, *visible, *exited, None) {
            if !options.no_osc {
                queued.push(crate::osc::format(&report));
            }
            if let Some(title) = titles.observe(
                screen.title(),
                *state,
                agent.as_ref().map(crate::osc::AgentId::as_str),
            ) {
                queued.push(title);
            }
        }
        if let Some(target) = sink {
            let time = timestamp();
            let visible_names = [
                (visible.idle, "idle"),
                (visible.blocker, "blocker"),
                (visible.working, "working"),
            ]
            .into_iter()
            .filter_map(|(set, name)| set.then_some(name))
            .collect();
            let current_name = state_name(*state);
            let previous_name = state_name(*previous);
            let line = EventLine {
                t: &time,
                state: current_name,
                previous: Some(previous_name),
                agent: agent.as_ref().map(crate::osc::AgentId::as_str),
                seq: *seq,
                pid: None,
                code: None,
                title: Some(screen.title()),
                visible: visible_names,
                exited: *exited,
            };
            if let Ok(bytes) = encode(&line) {
                target.write(&bytes);
            }
        }
    }
}
fn state_name(state: crate::osc::State) -> &'static str {
    match state {
        crate::osc::State::Working => "working",
        crate::osc::State::Blocked => "blocked",
        crate::osc::State::Idle => "idle",
        crate::osc::State::None => "none",
    }
}

pub fn run_transparent(command: &str, argv: &[String]) -> Result<u8> {
    let status = std::process::Command::new(command)
        .args(argv)
        .status()
        .context("run nested wrapped command")?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}
