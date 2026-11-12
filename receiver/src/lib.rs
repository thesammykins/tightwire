//! Tightwire receiver library: wire protocol, authentication and pairing,
//! jitter buffer, drift-compensating resampler and output backends. The
//! binary in `main.rs` wires them together.

pub mod applog;
pub mod auth;
pub mod clock;
pub mod deck;
pub mod doctor;
pub mod drift;
pub mod engine;
pub mod handoff;
pub mod ingress;
pub mod jitter;
pub mod net;
pub mod output;
pub mod outsel;
pub mod pairing;
pub mod protocol;
pub mod resample;
pub mod runstatus;
pub mod service;
#[cfg(windows)]
pub mod tray;
pub mod tuning;
#[cfg(windows)]
pub mod winconsole;
