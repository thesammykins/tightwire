//! Output backends behind one trait (SPEC §5).

use crate::engine::Engine;

pub mod cpal_out;
pub mod discard;
pub mod file;
#[cfg(windows)]
pub mod wasapi;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    pub rate: u32,
    pub channels: usize,
    /// Frames per callback (nominal); feeds the jitter-buffer floor.
    pub period_frames: usize,
    /// Reported to the Mac in the stats extension.
    pub mode: crate::protocol::OutputMode,
}

/// An opened, not yet running output device.
pub trait Output {
    fn format(&self) -> OutputFormat;
    /// Human-readable device and mode, for the start-up log line.
    fn describe(&self) -> String;
    /// Starts pulling audio from `engine` on the backend's own thread.
    fn start(self: Box<Self>, engine: Engine) -> Result<Box<dyn Running>>;
}

pub trait Running {
    /// Set once the backend has stopped on its own (device lost, file done).
    fn failed(&self) -> Option<String> {
        None
    }
    /// Stops playback and releases the device.
    fn stop(self: Box<Self>) -> Result<()>;
}
