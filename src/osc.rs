#![forbid(unsafe_code)]

use std::fmt;

const CODE: &[u8] = b"7877";
const MAX_AGENT_BYTES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_AGENT_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Working,
    Blocked,
    Idle,
    None,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flags {
    pub idle: bool,
    pub blocker: bool,
    pub working: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    state: State,
    agent: Option<AgentId>,
    seq: u64,
    visible: Flags,
    exited: bool,
    message: Option<String>,
}

impl Report {
    pub fn new(
        state: State,
        agent: Option<AgentId>,
        seq: u64,
        visible: Flags,
        exited: bool,
        message: Option<String>,
    ) -> Result<Self, Error> {
        if (state == State::None) != agent.is_none()
            || message
                .as_ref()
                .is_some_and(|value| value.len() > MAX_MESSAGE_BYTES)
        {
            return Err(Error);
        }
        Ok(Self {
            state,
            agent,
            seq,
            visible,
            exited,
            message,
        })
    }

    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }
    #[must_use]
    pub fn agent(&self) -> Option<&AgentId> {
        self.agent.as_ref()
    }
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }
    #[must_use]
    pub const fn visible(&self) -> Flags {
        self.visible
    }
    #[must_use]
    pub const fn exited(&self) -> bool {
        self.exited
    }
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid OSC 7877 report")
    }
}

impl std::error::Error for Error {}

#[must_use]
pub fn format(report: &Report) -> Vec<u8> {
    let mut output = format!("\u{1b}]7877;state={};", state_name(report.state));
    if let Some(agent) = &report.agent {
        output.push_str("agent=");
        output.push_str(agent.as_str());
        output.push(';');
    }
    output.push_str(&format!("seq={};visible=", report.seq));
    let mut separator = "";
    for (enabled, name) in [
        (report.visible.idle, "idle"),
        (report.visible.blocker, "blocker"),
        (report.visible.working, "working"),
    ] {
        if enabled {
            output.push_str(separator);
            output.push_str(name);
            separator = ",";
        }
    }
    output.push_str(";exited=");
    output.push(if report.exited { '1' } else { '0' });
    if let Some(message) = &report.message {
        output.push_str(";message=");
        percent_encode(message.as_bytes(), &mut output);
    }
    output.push_str("\u{1b}\\");
    output.into_bytes()
}

pub fn parse(input: &[u8]) -> Result<Report, Error> {
    let payload = strip_frame(input)?;
    let mut fields = payload.split(|byte| *byte == b';');
    if fields.next() != Some(CODE) {
        return Err(Error);
    }

    let mut state = None;
    let mut agent = None;
    let mut seq = None;
    let mut visible = None;
    let mut exited = None;
    let mut message = None;
    for field in fields {
        let Some(split) = field.iter().position(|byte| *byte == b'=') else {
            return Err(Error);
        };
        let Some(key) = field.get(..split) else {
            return Err(Error);
        };
        let Some(value) = field.get(split.saturating_add(1)..) else {
            return Err(Error);
        };
        match key {
            b"state" if state.is_none() => state = Some(parse_state(value)?),
            b"agent" if agent.is_none() => {
                let decoded = std::str::from_utf8(value).map_err(|_| Error)?;
                agent = Some(AgentId::new(decoded)?);
            }
            b"seq" if seq.is_none() => {
                let decoded = std::str::from_utf8(value).map_err(|_| Error)?;
                if decoded.is_empty() || !decoded.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(Error);
                }
                seq = Some(decoded.parse().map_err(|_| Error)?);
            }
            b"visible" if visible.is_none() => visible = Some(parse_flags(value)?),
            b"exited" if exited.is_none() => {
                exited = Some(match value {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(Error),
                })
            }
            b"message" if message.is_none() => message = Some(percent_decode(value)?),
            b"state" | b"agent" | b"seq" | b"visible" | b"exited" | b"message" => {
                return Err(Error);
            }
            _ => {}
        }
    }
    Report::new(
        state.ok_or(Error)?,
        agent,
        seq.ok_or(Error)?,
        visible.unwrap_or_default(),
        exited.unwrap_or(false),
        message,
    )
}

fn strip_frame(input: &[u8]) -> Result<&[u8], Error> {
    if let Some(rest) = input.strip_prefix(b"\x1b]") {
        if let Some(payload) = rest.strip_suffix(b"\x1b\\") {
            return Ok(payload);
        }
        if let Some(payload) = rest.strip_suffix(b"\x07") {
            return Ok(payload);
        }
        return Err(Error);
    }
    if input.contains(&0x07) || input.windows(2).any(|window| window == b"\x1b\\") {
        return Err(Error);
    }
    Ok(input)
}

fn parse_state(value: &[u8]) -> Result<State, Error> {
    match value {
        b"working" => Ok(State::Working),
        b"blocked" => Ok(State::Blocked),
        b"idle" => Ok(State::Idle),
        b"none" => Ok(State::None),
        _ => Err(Error),
    }
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Working => "working",
        State::Blocked => "blocked",
        State::Idle => "idle",
        State::None => "none",
    }
}

fn parse_flags(value: &[u8]) -> Result<Flags, Error> {
    let mut flags = Flags::default();
    if value.is_empty() {
        return Ok(flags);
    }
    for flag in value.split(|byte| *byte == b',') {
        let slot = match flag {
            b"idle" => &mut flags.idle,
            b"blocker" => &mut flags.blocker,
            b"working" => &mut flags.working,
            _ => return Err(Error),
        };
        if *slot {
            return Err(Error);
        }
        *slot = true;
    }
    Ok(flags)
}

fn percent_encode(input: &[u8], output: &mut String) {
    for byte in input {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b' ') {
            output.push(char::from(*byte));
        } else {
            output.push('%');
            output.push(hex_digit(byte >> 4));
            output.push(hex_digit(byte & 0x0f));
        }
    }
}

const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

fn percent_decode(input: &[u8]) -> Result<String, Error> {
    let mut output = Vec::with_capacity(input.len().min(MAX_MESSAGE_BYTES));
    let mut position = 0;
    while position < input.len() {
        let byte = *input.get(position).ok_or(Error)?;
        if byte == b'%' {
            let high = hex(*input.get(position.saturating_add(1)).ok_or(Error)?).ok_or(Error)?;
            let low = hex(*input.get(position.saturating_add(2)).ok_or(Error)?).ok_or(Error)?;
            output.push((high << 4) | low);
            position = position.saturating_add(3);
        } else {
            output.push(byte);
            position = position.saturating_add(1);
        }
        if output.len() > MAX_MESSAGE_BYTES {
            return Err(Error);
        }
    }
    String::from_utf8(output).map_err(|_| Error)
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
