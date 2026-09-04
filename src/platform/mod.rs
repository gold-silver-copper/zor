pub use crate::rules::ident::{Job, Pid, Process};

pub mod probe;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{
    Guard, foreground_pgid, forward_signal, job, leader, set_raw, suspend_self, winsize,
};
#[cfg(target_os = "macos")]
pub use macos::{
    Guard, foreground_pgid, forward_signal, job, leader, set_raw, suspend_self, winsize,
};
