use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySystem};
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
    rules::{RuleSet, evaluate, view::ScreenView},
    screen::Screen,
    state::{Config, Event, Machine},
};

pub struct Options {
    pub rule_sets: Vec<RuleSet>,
    pub agent: Option<crate::osc::AgentId>,
    pub no_osc: bool,
    pub title: TitleMode,
    pub events: Option<std::path::PathBuf>,
    pub debug: bool,
}

pub fn run(command: &str, argv: &[String], options: Options) -> Result<u8> {
    let system = NativePtySystem::default();
    let initial_size = crate::platform::winsize(0);
    let mut raw_guard = crate::platform::set_raw(0).ok();
    let pair = system.openpty(initial_size).context("open pty")?;
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
    enum Message {
        Chunk(Vec<u8>),
        Eof,
        Signal(i32),
    }
    let (output_tx, output_rx) = mpsc::channel::<Message>();
    let reader_tx = output_tx.clone();
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let Some(chunk) = buffer.get(..count) else {
                        break;
                    };
                    if reader_tx.send(Message::Chunk(chunk.to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = reader_tx.send(Message::Eof);
    });
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut writer);
        // A terminal has no pipe-style EOF. portable-pty's writer drop synthesizes
        // VEOF, which can be echoed into stdout and violate byte passthrough.
        std::mem::forget(writer);
    });

    let signal_tx = output_tx;
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGTSTP,
        signal_hook::consts::SIGCONT,
        signal_hook::consts::SIGUSR1,
    ])
    .context("register signal handlers")?;
    thread::spawn(move || {
        for signal in signals.forever() {
            if signal_tx.send(Message::Signal(signal)).is_err() {
                break;
            }
        }
    });

    // The main loop is the sole output owner: child bytes are flushed before parsing.
    let mut screen = Screen::new(initial_size.rows, initial_size.cols);
    let mut machine = Machine::new(Config::default());
    let mut titles = Titles::new(options.title);
    let mut sink = options.events.clone().map(Sink::connect);
    let mut queued = Vec::<Vec<u8>>::new();
    let mut stdout = io::stdout().lock();
    let child_pid = child.process_id().and_then(|pid| i32::try_from(pid).ok());
    let mut active_agent = options.agent.clone();
    let mut scheduler = crate::platform::probe::Scheduler::new(std::time::Instant::now());
    let mut last_pgid = None;
    let mut loss_tracker = crate::platform::probe::LossTracker::new();
    let mut last_title = String::new();
    loop {
        let message = output_rx.recv_timeout(std::time::Duration::from_millis(50));
        let chunk = match message {
            Ok(Message::Chunk(chunk)) => Some(chunk),
            Ok(Message::Eof) => break,
            Ok(Message::Signal(signal)) => {
                if signal == signal_hook::consts::SIGWINCH {
                    let size = crate::platform::winsize(0);
                    pair.master.resize(size).context("resize pty")?;
                    screen.resize(size.rows, size.cols);
                } else if signal == signal_hook::consts::SIGUSR1 {
                    write_fixture(&screen, options.agent.as_ref());
                } else if signal == signal_hook::consts::SIGTSTP {
                    if let Some(pid) = child_pid {
                        let _ = crate::platform::forward_signal(pid, signal);
                    }
                    drop(raw_guard.take());
                    crate::platform::suspend_self();
                    raw_guard = crate::platform::set_raw(0).ok();
                } else if signal == signal_hook::consts::SIGCONT {
                    if raw_guard.is_none() {
                        raw_guard = crate::platform::set_raw(0).ok();
                    }
                    if let Some(pid) = child_pid {
                        let _ = crate::platform::forward_signal(pid, signal);
                    }
                } else if let Some(pid) = child_pid {
                    let _ = crate::platform::forward_signal(pid, signal);
                }
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(chunk) = chunk {
            stdout.write_all(&chunk).context("write child output")?;
            stdout.flush().context("flush child output")?;
            let _ = screen.process(&chunk);
            for payload in screen.take_observed_reports() {
                if options.debug {
                    eprintln!(
                        "zor: observed child OSC {}",
                        String::from_utf8_lossy(&payload)
                    );
                }
            }
            if screen.title() != last_title {
                last_title = screen.title().to_owned();
                let (state, _, _) = machine.current();
                if let Some(bytes) = titles.observe(
                    &last_title,
                    state,
                    active_agent.as_ref().map(crate::osc::AgentId::as_str),
                ) {
                    queued.push(bytes);
                }
            }
            if screen.changed() {
                if active_agent.is_none() {
                    scheduler.screen_changed_without_agent(std::time::Instant::now());
                }
                let verdict = active_agent
                    .as_ref()
                    .and_then(|id| options.rule_sets.iter().find(|set| set.id == id.as_str()))
                    .map(|set| evaluate(set, &screen));
                if options.debug
                    && let Some(value) = &verdict
                {
                    eprintln!("zor: verdict {:?} rule={:?}", value.state, value.rule);
                }
                let events = machine.observe(
                    verdict,
                    active_agent.clone(),
                    child_pid,
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
        }
        let now = std::time::Instant::now();
        if options.agent.is_none()
            && scheduler.due(now)
            && let Some(pid) = child_pid
        {
            let pgid = crate::platform::foreground_pgid(pid);
            let no_pgid_full = scheduler.pgid_presence(pgid.is_some(), now);
            let changed = pgid != last_pgid;
            last_pgid = pgid;
            let full = scheduler.completed(
                now,
                active_agent.is_some(),
                machine.next_deadline().is_some(),
                changed,
            );
            if full || no_pgid_full {
                let listed = pgid.map_or_else(
                    || crate::platform::Job {
                        leader: 0,
                        processes: Vec::new(),
                    },
                    |group| crate::platform::job(pid, group),
                );
                let detected = crate::rules::ident::identify(&listed, &options.rule_sets);
                let shell = listed.processes.iter().any(|process| process.pid == pid);
                if let Some(outcome) = loss_tracker.update(detected, shell) {
                    let (next, exited) = match outcome {
                        crate::platform::probe::Detection::AgentFound { id, .. } => {
                            (Some(id), false)
                        }
                        crate::platform::probe::Detection::Exited { agent } => (Some(agent), true),
                        crate::platform::probe::Detection::AgentLost => (None, false),
                    };
                    active_agent = next;
                    let events =
                        machine.observe(None, active_agent.clone(), child_pid, exited, now);
                    queue_events(
                        &events,
                        &options,
                        &screen,
                        &mut titles,
                        &mut sink,
                        &mut queued,
                    );
                }
            }
        }
        let timer_events = machine.tick(now);
        queue_events(
            &timer_events,
            &options,
            &screen,
            &mut titles,
            &mut sink,
            &mut queued,
        );
        if screen.ground() {
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
    let code = status
        .signal()
        .and_then(signal_number)
        .map_or(status.exit_code(), |signal| 128 + signal);
    Ok(u8::try_from(code).unwrap_or(u8::MAX))
}

fn signal_number(name: &str) -> Option<u32> {
    [
        ("Hangup", 1),
        ("Interrupt", 2),
        ("Quit", 3),
        ("Killed", 9),
        ("Terminated", 15),
        ("Stopped", 19),
    ]
    .into_iter()
    .find_map(|(label, number)| name.contains(label).then_some(number))
}

fn write_fixture(screen: &Screen, agent: Option<&crate::osc::AgentId>) {
    use crate::rules::view::ScreenView;
    let path = std::env::temp_dir().join(format!(
        "zor-fixture-{}-{}.txt",
        std::process::id(),
        timestamp()
    ));
    let progress = screen.progress().map_or_else(String::new, |value| {
        format!("{}:{}", value.state, value.percent)
    });
    let body = format!(
        "# agent: {}\n# title: {}\n# progress: {progress}\n# expect: idle\n# matched: none\n{}",
        agent.map_or("unknown", crate::osc::AgentId::as_str),
        screen.title(),
        screen.text()
    );
    match std::fs::write(&path, body) {
        Ok(()) => eprintln!("zor: fixture written to {}", path.display()),
        Err(error) => eprintln!("zor: failed to write fixture: {error}"),
    }
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
            if let Event::Heartbeat {
                state,
                agent,
                seq,
                visible,
            } = event
                && let Some(target) = sink
            {
                write_event_line(
                    target,
                    *state,
                    None,
                    agent.as_ref(),
                    *seq,
                    *visible,
                    false,
                    screen,
                );
            }
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
            write_event_line(
                target,
                *state,
                Some(*previous),
                agent.as_ref(),
                *seq,
                *visible,
                *exited,
                screen,
            );
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn write_event_line(
    target: &mut Sink,
    state: crate::osc::State,
    previous: Option<crate::osc::State>,
    agent: Option<&crate::osc::AgentId>,
    seq: u64,
    visible: crate::osc::Flags,
    exited: bool,
    screen: &Screen,
) {
    use crate::rules::view::ScreenView;
    let time = timestamp();
    let visible_names = [
        (visible.idle, "idle"),
        (visible.blocker, "blocker"),
        (visible.working, "working"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    let line = EventLine {
        t: &time,
        state: state_name(state),
        previous: previous.map(state_name),
        agent: agent.map(crate::osc::AgentId::as_str),
        seq,
        pid: None,
        code: None,
        title: Some(screen.title()),
        visible: visible_names,
        exited,
    };
    if let Ok(bytes) = encode(&line) {
        target.write(&bytes);
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
