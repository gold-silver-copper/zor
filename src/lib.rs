#![doc = include_str!("../DESIGN.md")]
#![forbid(unsafe_code)]

pub mod osc;

#[cfg(feature = "cli")]
pub mod emit;
#[cfg(feature = "cli")]
pub mod pty;
#[cfg(feature = "cli")]
pub mod rules;
#[cfg(feature = "cli")]
pub mod screen;
#[cfg(feature = "cli")]
pub mod state;
