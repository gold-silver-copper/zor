pub type Pid = i32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Process {
    pub pid: Pid,
    pub ppid: Pid,
    pub comm: String,
    pub argv0: Option<String>,
    pub argv: Vec<String>,
    pub env_agent: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub leader: Pid,
    pub processes: Vec<Process>,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{Guard, foreground_pgid, job, leader, set_raw, winsize};
#[cfg(target_os = "macos")]
pub use macos::{Guard, foreground_pgid, job, leader, set_raw, winsize};
