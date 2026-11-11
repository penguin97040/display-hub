//! A stand-in for a Display Hub display, for testing the hub without hardware.
//!
//! It behaves as the firmware contract asks (`docs/protocol-v1.md`): it polls with jitter and
//! backoff, keeps the last good snapshot on disk, shows age and stale state from the hub's clock,
//! and explains problems in plain words.

pub mod cache;
pub mod device;
pub mod paired;
pub mod render;

pub use device::{ClockJitter, Device, Jitter, Link, NoJitter, Problem};
