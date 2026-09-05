//! Optional observation of a multiplexer-owned pane through its local control interface.
//! This process never spawns, signals, or owns the observed command.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_REPLY: usize = 1024 * 1024;

fn request(socket: &Path, value: Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    #[cfg(target_os = "macos")]
    let peer = nix::unistd::getpeereid(&stream)?.0.as_raw();
    #[cfg(target_os = "linux")]
    let peer =
        nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)?.uid();
    anyhow::ensure!(
        peer == nix::unistd::geteuid().as_raw(),
        "control socket belongs to another user"
    );
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"FUXCTL1\n")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut preface = [0; 8];
    let mut used = 0;
    while used < preface.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("control version negotiation timed out"))?;
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(
            preface
                .get_mut(used..)
                .ok_or_else(|| anyhow::anyhow!("invalid preface offset"))?,
        )?;
        anyhow::ensure!(n != 0, "control server closed during negotiation");
        used += n;
    }
    anyhow::ensure!(
        &preface == b"FUXCTL1\n",
        "incompatible fux control protocol"
    );
    serde_json::to_writer(&mut stream, &value)?;
    stream.write_all(b"\n")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("control response timed out"))?;
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0; 8192];
        let n = stream.read(&mut chunk)?;
        anyhow::ensure!(n != 0, "control socket closed before a response");
        anyhow::ensure!(
            output.len() + n <= MAX_REPLY,
            "control response exceeds limit"
        );
        output.extend_from_slice(
            chunk
                .get(..n)
                .ok_or_else(|| anyhow::anyhow!("invalid socket read length"))?,
        );
        if output.contains(&b'\n') {
            break;
        }
    }
    let response: Value = serde_json::from_slice(&output)?;
    anyhow::ensure!(
        response.get("status").and_then(Value::as_str) == Some("completed"),
        "control request failed: {response}"
    );
    Ok(response)
}

pub fn run(
    socket: &Path,
    pane_id: u32,
    expected_pid: u32,
    forced: Option<&str>,
    sets: &[crate::rules::RuleSet],
) -> anyhow::Result<u8> {
    use crate::state::{Event, Machine, Observation, ObservationState};
    let mut machine = Machine::new(crate::state::Config::default());
    let forced = forced.map(crate::osc::AgentId::new).transpose()?;
    let pid = i32::try_from(expected_pid)?;
    let mut output = std::io::stdout().lock();
    let mut loss = crate::platform::probe::LossTracker::new();
    let mut active_agent = forced.clone();
    // The observer may start immediately before its parent's control accept loop.
    let startup = Instant::now() + Duration::from_secs(3);
    loop {
        let listing = match request(socket, json!({"command":"list","id":1})) {
            Ok(value) => value,
            Err(_) if Instant::now() < startup => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(error),
        };
        let panes = listing
            .pointer("/result/value/workspaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|workspace| {
                workspace
                    .get("tabs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .flat_map(|tab| {
                tab.get("panes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            });
        let Some(pane) = panes
            .into_iter()
            .find(|pane| pane.get("id").and_then(Value::as_u64) == Some(u64::from(pane_id)))
        else {
            return Ok(0);
        };
        if pane.get("pid").and_then(Value::as_u64) != Some(u64::from(expected_pid)) {
            return Ok(0);
        }
        let rows = pane
            .pointer("/geometry/height")
            .and_then(Value::as_u64)
            .unwrap_or(26)
            .saturating_sub(2)
            .clamp(2, 1000) as u16;
        let columns = pane
            .pointer("/geometry/width")
            .and_then(Value::as_u64)
            .unwrap_or(82)
            .saturating_sub(2)
            .clamp(2, 1000) as u16;
        let capture = request(
            socket,
            json!({"command":"capture","id":2,"pane":pane_id,"attrs":true,"scrollback":0,"max_bytes":131072}),
        )?;
        let text = capture
            .pointer("/result/value/text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("invalid capture response"))?;
        let mut screen = crate::screen::Screen::new(rows, columns);
        screen.process(text.as_bytes());
        screen.set_observed_title(
            pane.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let progress = pane
            .get("progress")
            .and_then(Value::as_array)
            .and_then(|values| {
                let state = u8::try_from(values.first()?.as_u64()?).ok()?;
                let percent = u8::try_from(values.get(1)?.as_u64()?).ok()?;
                (state <= 4 && percent <= 100)
                    .then_some(crate::rules::view::Progress { state, percent })
            });
        screen.set_observed_progress(progress);
        let job =
            crate::platform::foreground_pgid(pid, None).map(|pgid| crate::platform::job(pid, pgid));
        let detected = job
            .as_ref()
            .and_then(|job| crate::rules::ident::identify(job, sets));
        let mut exited = false;
        if forced.is_none() {
            let shell = job
                .as_ref()
                .is_some_and(|job| job.processes.iter().any(|process| process.pid == pid));
            if let Some(change) = loss.update(detected.clone(), shell) {
                match change {
                    crate::platform::probe::Detection::AgentFound { id, .. } => {
                        active_agent = Some(id)
                    }
                    crate::platform::probe::Detection::Exited { agent } => {
                        active_agent = Some(agent);
                        exited = true;
                    }
                    crate::platform::probe::Detection::AgentLost => active_agent = None,
                }
            }
        }
        let agent = active_agent.clone();
        let verdict = agent
            .as_ref()
            .and_then(|agent| sets.iter().find(|set| set.id == agent.as_str()))
            .map(|set| crate::rules::evaluate(set, &screen))
            .map(|verdict| Observation {
                state: match verdict.state {
                    crate::rules::RuleState::Working => ObservationState::Working,
                    crate::rules::RuleState::Blocked => ObservationState::Blocked,
                    crate::rules::RuleState::Idle => ObservationState::Idle,
                    crate::rules::RuleState::Skip => ObservationState::Skip,
                },
                visible: verdict.visible,
            });
        let now = Instant::now();
        let mut events = machine.observe(verdict, agent, detected.map(|(_, pid)| pid), exited, now);
        events.extend(machine.tick(now));
        for event in events {
            let report = match event {
                Event::Changed {
                    state,
                    agent,
                    seq,
                    visible,
                    exited,
                    ..
                } => Some(crate::osc::Report::new(
                    state, agent, seq, visible, exited, None,
                )?),
                Event::Heartbeat {
                    state,
                    agent,
                    seq,
                    visible,
                } => Some(crate::osc::Report::new(
                    state, agent, seq, visible, false, None,
                )?),
                _ => None,
            };
            if let Some(report) = report {
                output.write_all(&crate::osc::format(&report))?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
