use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

const OUTPUT_QUEUE_DEPTH: usize = 8;
const MAX_QUEUED_INJECTIONS: usize = 128;
const MAX_QUEUED_INJECTION_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PTY_ROWS: u16 = 512;
pub(crate) const MAX_PTY_COLS: u16 = 512;

fn clamp_size(mut size: portable_pty::PtySize) -> portable_pty::PtySize {
    size.rows = size.rows.clamp(1, MAX_PTY_ROWS);
    size.cols = size.cols.clamp(1, MAX_PTY_COLS);
    size
}

fn queue_injection(queued: &mut Vec<Vec<u8>>, bytes: Vec<u8>) {
    let current = queued.iter().map(Vec::len).sum::<usize>();
    if queued.len() < MAX_QUEUED_INJECTIONS
        && current.saturating_add(bytes.len()) <= MAX_QUEUED_INJECTION_BYTES
    {
        queued.push(bytes);
    }
}

fn take_pending_signal(pending: &AtomicU64) -> Option<i32> {
    // Terminal lifecycle signals take precedence over repaint/capture traffic. Clear exactly one
    // bit with an atomic RMW so concurrently arriving signals cannot be lost.
    for signal in [
        signal_hook::consts::SIGTSTP,
        signal_hook::consts::SIGCONT,
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGUSR1,
    ] {
        let bit = 1_u64 << u32::try_from(signal).unwrap_or_default();
        let previous = pending.fetch_and(!bit, Ordering::AcqRel);
        if previous & bit != 0 {
            return Some(signal);
        }
    }
    None
}

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
    signal_forwarding: Arc<Mutex<bool>>,
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
    fn new(
        child: Box<dyn portable_pty::Child + Send + Sync>,
        signal_forwarding: Arc<Mutex<bool>>,
    ) -> Self {
        Self {
            child,
            armed: true,
            signal_forwarding,
        }
    }
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        *self
            .signal_forwarding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
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
    let initial_size = clamp_size(crate::platform::winsize(0));
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
    let cancel = Arc::new(AtomicBool::new(false));
    let (output_tx, output_rx) = mpsc::sync_channel::<Message>(OUTPUT_QUEUE_DEPTH);
    let reader_tx = output_tx.clone();
    let reader_cancel = Arc::clone(&cancel);
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let Some(chunk) = buffer.get(..count) else {
                        break;
                    };
                    let mut message = Message::Chunk(chunk.to_vec());
                    loop {
                        match reader_tx.try_send(message) {
                            Ok(()) => break,
                            Err(mpsc::TrySendError::Full(returned))
                                if !reader_cancel.load(Ordering::Relaxed) =>
                            {
                                message = returned;
                                thread::park_timeout(std::time::Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    }
                }
            }
        }
        let mut eof = Message::Eof;
        loop {
            match reader_tx.try_send(eof) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned))
                    if !reader_cancel.load(Ordering::Relaxed) =>
                {
                    eof = returned;
                    thread::park_timeout(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
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

    let child_pid = spawned_child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok());
    let child_pgid = pair
        .master
        .process_group_leader()
        .filter(|pgid| *pgid > 0)
        .or(child_pid);
    // portable-pty starts the child as its process-group leader. Before the first successful
    // foreground probe, the child's pid is the only group we can safely claim is ours; the PTY
    // driver's cached foreground value can still describe the parent during startup.
    let foreground_pgid = Arc::new(AtomicI32::new(child_pid.or(child_pgid).unwrap_or_default()));
    let foreground_fd = pair.master.as_raw_fd();

    // Terminal lifecycle signals use a bounded coalescing bitset. Termination signals are
    // forwarded by this independent thread so a blocked stdout can never stall delivery.
    let pending_signals = Arc::new(AtomicU64::new(0));
    let signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
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
    let signal_pending = Arc::clone(&pending_signals);
    let signal_foreground = Arc::clone(&foreground_pgid);
    let signal_forwarding = Arc::new(Mutex::new(true));
    let thread_signal_forwarding = Arc::clone(&signal_forwarding);
    let signal_thread = thread::spawn(move || {
        for signal in signals.forever() {
            if matches!(
                signal,
                signal_hook::consts::SIGINT
                    | signal_hook::consts::SIGTERM
                    | signal_hook::consts::SIGHUP
                    | signal_hook::consts::SIGQUIT
            ) {
                let forwarding = thread_signal_forwarding
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !*forwarding {
                    continue;
                }
                let cached = signal_foreground.load(Ordering::Acquire);
                let current = child_pid
                    .and_then(|pid| crate::platform::foreground_pgid(pid, foreground_fd))
                    .or((cached > 0).then_some(cached));
                if let Some(current) = current {
                    signal_foreground.store(current, Ordering::Release);
                }
                let _ = forward_termination_with_escalation(
                    current,
                    child_pid.or(child_pgid),
                    signal,
                    || thread::park_timeout(std::time::Duration::from_millis(100)),
                    crate::platform::forward_signal,
                );
                continue;
            }
            if let Ok(index) = u32::try_from(signal)
                && index < u64::BITS
            {
                signal_pending.fetch_or(1_u64 << index, Ordering::Release);
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
    let mut child = ChildCleanup::new(spawned_child, Arc::clone(&signal_forwarding));

    // The main loop is the sole output owner: child bytes are flushed before parsing.
    let mut screen = Screen::new(initial_size.rows, initial_size.cols);
    let mut machine = Machine::new(Config::default());
    let mut titles = Titles::new(options.title);
    let mut sink = options.events.clone().map(Sink::connect);
    let mut queued = Vec::<Vec<u8>>::new();
    let mut stdout = io::stdout().lock();
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
    let mut current_size = initial_size;
    loop {
        // Some PTY hosts update the wrapper's controlling terminal without delivering SIGWINCH
        // to its process group. Polling alongside the already bounded 50 ms event wait keeps the
        // child PTY authoritative on those hosts while the signal path remains the fast path.
        let observed_size = clamp_size(crate::platform::winsize(0));
        if observed_size.rows != current_size.rows || observed_size.cols != current_size.cols {
            restore_on_error(
                pair.master.resize(observed_size).context("resize pty"),
                &titles,
                &mut stdout,
            )?;
            screen.resize(observed_size.rows, observed_size.cols);
            current_size = observed_size;
        }
        let message = take_pending_signal(&pending_signals).map_or_else(
            || output_rx.recv_timeout(std::time::Duration::from_millis(50)),
            |signal| Ok(Message::Signal(signal)),
        );
        let chunk = match message {
            Ok(Message::Chunk(chunk)) => Some(chunk),
            Ok(Message::Eof) => break,
            Ok(Message::Signal(signal)) => {
                if signal == signal_hook::consts::SIGWINCH {
                    let size = clamp_size(crate::platform::winsize(0));
                    restore_on_error(
                        pair.master.resize(size).context("resize pty"),
                        &titles,
                        &mut stdout,
                    )?;
                    screen.resize(size.rows, size.cols);
                    current_size = size;
                } else if signal == signal_hook::consts::SIGUSR1 {
                    write_fixture(&screen, options.agent.as_ref());
                } else if signal == signal_hook::consts::SIGTSTP {
                    let _ = forward_signal_with_retry(
                        last_pgid,
                        pair.master.process_group_leader(),
                        child_pgid,
                        signal,
                        crate::platform::forward_signal,
                    );
                    drop(raw_guard.take());
                    crate::platform::suspend_self();
                    raw_guard = crate::platform::set_raw(0).ok();
                } else if signal == signal_hook::consts::SIGCONT {
                    if raw_guard.is_none() {
                        raw_guard = crate::platform::set_raw(0).ok();
                    }
                    let _ = forward_signal_with_retry(
                        last_pgid,
                        pair.master.process_group_leader(),
                        child_pgid,
                        signal,
                        crate::platform::forward_signal,
                    );
                } else {
                    let _ = forward_signal_with_retry(
                        last_pgid,
                        pair.master.process_group_leader(),
                        child_pgid,
                        signal,
                        crate::platform::forward_signal,
                    );
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
                    queue_injection(&mut queued, bytes);
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
            foreground_pgid.store(
                pgid.unwrap_or(child_pid.or(child_pgid).unwrap_or_default()),
                Ordering::Release,
            );
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
    *signal_forwarding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
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

fn forward_signal_with_retry(
    cached_pgid: Option<i32>,
    current_pgid: Option<i32>,
    child_pgid: Option<i32>,
    signal: i32,
    mut forward: impl FnMut(i32, i32) -> io::Result<()>,
) -> io::Result<()> {
    let mut last_error = None;
    let mut attempted = Vec::with_capacity(3);
    for pgid in [cached_pgid, current_pgid, child_pgid]
        .into_iter()
        .flatten()
        .filter(|pgid| *pgid > 0)
    {
        if attempted.contains(&pgid) {
            continue;
        }
        attempted.push(pgid);
        match forward(pgid, signal) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no live child process group")))
}

fn forward_termination_with_escalation(
    current_pgid: Option<i32>,
    child_pgid: Option<i32>,
    signal: i32,
    pause: impl FnOnce(),
    mut forward: impl FnMut(i32, i32) -> io::Result<()>,
) -> io::Result<()> {
    let initial = forward_signal_with_retry(current_pgid, None, child_pgid, signal, &mut forward);
    pause();
    let forced = forward_signal_with_retry(
        current_pgid,
        None,
        child_pgid,
        signal_hook::consts::SIGKILL,
        &mut forward,
    );
    match (initial, forced) {
        (Ok(()), _) | (_, Ok(())) => Ok(()),
        (Err(initial), Err(_)) => Err(initial),
    }
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
    match write_private_fixture(&path, body.as_bytes()) {
        Ok(()) => eprintln!("zor: fixture written to {}", path.display()),
        Err(error) => eprintln!("zor: failed to write fixture: {error}"),
    }
}

fn write_private_fixture(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)
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
                queue_injection(queued, crate::osc::format(&report));
            }
            if let Some(title) = titles.observe(
                screen.title(),
                *state,
                agent.as_ref().map(crate::osc::AgentId::as_str),
            ) {
                queue_injection(queued, title);
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
#[allow(clippy::expect_used)]
mod signal_tests {
    use super::{
        MAX_PTY_COLS, MAX_PTY_ROWS, clamp_size, forward_signal_with_retry,
        forward_termination_with_escalation, queue_injection, take_pending_signal,
        write_private_fixture,
    };

    #[test]
    fn failed_cached_group_retries_current_then_child_fallback() {
        // Phase Z §4-5: a raced cached pgid refreshes tcgetpgrp before using the child group.
        let mut attempts = Vec::new();
        let result = forward_signal_with_retry(Some(42), Some(43), Some(7), 15, |pgid, _| {
            attempts.push(pgid);
            (pgid == 7)
                .then_some(())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(result.is_ok());
        assert_eq!(attempts, [42, 43, 7]);

        attempts.clear();
        let result = forward_signal_with_retry(Some(42), Some(43), Some(7), 15, |pgid, _| {
            attempts.push(pgid);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(attempts, [42]);
    }

    #[test]
    fn wrapper_termination_escalates_the_owned_inner_group_before_reaping() {
        let mut attempts = Vec::new();
        let mut paused = false;
        forward_termination_with_escalation(
            Some(42),
            Some(7),
            signal_hook::consts::SIGHUP,
            || paused = true,
            |pgid, signal| {
                attempts.push((pgid, signal));
                Ok(())
            },
        )
        .expect("termination forwarding");
        assert!(paused);
        assert_eq!(
            attempts,
            [
                (42, signal_hook::consts::SIGHUP),
                (42, signal_hook::consts::SIGKILL)
            ]
        );
    }

    #[test]
    fn geometry_and_deferred_injections_are_bounded() {
        let size = clamp_size(portable_pty::PtySize {
            rows: u16::MAX,
            cols: u16::MAX,
            pixel_width: u16::MAX,
            pixel_height: u16::MAX,
        });
        assert_eq!((size.rows, size.cols), (MAX_PTY_ROWS, MAX_PTY_COLS));
        assert_eq!((size.pixel_width, size.pixel_height), (u16::MAX, u16::MAX));

        let mut queued = Vec::new();
        for _ in 0..10_000 {
            queue_injection(&mut queued, vec![b'x'; 1_024]);
        }
        assert!(queued.len() <= super::MAX_QUEUED_INJECTIONS);
        assert!(queued.iter().map(Vec::len).sum::<usize>() <= super::MAX_QUEUED_INJECTION_BYTES);
        let before = queued.len();
        queue_injection(
            &mut queued,
            vec![b'x'; super::MAX_QUEUED_INJECTION_BYTES + 1],
        );
        assert_eq!(queued.len(), before);
    }

    #[test]
    fn signals_are_independent_of_full_output_and_fixtures_are_private_create_new() {
        let (output_tx, _output_rx) = std::sync::mpsc::sync_channel(1);
        output_tx.send(1_u8).expect("fill output");
        let signals = std::sync::atomic::AtomicU64::new(0);
        signals.fetch_or(
            (1 << signal_hook::consts::SIGUSR1) | (1 << signal_hook::consts::SIGWINCH),
            std::sync::atomic::Ordering::Release,
        );
        assert_eq!(
            take_pending_signal(&signals),
            Some(signal_hook::consts::SIGWINCH)
        );
        assert_eq!(
            take_pending_signal(&signals),
            Some(signal_hook::consts::SIGUSR1)
        );
        assert_eq!(take_pending_signal(&signals), None);

        use std::os::unix::fs::PermissionsExt as _;
        let path =
            std::env::temp_dir().join(format!("zor-private-fixture-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_private_fixture(&path, b"secret").expect("create fixture");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(write_private_fixture(&path, b"overwrite").is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"secret");
        std::fs::remove_file(path).expect("cleanup");
    }
}
