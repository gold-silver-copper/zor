use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::{
    emit::{
        events::{AgentLine, EventLine, ExitLine, Sink, encode, timestamp},
        title::{Mode as TitleMode, Titles},
    },
    osc::Report,
    rules::{RuleSet, evaluate, view::ScreenView},
    screen::Screen,
    state::{Config, Event, Machine},
};

struct ChildCleanup {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    armed: bool,
}

struct ThreadCleanup {
    cancel: Arc<AtomicBool>,
    signals: signal_hook::iterator::Handle,
    reader: Option<thread::JoinHandle<()>>,
    writer: Option<thread::JoinHandle<()>>,
    signal: Option<thread::JoinHandle<()>>,
}

impl Drop for ThreadCleanup {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.signals.close();
        if let Some(thread) = self.reader.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.writer.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.signal.take() {
            let _ = thread.join();
        }
    }
}

impl ChildCleanup {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self { child, armed: true }
    }
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

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
    let mut spawned_child = pair
        .slave
        .spawn_command(builder)
        .context("spawn wrapped command")?;
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = spawned_child.kill();
            let _ = spawned_child.wait();
            return Err(error).context("clone pty reader");
        }
    };
    let mut writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = spawned_child.kill();
            let _ = spawned_child.wait();
            return Err(error).context("take pty writer");
        }
    };
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
    let cancel = Arc::new(AtomicBool::new(false));
    let writer_cancel = Arc::clone(&cancel);
    let writer_thread = thread::spawn(move || {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let mut bytes = [0_u8; 1024];
        while !writer_cancel.load(Ordering::Relaxed) {
            let mut descriptors = [nix::poll::PollFd::new(
                std::os::fd::AsFd::as_fd(&stdin),
                nix::poll::PollFlags::POLLIN,
            )];
            match nix::poll::poll(&mut descriptors, 100_u16) {
                Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                Ok(_) => match stdin.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if writer
                            .write_all(bytes.get(..count).unwrap_or_default())
                            .is_err()
                        {
                            break;
                        }
                    }
                },
                Err(_) => break,
            }
        }
        // Closing a PTY master writer synthesizes VEOF, which can be echoed as child output.
        // Retain it until the child has exited and the coordinator requests teardown.
        while !writer_cancel.load(Ordering::Relaxed) {
            thread::park_timeout(std::time::Duration::from_millis(10));
        }
    });

    let signal_tx = output_tx;
    let signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGTSTP,
        signal_hook::consts::SIGCONT,
        signal_hook::consts::SIGUSR1,
    ]);
    let mut signals = match signals {
        Ok(signals) => signals,
        Err(error) => {
            cancel.store(true, Ordering::Relaxed);
            let _ = spawned_child.kill();
            let _ = spawned_child.wait();
            let _ = reader_thread.join();
            let _ = writer_thread.join();
            return Err(error).context("register signal handlers");
        }
    };
    let signal_handle = signals.handle();
    let signal_thread = thread::spawn(move || {
        for signal in signals.forever() {
            if signal_tx.send(Message::Signal(signal)).is_err() {
                break;
            }
        }
    });
    let _threads = ThreadCleanup {
        cancel: Arc::clone(&cancel),
        signals: signal_handle,
        reader: Some(reader_thread),
        writer: Some(writer_thread),
        signal: Some(signal_thread),
    };
    // Declared after the thread guard so failure unwinding kills the child before joining readers.
    let mut child = ChildCleanup::new(spawned_child);

    // The main loop is the sole output owner: child bytes are flushed before parsing.
    let mut screen = Screen::new(initial_size.rows, initial_size.cols);
    let mut machine = Machine::new(Config::default());
    let mut titles = Titles::new(options.title);
    let mut sink = options.events.clone().map(Sink::connect);
    let mut queued = Vec::<Vec<u8>>::new();
    let mut stdout = io::stdout().lock();
    let child_pid = child
        .child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok());
    let child_pgid = pair
        .master
        .process_group_leader()
        .filter(|pgid| *pgid > 0)
        .or(child_pid);
    let command_name = std::path::Path::new(command)
        .file_name()
        .and_then(|value| value.to_str());
    let mut active_agent = options.agent.clone().or_else(|| {
        command_name.and_then(|name| {
            options
                .rule_sets
                .iter()
                .find(|set| set.id == name || set.process_names.iter().any(|value| value == name))
                .and_then(|set| crate::osc::AgentId::new(set.id.clone()).ok())
        })
    });
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
                    restore_on_error(
                        pair.master.resize(size).context("resize pty"),
                        &titles,
                        &mut stdout,
                    )?;
                    screen.resize(size.rows, size.cols);
                } else if signal == signal_hook::consts::SIGUSR1 {
                    write_fixture(&screen, options.agent.as_ref());
                } else if signal == signal_hook::consts::SIGTSTP {
                    if let Some(pgid) = signal_target(last_pgid, child_pgid) {
                        let _ = crate::platform::forward_signal(pgid, signal);
                    }
                    drop(raw_guard.take());
                    crate::platform::suspend_self();
                    raw_guard = crate::platform::set_raw(0).ok();
                } else if signal == signal_hook::consts::SIGCONT {
                    if raw_guard.is_none() {
                        raw_guard = crate::platform::set_raw(0).ok();
                    }
                    if let Some(pgid) = signal_target(last_pgid, child_pgid) {
                        let _ = crate::platform::forward_signal(pgid, signal);
                    }
                } else if let Some(pgid) = signal_target(last_pgid, child_pgid) {
                    let _ = crate::platform::forward_signal(pgid, signal);
                }
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let had_chunk = chunk.is_some();
        if let Some(chunk) = chunk {
            restore_on_error(
                stdout.write_all(&chunk).context("write child output"),
                &titles,
                &mut stdout,
            )?;
            restore_on_error(
                stdout.flush().context("flush child output"),
                &titles,
                &mut stdout,
            )?;
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
                let evaluated = active_agent
                    .as_ref()
                    .and_then(|id| options.rule_sets.iter().find(|set| set.id == id.as_str()))
                    .map(|set| evaluate(set, &screen));
                if options.debug
                    && let Some(value) = &evaluated
                {
                    eprintln!("zor: verdict {:?} rule={:?}", value.state, value.rule);
                }
                let verdict = evaluated.as_ref().map(observation);
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
        if !had_chunk
            && machine.hold_pending()
            && machine
                .next_deadline()
                .is_some_and(|deadline| now >= deadline)
        {
            let verdict = active_agent
                .as_ref()
                .and_then(|id| options.rule_sets.iter().find(|set| set.id == id.as_str()))
                .map(|set| observation(&evaluate(set, &screen)));
            let events = machine.observe(verdict, active_agent.clone(), child_pid, false, now);
            queue_events(
                &events,
                &options,
                &screen,
                &mut titles,
                &mut sink,
                &mut queued,
            );
        }
        if options.agent.is_none()
            && scheduler.due(now)
            && let Some(pid) = child_pid
        {
            let pgid = crate::platform::foreground_pgid(pid, pair.master.as_raw_fd());
            let no_pgid_full = scheduler.pgid_presence(pgid.is_some(), now);
            let changed = pgid != last_pgid;
            last_pgid = pgid;
            let full =
                scheduler.completed(now, active_agent.is_some(), machine.hold_pending(), changed);
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
                    let (next, event_pid, exited, found) = match outcome {
                        crate::platform::probe::Detection::AgentFound { id, pid } => {
                            (Some(id), Some(pid), false, true)
                        }
                        crate::platform::probe::Detection::Exited { agent } => {
                            (Some(agent), None, true, false)
                        }
                        crate::platform::probe::Detection::AgentLost => (None, None, false, false),
                    };
                    active_agent = next;
                    if found {
                        screen.clear_detection_evidence();
                        last_title.clear();
                    }
                    let verdict = found
                        .then(|| {
                            active_agent
                                .as_ref()
                                .and_then(|id| {
                                    options.rule_sets.iter().find(|set| set.id == id.as_str())
                                })
                                .map(|set| observation(&evaluate(set, &screen)))
                        })
                        .flatten();
                    let events =
                        machine.observe(verdict, active_agent.clone(), event_pid, exited, now);
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
                restore_on_error(
                    stdout.write_all(&bytes).context("write zor output"),
                    &titles,
                    &mut stdout,
                )?;
                restore_on_error(
                    stdout.flush().context("flush zor output"),
                    &titles,
                    &mut stdout,
                )?;
            }
        }
    }
    let status = restore_on_error(
        child.child.wait().context("wait for wrapped command"),
        &titles,
        &mut stdout,
    )?;
    child.armed = false;
    if let Some(bytes) = titles.restore() {
        stdout.write_all(&bytes)?;
        stdout.flush()?;
    }
    let code = status
        .signal()
        .and_then(signal_number)
        .map_or(status.exit_code(), |signal| 128 + signal);
    let code = i32::try_from(code).unwrap_or(i32::MAX);
    if let Some(target) = &mut sink {
        let time = timestamp();
        if let Ok(bytes) = encode(&ExitLine {
            t: "exit",
            code,
            ts: time,
        }) {
            target.write(&bytes);
        }
    }
    if options.debug
        && let Some(value) = sink
    {
        eprintln!("zor: dropped event lines: {}", value.dropped);
    }
    Ok(u8::try_from(code).unwrap_or(u8::MAX))
}

