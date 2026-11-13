//! One log path for the CLI (stderr) and the Windows tray app (rotating file
//! under %LOCALAPPDATA%\Tightwire). Never called from the audio callback.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
const ROTATE_AT: u64 = 1024 * 1024;
const KEEP: usize = 3;

/// Also write log lines to `path`, rotating at 1 MB (path.1 … path.3).
pub fn to_file(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink {
        path: path.to_owned(),
        file,
        written,
    });
    Ok(())
}

/// `%LOCALAPPDATA%\Tightwire\tightwire.log` (temp dir if unset).
pub fn default_log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Tightwire")
        .join("tightwire.log")
}

pub fn log_path() -> Option<PathBuf> {
    SINK.lock()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.path.clone()))
}

fn rotate(s: &mut Sink) {
    for i in (1..KEEP).rev() {
        let _ = std::fs::rename(
            s.path.with_extension(format!("log.{i}")),
            s.path.with_extension(format!("log.{}", i + 1)),
        );
    }
    let _ = std::fs::rename(&s.path, s.path.with_extension("log.1"));
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&s.path) {
        s.file = f;
        s.written = 0;
    }
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_mut() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("{} {msg}\n", utc(secs));
        if s.file.write_all(line.as_bytes()).is_ok() {
            s.written += line.len() as u64;
        }
        if s.written > ROTATE_AT {
            rotate(s);
        }
    }
}

/// "2026-11-12T18:42:07Z" without a date crate.
fn utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[macro_export]
macro_rules! twlog {
    ($($t:tt)*) => { $crate::applog::log(&format!($($t)*)) };
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short git commit of the build (`+dirty` with uncommitted changes); see build.rs.
pub const COMMIT: &str = env!("TIGHTWIRE_GIT_COMMIT");

/// "Tightwire 0.2.0 (abc1234567)": shown in the tray menu, doctor and log headers.
pub fn build_label() -> String {
    format!("Tightwire {VERSION} ({COMMIT})")
}

/// The first line every process writes, so separate runs stand out in one
/// shared log file.
pub fn start_header(role: &str, pid: u32) -> String {
    format!(
        "======== {} {role} start, pid {pid}, {}/{} ========",
        build_label(),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Removes ANSI colour sequences (usbip-win2 colours "error:" when it
/// thinks it has a console).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// One-line, bounded rendering of tool output for the log: lines joined
/// with " | ", ANSI colour codes removed, cut at `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let joined = strip_ansi(text)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    let total = joined.chars().count();
    if total <= max {
        joined
    } else {
        let mut s: String = joined.chars().take(max).collect();
        s.push_str(&format!("… (+{} chars)", total - max));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_format() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_762_970_527), "2025-11-12T18:02:07Z");
    }

    #[test]
    fn header_names_version_and_commit() {
        let h = start_header("tray", 42);
        assert!(h.contains(&format!("Tightwire {VERSION} (")), "{h}");
        assert!(h.contains(COMMIT), "{h}");
        assert!(h.contains("tray start, pid 42"), "{h}");
        assert_eq!(VERSION, "0.2.1");
    }

    #[test]
    fn one_line_joins_strips_colour_and_truncates() {
        assert_eq!(one_line("", 10), "");
        assert_eq!(
            one_line("\x1b[31merror\x1b[0m: cannot connect\r\n\n  retry\n", 100),
            "error: cannot connect | retry"
        );
        assert_eq!(one_line("abcdef", 3), "abc… (+3 chars)");
    }
}
