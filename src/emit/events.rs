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
const MAX_PENDING_RECORD: usize = 2_048;

struct Pending {
    bytes: Vec<u8>,
    offset: usize,
}
pub struct Sink {
    path: PathBuf,
    target: Option<Target>,
    retry_at: Instant,
    pending: Option<Pending>,
    pub dropped: u64,
}
impl Sink {
    pub fn connect(path: impl Into<PathBuf>) -> Self {
        let mut value = Self {
            path: path.into(),
            target: None,
            retry_at: Instant::now(),
            pending: None,
            dropped: 0,
        };
        value.reconnect();
        value
    }
    pub fn write(&mut self, line: &[u8]) {
        self.flush_pending();
        if self.pending.is_some() {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        if line.len() > MAX_PENDING_RECORD {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.pending = Some(Pending {
            bytes: line.to_vec(),
            offset: 0,
        });
        self.flush_pending();
    }
    fn flush_pending(&mut self) {
        if self.pending.is_none() {
            return;
        }
        if self.target.is_none() && Instant::now() >= self.retry_at {
            self.reconnect();
        }
        let Some(target) = self.target.as_mut() else {
            return;
        };
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let remaining = pending.bytes.get(pending.offset..).unwrap_or_default();
        let result = match target {
            Target::Socket(value) => value.write(remaining),
            Target::File(value) => value.write(remaining),
            #[cfg(test)]
            Target::Test(value) => value.write(remaining),
        };
        match result {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Ok(written) => {
                pending.offset = pending
                    .offset
                    .saturating_add(written)
                    .min(pending.bytes.len());
                if pending.offset == pending.bytes.len() {
                    self.pending = None;
                }
            }
            Err(_) => {
                // A new connection cannot continue a record whose prefix went to the old one.
                // Restart the bounded record after the reconnect boundary.
                pending.offset = 0;
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
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Read;
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
            pending: None,
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
    fn partial_write_finishes_before_accepting_another_json_line() {
        // Phase Z §6: short writes retain one bounded record and drop newer backpressure traffic.
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = Sink {
            path: PathBuf::new(),
            target: Some(Target::Test(Box::new(PartialWriter {
                bytes: Arc::clone(&bytes),
            }))),
            retry_at: Instant::now(),
            pending: None,
            dropped: 0,
        };
        sink.write(b"{\"one\":1}\n");
        sink.write(b"{\"two\":2}\n");
        assert_eq!(sink.dropped, 1);
        while sink.pending.is_some() {
            sink.flush_pending();
        }
        sink.write(b"{\"three\":3}\n");
        while sink.pending.is_some() {
            sink.flush_pending();
        }
        assert_eq!(
            bytes.lock().map(|value| value.clone()).unwrap_or_default(),
            b"{\"one\":1}\n{\"three\":3}\n"
        );
    }

    #[test]
    #[allow(clippy::panic)]
    fn nonblocking_file_target_never_concatenates_after_pressure() {
        // Phase Z §6: fd3-style shared descriptors preserve JSONL boundaries under pressure.
        let (mut writer, mut reader) =
            UnixStream::pair().unwrap_or_else(|error| panic!("socket pair: {error}"));
        writer
            .set_nonblocking(true)
            .unwrap_or_else(|error| panic!("nonblocking: {error}"));
        let filler = [b'x'; 8_192];
        let mut filled = 0usize;
        loop {
            match writer.write(&filler) {
                Ok(count) => filled = filled.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill pressure: {error}"),
            }
        }
        let owned: std::os::fd::OwnedFd = writer.into();
        let mut sink = Sink {
            path: PathBuf::new(),
            target: Some(Target::File(File::from(owned))),
            retry_at: Instant::now(),
            pending: None,
            dropped: 0,
        };
        sink.write(b"{\"seq\":1}\n");
        sink.write(b"{\"seq\":2}\n");
        assert_eq!(sink.dropped, 1);
        let mut discarded = vec![0; filled];
        reader
            .read_exact(&mut discarded)
            .unwrap_or_else(|error| panic!("drain filler: {error}"));
        sink.flush_pending();
        sink.write(b"{\"seq\":3}\n");
        reader
            .set_nonblocking(true)
            .unwrap_or_else(|error| panic!("reader nonblocking: {error}"));
        let mut records = Vec::new();
        loop {
            let mut bytes = [0_u8; 128];
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => records.extend_from_slice(bytes.get(..count).unwrap_or_default()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("read records: {error}"),
            }
        }
        let decoded: Vec<serde_json::Value> = records
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|error| panic!("decode records: {error}; bytes={records:?}"));
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            decoded.first().and_then(|value| value.get("seq")),
            Some(&1.into())
        );
        assert_eq!(
            decoded.get(1).and_then(|value| value.get("seq")),
            Some(&3.into())
        );
    }
}
