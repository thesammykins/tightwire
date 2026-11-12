//! No output at all: used while no device is chosen or the chosen one is
//! absent, so pairing, liveness and device handoff keep working without
//! opening any audio endpoint. The engine is never run; audio packets fill
//! the ring and are then dropped by the socket thread (counted as ring_full).

use super::{Output, OutputFormat, Result, Running};
use crate::engine::Engine;

pub struct DiscardOutput {
    reason: String,
}

impl DiscardOutput {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Output for DiscardOutput {
    fn format(&self) -> OutputFormat {
        // Nominal values; nothing renders at this rate.
        OutputFormat {
            rate: 48_000,
            channels: 2,
            period_frames: 480,
            mode: crate::protocol::OutputMode::None,
        }
    }

    fn describe(&self) -> String {
        format!("none, audio discarded ({})", self.reason)
    }

    fn start(self: Box<Self>, engine: Engine) -> Result<Box<dyn Running>> {
        Ok(Box::new(DiscardRunning { _engine: engine }))
    }
}

struct DiscardRunning {
    _engine: Engine,
}

impl Running for DiscardRunning {
    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}