fn signal_target(foreground_pgid: Option<i32>, child_pgid: Option<i32>) -> Option<i32> {
    foreground_pgid
        .filter(|pgid| *pgid > 0)
        .or_else(|| child_pgid.filter(|pgid| *pgid > 0))
}

fn restore_on_error<T>(result: Result<T>, titles: &Titles, stdout: &mut impl Write) -> Result<T> {
    if result.is_err()
        && let Some(bytes) = titles.restore()
    {
        let _ = stdout.write_all(&bytes);
        let _ = stdout.flush();
    }
    result
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
        if let Event::AgentFound { id, pid } = event {
            write_agent_event(sink, Some(id), Some(*pid));
            continue;
        }
        if matches!(event, Event::AgentLost) {
            write_agent_event(sink, None, None);
            continue;
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
        t: "state",
        ts: time,
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

fn write_agent_event(
    sink: &mut Option<Sink>,
    agent: Option<&crate::osc::AgentId>,
    pid: Option<i32>,
) {
    let Some(target) = sink else { return };
    let time = timestamp();
    let line = AgentLine {
        t: "agent",
        agent: agent.map(crate::osc::AgentId::as_str),
        pid,
        ts: time,
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

fn observation(verdict: &crate::rules::Verdict) -> crate::state::Observation {
    use crate::{rules::RuleState, state::ObservationState};
    crate::state::Observation {
        state: match verdict.state {
            RuleState::Working => ObservationState::Working,
            RuleState::Blocked => ObservationState::Blocked,
            RuleState::Idle => ObservationState::Idle,
            RuleState::Skip => ObservationState::Skip,
        },
        visible: verdict.visible,
    }
}

pub fn run_transparent(command: &str, argv: &[String]) -> Result<u8> {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::Command::new(command)
        .args(argv)
        .status()
        .context("run nested wrapped command")?;
    let code = status
        .code()
        .unwrap_or_else(|| 128_i32.saturating_add(status.signal().unwrap_or_default()));
    Ok(u8::try_from(code).unwrap_or(u8::MAX))
}

#[cfg(test)]
mod signal_tests {
    use super::signal_target;

    #[test]
    fn foreground_process_group_precedes_validated_child_fallback() {
        // Phase Z §4-5: forwarded signals follow the active foreground job when known.
        assert_eq!(signal_target(Some(42), Some(7)), Some(42));
        assert_eq!(signal_target(None, Some(7)), Some(7));
        assert_eq!(signal_target(Some(0), Some(-1)), None);
    }
}
