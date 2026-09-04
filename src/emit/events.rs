use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
pub struct EventLine<'a> {
    pub t: &'a str,
    pub state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<&'a str>,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub visible: Vec<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    pub exited: bool,
}
fn is_false(value: &bool) -> bool {
    !value
}

pub fn encode(event: &EventLine<'_>) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    Ok(line)
}

#[must_use]
pub fn timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", duration.as_secs(), duration.subsec_millis())
}

enum Target {
    Socket(UnixStream),
    File(File),
}
pub struct Sink {
    path: PathBuf,
    target: Option<Target>,
    retry_at: Instant,
    pub dropped: u64,
}
impl Sink {
    pub fn connect(path: impl Into<PathBuf>) -> Self {
        let mut value = Self {
            path: path.into(),
            target: None,
            retry_at: Instant::now(),
            dropped: 0,
        };
        value.reconnect();
        value
    }
    pub fn write(&mut self, line: &[u8]) {
        if self.target.is_none() && Instant::now() >= self.retry_at {
            self.reconnect();
        }
        let result = match self.target.as_mut() {
            Some(Target::Socket(value)) => value.write_all(line),
            Some(Target::File(value)) => value.write_all(line),
            None => return,
        };
        if let Err(error) = result {
            self.dropped = self.dropped.saturating_add(1);
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
            ) {
                self.target = None;
                self.retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
    }
    fn reconnect(&mut self) {
        self.target = open_target(&self.path).ok();
        if self.target.is_none() {
            self.retry_at = Instant::now() + Duration::from_secs(1);
        }
    }
}
fn open_target(path: &Path) -> io::Result<Target> {
    if path == Path::new("-") {
        return OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/fd/3")
            .map(Target::File);
    }
    if let Ok(socket) = UnixStream::connect(path) {
        socket.set_nonblocking(true)?;
        return Ok(Target::Socket(socket));
    }
    let metadata = std::fs::metadata(path)?;
    if !metadata.file_type().is_fifo() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "event path is not a unix socket or fifo",
        ));
    }
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map(Target::File)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_is_one_json_line() {
        // Phase Z §6: event output is parseable JSON Lines with optional fields omitted.
        let line = EventLine {
            t: "1.000",
            state: "idle",
            previous: None,
            agent: Some("a"),
            seq: 2,
            pid: None,
            code: None,
            title: None,
            visible: vec![],
            exited: false,
        };
        let encoded = encode(&line).unwrap_or_default();
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<serde_json::Value>(&encoded).is_ok());
    }
}
