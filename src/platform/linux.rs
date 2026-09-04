#![allow(unsafe_code)]
use super::{Job, Pid, Process};
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs, io,
    os::unix::ffi::OsStrExt,
};

pub fn foreground_pgid(child: Pid, master_fd: Option<i32>) -> Option<Pid> {
    stat(child)
        .map(|value| value.tpgid)
        .filter(|value| *value > 0)
        .or_else(|| unsafe {
            // SAFETY: tcgetpgrp only inspects the supplied live PTY descriptor.
            master_fd.and_then(|fd| {
                let pgid = libc::tcgetpgrp(fd);
                (pgid > 0).then_some(pgid)
            })
        })
}
pub fn leader(pgid: Pid) -> Option<Process> {
    process(pgid).filter(|_| stat(pgid).is_some_and(|value| value.pgrp == pgid))
}
pub fn job(child: Pid, pgid: Pid) -> Job {
    let mut queue = VecDeque::from([child, pgid]);
    let mut seen = HashSet::new();
    let mut processes = Vec::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if stat(pid).is_some_and(|value| value.pgrp == pgid)
            && let Some(value) = process(pid)
        {
            processes.push(value);
        }
        if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                if let Ok(children) = fs::read_to_string(task.path().join("children")) {
                    queue.extend(
                        children
                            .split_whitespace()
                            .filter_map(|value| value.parse::<i32>().ok()),
                    );
                }
            }
        }
    }
    Job {
        leader: pgid,
        processes,
    }
}
struct Stat {
    ppid: Pid,
    pgrp: Pid,
    tpgid: Pid,
    comm: String,
}
fn stat(pid: Pid) -> Option<Stat> {
    let value = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = value.rfind(')')?;
    let comm = value
        .get(value.find('(')?.saturating_add(1)..close)?
        .to_owned();
    let fields: Vec<_> = value
        .get(close.saturating_add(2)..)?
        .split_whitespace()
        .collect();
    Some(Stat {
        ppid: fields.get(1)?.parse().ok()?,
        pgrp: fields.get(2)?.parse().ok()?,
        tpgid: fields.get(5)?.parse().ok()?,
        comm,
    })
}
fn process(pid: Pid) -> Option<Process> {
    let stat = stat(pid)?;
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let argv = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| OsStr::from_bytes(part).to_string_lossy().into_owned())
        .collect();
    let env = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let env_agent = env
        .split(|byte| *byte == 0)
        .find_map(|part| part.strip_prefix(b"ZOR_AGENT="))
        .map(|part| String::from_utf8_lossy(part).into_owned());
    Some(Process {
        pid,
        ppid: stat.ppid,
        comm: stat.comm,
        argv0: None,
        argv,
        env_agent,
    })
}

pub struct Guard {
    fd: i32,
    original: nix::sys::termios::Termios,
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: Guard owns a valid borrowed descriptor for its lifetime.
            let fd = std::os::fd::BorrowedFd::borrow_raw(self.fd);
            let _ = nix::sys::termios::tcsetattr(
                fd,
                nix::sys::termios::SetArg::TCSANOW,
                &self.original,
            );
        }
    }
}
pub fn set_raw(fd: i32) -> io::Result<Guard> {
    unsafe {
        // SAFETY: nix borrows but does not retain the descriptor.
        let borrowed = std::os::fd::BorrowedFd::borrow_raw(fd);
        let original = nix::sys::termios::tcgetattr(borrowed)?;
        let mut raw = original.clone();
        nix::sys::termios::cfmakeraw(&mut raw);
        nix::sys::termios::tcsetattr(borrowed, nix::sys::termios::SetArg::TCSANOW, &raw)?;
        Ok(Guard { fd, original })
    }
}
pub fn winsize(fd: i32) -> portable_pty::PtySize {
    unsafe {
        // SAFETY: ioctl writes to a correctly sized winsize value.
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) == 0 {
            portable_pty::PtySize {
                rows: size.ws_row.max(1),
                cols: size.ws_col.max(1),
                pixel_width: size.ws_xpixel,
                pixel_height: size.ws_ypixel,
            }
        } else {
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        }
    }
}
pub fn forward_signal(pgid: Pid, signal: i32) -> io::Result<()> {
    unsafe {
        // SAFETY: kill validates the process-group id and signal number.
        if libc::kill(-pgid, signal) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
pub fn suspend_self() {
    unsafe {
        // SAFETY: SIGSTOP has defined process-wide semantics and no handler.
        libc::raise(libc::SIGSTOP);
    }
}
