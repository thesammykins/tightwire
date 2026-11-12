//! The binary uses the Windows GUI subsystem (no console for the tray app or
//! Stream Deck buttons). For CLI use from cmd/PowerShell, attach to the parent
//! console so println!/eprintln! are visible.

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

pub fn attach_parent() {
    // SAFETY: fails harmlessly when there is no parent console (e.g. launched
    // from a Stream Deck button); std's handles then discard output.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
