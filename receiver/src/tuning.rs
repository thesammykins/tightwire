//! Jitter-buffer floor and cap set from the Mac (SPEC §3 type 21), persisted
//! in `tuning.conf` next to the pairing file so they survive restarts.
//!
//! Precedence at start-up: `--target-ms` / `--max-ms` on the command line,
//! then this file, then the defaults (adaptive floor, 20 ms cap). A message
//! from the Mac applies at once and rewrites the file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::protocol::{MAX_TUNING_US, TuningSet};

/// Default cap: SPEC §5.
pub const DEFAULT_MAX_US: u32 = 20_000;
/// Smallest cap accepted remotely: below one 1 ms packet plus a device
/// period the buffer cannot hold anything.
pub const MIN_MAX_US: u32 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Target floor (µs); 0 = adaptive only.
    pub min_target_us: u32,
    pub max_target_us: u32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            min_target_us: 0,
            max_target_us: DEFAULT_MAX_US,
        }
    }
}

impl Tuning {
    /// Checks a type-21 request. Out-of-range values are refused whole
    /// rather than clamped, so the Mac never believes something was applied
    /// that was not.
    pub fn from_request(m: &TuningSet) -> Result<Self, String> {
        let min = if m.adaptive { 0 } else { m.min_target_us };
        if !(MIN_MAX_US..=MAX_TUNING_US).contains(&m.max_target_us) {
            return Err(format!(
                "cap {:.1} ms is outside {:.0}–{:.0} ms",
                m.max_target_us as f64 / 1000.0,
                MIN_MAX_US as f64 / 1000.0,
                MAX_TUNING_US as f64 / 1000.0
            ));
        }
        if min > m.max_target_us {
            return Err(format!(
                "floor {:.1} ms is above the cap {:.1} ms",
                min as f64 / 1000.0,
                m.max_target_us as f64 / 1000.0
            ));
        }
        Ok(Self {
            min_target_us: min,
            max_target_us: m.max_target_us,
        })
    }

    pub fn describe(&self) -> String {
        format!(
            "floor {}, cap {:.1} ms",
            if self.min_target_us == 0 {
                "adaptive".to_string()
            } else {
                format!("{:.1} ms", self.min_target_us as f64 / 1000.0)
            },
            self.max_target_us as f64 / 1000.0
        )
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut t = Self::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected key = value", n + 1))?;
            let v: u32 = v
                .trim()
                .parse()
                .map_err(|_| format!("line {}: {} is not a whole number", n + 1, v.trim()))?;
            match k.trim() {
                "buffer_floor_us" => t.min_target_us = v,
                "buffer_max_us" => t.max_target_us = v,
                other => return Err(format!("line {}: unknown key {other}", n + 1)),
            }
        }
        if t.min_target_us > t.max_target_us || t.max_target_us > MAX_TUNING_US {
            return Err(format!("inconsistent values ({})", t.describe()));
        }
        Ok(t)
    }

    pub fn render(&self) -> String {
        format!(
            "# Jitter-buffer floor and cap, set from the Mac's latency test (0 = adaptive floor).\nbuffer_floor_us = {}\nbuffer_max_us = {}\n",
            self.min_target_us, self.max_target_us
        )
    }

    /// Missing file = None (defaults apply).
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match fs::read_to_string(path) {
            Ok(t) => Self::parse(&t)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().ok_or("tuning path has no directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = path.with_extension("conf.tmp");
        let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(self.render().as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// `tuning.conf` next to the pairing file.
pub fn path_for(config: &Path) -> PathBuf {
    config.with_file_name("tuning.conf")
}

/// Start-up limits from the command line, the file and the defaults.
pub fn effective(cli_min_ms: Option<f64>, cli_max_ms: Option<f64>, file: Option<Tuning>) -> Tuning {
    let base = file.unwrap_or_default();
    Tuning {
        min_target_us: cli_min_ms.map_or(base.min_target_us, |m| (m * 1000.0) as u32),
        max_target_us: cli_max_ms.map_or(base.max_target_us, |m| (m * 1000.0) as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(adaptive: bool, min: u32, max: u32) -> TuningSet {
        TuningSet {
            request_id: 1,
            adaptive,
            min_target_us: min,
            max_target_us: max,
        }
    }

    #[test]
    fn requests_are_validated_whole() {
        assert_eq!(
            Tuning::from_request(&req(false, 4000, 12000)),
            Ok(Tuning {
                min_target_us: 4000,
                max_target_us: 12000
            })
        );
        assert_eq!(
            Tuning::from_request(&req(true, 4000, 12000))
                .unwrap()
                .min_target_us,
            0
        );
        assert!(Tuning::from_request(&req(false, 13000, 12000)).is_err());
        assert!(Tuning::from_request(&req(false, 0, 1000)).is_err());
        assert!(Tuning::from_request(&req(false, 0, MAX_TUNING_US + 1)).is_err());
    }

    #[test]
    fn file_round_trip_and_precedence() {
        let t = Tuning {
            min_target_us: 3500,
            max_target_us: 15000,
        };
        assert_eq!(Tuning::parse(&t.render()), Ok(t));
        assert!(Tuning::parse("buffer_floor_us = x").is_err());
        assert!(Tuning::parse("colour = 1").is_err());
        assert_eq!(effective(None, None, None), Tuning::default());
        assert_eq!(effective(None, None, Some(t)), t);
        assert_eq!(
            effective(Some(2.0), None, Some(t)),
            Tuning {
                min_target_us: 2000,
                max_target_us: 15000
            }
        );
        let dir = std::env::temp_dir().join(format!("tw-tuning-{}", std::process::id()));
        let path = dir.join("tuning.conf");
        assert_eq!(Tuning::load(&path), Ok(None));
        t.save(&path).unwrap();
        assert_eq!(Tuning::load(&path), Ok(Some(t)));
        let _ = fs::remove_dir_all(&dir);
    }
}
