//! A small status file the tray rewrites every few seconds
//! (`%LOCALAPPDATA%\Tightwire\status.txt`), so `tightwire.exe doctor`, which
//! runs as a separate process, can report what the running tray sees: phase,
//! output, and how long ago the paired Mac last sent an authenticated packet.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeStatus {
    pub pid: u32,
    pub build: String,
    pub written_unix: u64,
    pub phase: String,
    pub output: String,
    pub paired_with: String,
    pub peer: String,
    /// Unix seconds of the last authenticated packet from the Mac (0 = none this run).
    pub last_auth_unix: u64,
    pub last_handoff: String,
}

impl RuntimeStatus {
    pub fn render(&self) -> String {
        let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).collect() };
        format!(
            "pid = {}\nbuild = {}\nwritten_unix = {}\nphase = {}\noutput = {}\npaired_with = {}\npeer = {}\nlast_auth_unix = {}\nlast_handoff = {}\n",
            self.pid,
            clean(&self.build),
            self.written_unix,
            clean(&self.phase),
            clean(&self.output),
            clean(&self.paired_with),
            clean(&self.peer),
            self.last_auth_unix,
            clean(&self.last_handoff),
        )
    }

    /// Lenient: unknown keys and bad numbers are ignored (the file is
    /// diagnostic only, and may come from another Tightwire version).
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().to_owned();
            match k.trim() {
                "pid" => s.pid = v.parse().unwrap_or(0),
                "build" => s.build = v,
                "written_unix" => s.written_unix = v.parse().unwrap_or(0),
                "phase" => s.phase = v,
                "output" => s.output = v,
                "paired_with" => s.paired_with = v,
                "peer" => s.peer = v,
                "last_auth_unix" => s.last_auth_unix = v.parse().unwrap_or(0),
                "last_handoff" => s.last_handoff = v,
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("txt.tmp");
        fs::write(&tmp, self.render())?;
        fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        fs::read_to_string(path).map(|t| Self::parse(&t))
    }
}

/// `%LOCALAPPDATA%\Tightwire\status.txt` on Windows (next to the log);
/// beside the pairing file elsewhere.
pub fn default_path() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| {
            std::path::PathBuf::from(d)
                .join("Tightwire")
                .join("status.txt")
        })
    } else {
        crate::pairing::default_config_path().map(|p| p.with_file_name("status.txt"))
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "12 s ago", "3 min ago", "never".
pub fn age(now: u64, then: u64) -> String {
    if then == 0 {
        return "never".into();
    }
    let d = now.saturating_sub(then);
    match d {
        0..=119 => format!("{d} s ago"),
        120..=7199 => format!("{} min ago", d / 60),
        _ => format!("{} h ago", d / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_lenient_parse() {
        let s = RuntimeStatus {
            pid: 7,
            build: "Tightwire 0.2.0 (abc)".into(),
            written_unix: 100,
            phase: "streaming".into(),
            output: "wasapi \"Games\nX\"".into(),
            paired_with: "mac".into(),
            peer: "192.168.1.10:4812".into(),
            last_auth_unix: 99,
            last_handoff: "attach 1-1: port 3".into(),
        };
        let mut expect = s.clone();
        expect.output = "wasapi \"GamesX\"".into();
        assert_eq!(RuntimeStatus::parse(&s.render()), expect);
        let p = RuntimeStatus::parse("pid = x\nfuture = 1\nphase = searching\n");
        assert_eq!(p.pid, 0);
        assert_eq!(p.phase, "searching");
    }

    #[test]
    fn ages() {
        assert_eq!(age(100, 0), "never");
        assert_eq!(age(100, 97), "3 s ago");
        assert_eq!(age(1000, 100), "15 min ago");
        assert_eq!(age(20_000, 100), "5 h ago");
        assert_eq!(age(5, 10), "0 s ago");
    }
}
