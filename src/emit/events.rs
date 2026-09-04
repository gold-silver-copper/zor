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
    pub ts: f64,
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

#[derive(Serialize)]
pub struct AgentLine<'a> {
    pub t: &'static str,
    pub agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    pub ts: f64,
}

#[derive(Serialize)]
pub struct ExitLine {
    pub t: &'static str,
    pub code: i32,
    pub ts: f64,
}
fn is_false(value: &bool) -> bool {
    !value
}

pub fn encode(event: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    Ok(line)
}

#[must_use]
pub fn timestamp() -> f64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_millis() as f64 / 1_000.0
}

enum Target {
    Socket(UnixStream),
    File(File),
    #[cfg(test)]
    Test(Box<dyn Write + Send>),
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
            Some(Target::Socket(value)) => value.write(line),
            Some(Target::File(value)) => value.write(line),
            #[cfg(test)]
            Some(Target::Test(value)) => value.write(line),
            None => return,
        };
        if !matches!(result, Ok(written) if written == line.len()) {
            self.dropped = self.dropped.saturating_add(1);
            // A partial JSON line cannot safely share a stream with a later record. Closing the
            // target gives socket readers an EOF boundary at which to discard the fragment.
            self.target = None;
            self.retry_at = Instant::now() + Duration::from_secs(1);
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
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct PartialWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for PartialWriter {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let count = input.len().min(3);
            if let Ok(mut bytes) = self.bytes.lock() {
                bytes.extend_from_slice(input.get(..count).unwrap_or_default());
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn event_is_one_json_line() {
        // Phase Z §6: event output is parseable JSON Lines with optional fields omitted.
        let line = EventLine {
            t: "state",
            ts: 1.0,
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
        let value = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap_or_default();
        assert_eq!(value["t"], "state");
        assert_eq!(value["ts"], 1.0);
        assert_eq!(value["state"], "idle");
        assert!(value.get("pid").is_none());
    }

    #[test]
    fn agent_and_exit_lines_use_the_tagged_contract() {
        // Phase Z §6: lifecycle lines carry their type, timestamp, and relevant payload.
        let agent = encode(&AgentLine {
            t: "agent",
            agent: Some("claude"),
            pid: Some(42),
            ts: 2.0,
        })
        .unwrap_or_default();
        let exit = encode(&ExitLine {
            t: "exit",
            code: 143,
            ts: 3.0,
        })
        .unwrap_or_default();
        let agent: serde_json::Value = serde_json::from_slice(&agent).unwrap_or_default();
        let exit: serde_json::Value = serde_json::from_slice(&exit).unwrap_or_default();
        assert_eq!(agent["t"], "agent");
        assert_eq!(agent["pid"], 42);
        assert_eq!(agent["ts"], 2.0);
        assert_eq!(exit["t"], "exit");
        assert_eq!(exit["code"], 143);
        assert_eq!(exit["ts"], 3.0);
    }

    #[test]
    #[allow(clippy::panic)]
    fn full_socket_drops_instead_of_blocking() {
        // Phase Z §6: a nonblocking full sink increments the drop counter.
        let (writer, _reader) =
            UnixStream::pair().unwrap_or_else(|error| panic!("socket pair: {error}"));
        writer
            .set_nonblocking(true)
            .unwrap_or_else(|error| panic!("nonblocking: {error}"));
        let mut sink = Sink {
            path: PathBuf::new(),
            target: Some(Target::Socket(writer)),
            retry_at: Instant::now(),
            dropped: 0,
        };
        let line = vec![b'x'; 65_536];
        for _ in 0..1024 {
            sink.write(&line);
            if sink.dropped > 0 {
                break;
            }
        }
        assert!(sink.dropped > 0);
    }

    #[test]
    fn partial_write_closes_the_target_before_another_json_line() {
        // Phase Z §6: a short nonblocking write cannot prefix-corrupt a later JSON record.
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = Sink {
            path: PathBuf::new(),
            target: Some(Target::Test(Box::new(PartialWriter {
                bytes: Arc::clone(&bytes),
            }))),
            retry_at: Instant::now(),
            dropped: 0,
        };
        sink.write(b"{\"one\":1}\n");
        sink.write(b"{\"two\":2}\n");
        assert_eq!(sink.dropped, 1);
        assert!(sink.target.is_none());
        assert_eq!(
            bytes.lock().map(|value| value.clone()).unwrap_or_default(),
            b"{\"o"
        );
    }
}
