#![allow(unsafe_code)]
use super::{Job, Pid, Process};
use std::{ffi::CStr, io};
const PROC_PGRP_ONLY: u32 = 2;

pub fn foreground_pgid(child: Pid) -> Option<Pid> {
    info(child)
        .map(|value| value.e_tpgid as Pid)
        .filter(|value| *value > 0)
}
pub fn leader(pgid: Pid) -> Option<Process> {
    process(pgid).filter(|_| info(pgid).is_some_and(|value| value.pbi_pgid as Pid == pgid))
}
pub fn job(_: Pid, pgid: Pid) -> Job {
    unsafe {
        // SAFETY: proc_listpids writes at most the supplied allocated byte count.
        let bytes = libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, std::ptr::null_mut(), 0);
        if bytes <= 0 {
            return Job {
                leader: pgid,
                processes: Vec::new(),
            };
        }
        let count = usize::try_from(bytes).unwrap_or_default() / std::mem::size_of::<Pid>();
        let mut pids = vec![0; count];
        let size = i32::try_from(pids.len() * std::mem::size_of::<Pid>()).unwrap_or(i32::MAX);
        let written =
            libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, pids.as_mut_ptr().cast(), size);
        pids.truncate(usize::try_from(written).unwrap_or_default() / std::mem::size_of::<Pid>());
        Job {
            leader: pgid,
            processes: pids
                .into_iter()
                .filter_map(process)
                .filter(|value| info(value.pid).is_some_and(|entry| entry.pbi_pgid as Pid == pgid))
                .collect(),
        }
    }
}
fn info(pid: Pid) -> Option<libc::proc_bsdinfo> {
    unsafe {
        // SAFETY: proc_pidinfo writes at most the supplied structure size.
        let mut value = std::mem::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        (libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut value as *mut libc::proc_bsdinfo).cast(),
            size,
        ) == size)
            .then_some(value)
    }
}
fn process(pid: Pid) -> Option<Process> {
    let value = info(pid)?;
    let comm = unsafe {
        // SAFETY: pbi_comm is a kernel-provided NUL-terminated fixed buffer.
        CStr::from_ptr(value.pbi_comm.as_ptr())
    }
    .to_string_lossy()
    .into_owned();
    Some(Process {
        pid,
        ppid: value.pbi_ppid as Pid,
        comm: comm.clone(),
        argv0: Some(comm.clone()),
        argv: vec![comm],
        env_agent: None,
    })
}
pub struct Guard {
    fd: i32,
    original: libc::termios,
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: original was initialized by tcgetattr for this live fd.
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}
pub fn set_raw(fd: i32) -> io::Result<Guard> {
    unsafe {
        // SAFETY: termios is plain data and libc validates the fd.
        let mut original = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        libc::cfmakeraw(&mut raw);
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            return Err(io::Error::last_os_error());
        }
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
