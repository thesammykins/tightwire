//! Which output endpoint the Windows tray plays into (SPEC §10), and the
//! tray settings file that remembers it. Platform-independent so the policy
//! is unit-tested on macOS; the tray only feeds it the endpoint list.
//!
//! Policy: nothing is ever opened that the user did not pick. No saved choice means
//! no output at all; a saved choice that is absent means waiting for it, never
//! a substitute. The system default endpoint is listed but never chosen
//! automatically.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

/// One active render endpoint as Windows reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// IMMDevice::GetId, stable across reboots for the same endpoint.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The endpoint the user picked. `id` is empty only for a choice migrated
/// from 0.1.0's name-only `device = …` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChoice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraySettings {
    pub output: Option<OutputChoice>,
    /// Exclusive-mode preference per endpoint id; missing = off.
    pub exclusive: BTreeMap<String, bool>,
}

/// Exclusive mode is strictly opt-in (0.2.1), for every endpoint. 0.2.0
/// turned it on by default for virtual endpoints by name; in the field one
/// such endpoint accepted exclusive mode and then delivered its buffers in
/// bursts at an off-nominal rate, which shared mode on the same endpoint
/// did not. The tray describes it as experimental for virtual devices, and
/// the exclusive-mode watchdog (`watchdog.rs`) turns it off again if it
/// misbehaves.
pub const DEFAULT_EXCLUSIVE: bool = false;

impl TraySettings {
    /// Parses tray.conf. Unknown or malformed lines are skipped and returned
    /// as warnings, so a hand edit never loses the output choice.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut warnings = vec![];
        let (mut id, mut name, mut legacy) = (None, None, None);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                warnings.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "output_id" => id = Some(v.to_owned()),
                "output_name" => name = Some(v.to_owned()),
                // 0.1.0 stored only the friendly name.
                "device" => legacy = Some(v.to_owned()),
                _ => match k.strip_prefix("exclusive.") {
                    Some(dev) if !dev.is_empty() => match v {
                        "on" => {
                            s.exclusive.insert(dev.to_owned(), true);
                        }
                        "off" => {
                            s.exclusive.insert(dev.to_owned(), false);
                        }
                        _ => warnings.push(format!("line {}: {k} must be on or off", n + 1)),
                    },
                    _ => warnings.push(format!("line {}: unknown key {k}", n + 1)),
                },
            }
        }
        let id = id.unwrap_or_default();
        let name = name.or(legacy).unwrap_or_default();
        if !id.is_empty() || !name.is_empty() {
            s.output = Some(OutputChoice { id, name });
        }
        (s, warnings)
    }

    pub fn render(&self) -> String {
        let mut t = String::from(
            "# Tightwire tray settings. output_id is the Windows endpoint id; the name is for humans.\n",
        );
        if let Some(o) = &self.output {
            t += &format!("output_id = {}\n", one_line(&o.id));
            t += &format!("output_name = {}\n", one_line(&o.name));
        }
        for (id, on) in &self.exclusive {
            t += &format!(
                "exclusive.{} = {}\n",
                one_line(id),
                if *on { "on" } else { "off" }
            );
        }
        t
    }

    /// Missing file = defaults (first run). Any other read error is returned.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>), String> {
        match fs::read_to_string(path) {
            Ok(t) => Ok(Self::parse(&t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Self::default(), vec![])),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Atomic write (temp file + rename), so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().ok_or("settings path has no directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = path.with_extension("conf.tmp");
        let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(self.render().as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Where the exclusive preference for `id` comes from, for `doctor`: an
    /// explicit `exclusive.<id> = on|off` line (written by the tray's
    /// Exclusive mode toggle or its watchdog), or the default (off).
    pub fn exclusive_source(&self, id: &str) -> &'static str {
        let saved = self.exclusive.contains_key(id)
            || self
                .output
                .as_ref()
                .is_some_and(|o| self.exclusive.contains_key(&o.id));
        if saved {
            "saved in tray.conf (the tray's Exclusive mode toggle, or its watchdog)"
        } else {
            "default (off; exclusive mode is opt-in)"
        }
    }

    pub fn exclusive_for(&self, id: &str) -> bool {
        self.exclusive.get(id).copied().unwrap_or(DEFAULT_EXCLUSIVE)
    }

    /// Records an explicit pick from the device menu.
    pub fn choose(&mut self, ep: &Endpoint) {
        self.output = Some(OutputChoice {
            id: ep.id.clone(),
            name: ep.name.clone(),
        });
    }

    /// True when `ep` is the saved choice (by id; by exact name for a
    /// migrated name-only choice).
    pub fn is_chosen(&self, ep: &Endpoint) -> bool {
        self.output.as_ref().is_some_and(|o| {
            if o.id.is_empty() {
                o.name == ep.name
            } else {
                o.id == ep.id
            }
        })
    }
}

/// `tray.conf` next to the pairing file (`%APPDATA%\Tightwire\tray.conf`).
pub fn tray_settings_path() -> Option<std::path::PathBuf> {
    crate::pairing::default_config_path().map(|p| p.with_file_name("tray.conf"))
}

fn one_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// What the tray should do with the audio output right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No saved choice: open nothing, discard audio, ask the user to pick.
    ChooseOutput,
    /// The saved endpoint is not active (unplugged, Wave Link not running…):
    /// keep the choice, open nothing, retry later.
    Waiting { name: String },
    Open {
        id: String,
        name: String,
        exclusive: bool,
        /// Matched by name because the saved id was empty (0.1.0 file) or
        /// no longer exists; the caller stores the new id.
        rebound: bool,
    },
}

pub fn plan(s: &TraySettings, endpoints: &[Endpoint]) -> Plan {
    let Some(choice) = &s.output else {
        return Plan::ChooseOutput;
    };
    let open = |ep: &Endpoint, rebound: bool| Plan::Open {
        id: ep.id.clone(),
        name: ep.name.clone(),
        // A migrated or rebound choice keeps its exclusive preference, which
        // may be stored under the old id.
        exclusive: s
            .exclusive
            .get(&ep.id)
            .or_else(|| s.exclusive.get(&choice.id))
            .copied()
            .unwrap_or(DEFAULT_EXCLUSIVE),
        rebound,
    };
    if !choice.id.is_empty()
        && let Some(ep) = endpoints.iter().find(|e| e.id == choice.id)
    {
        return open(ep, false);
    }
    // Same exact name, and only one such endpoint: the same device under a
    // new id (driver reinstall). Ambiguous names are not guessed.
    let mut same_name = endpoints.iter().filter(|e| e.name == choice.name);
    if let (Some(ep), None) = (same_name.next(), same_name.next())
        && !choice.name.is_empty()
    {
        return open(ep, true);
    }
    Plan::Waiting {
        name: if choice.name.is_empty() {
            choice.id.clone()
        } else {
            choice.name.clone()
        },
    }
}

/// Device-menu text. Win32 menus treat `&` as a mnemonic marker, so it is doubled.
pub fn menu_label(ep: &Endpoint) -> String {
    let name = ep.name.replace('&', "&&");
    if ep.is_default {
        format!("{name} — system default, not recommended")
    } else {
        name
    }
}

/// Short state for the tray status line and tooltip.
pub fn plan_status(p: &Plan) -> String {
    match p {
        Plan::ChooseOutput => "Choose an output (right-click › Output device)".into(),
        Plan::Waiting { name } => format!("Waiting for {name}"),
        Plan::Open { name, .. } => format!("Output: {name}"),
    }
}

/// Logged once per stream format: how far the engine resamples. Any ratio
/// works; a large one just costs CPU and suggests changing the endpoint's
/// format in Windows' Sound settings.
pub fn resample_note(stream_rate: u32, out_rate: u32) -> Option<String> {
    if stream_rate == 0 || out_rate == 0 || stream_rate == out_rate {
        return None;
    }
    let ratio = out_rate as f64 / stream_rate as f64;
    let mut s = format!("resampling {stream_rate} Hz → {out_rate} Hz (ratio {ratio:.4})");
    if !(0.5..=2.0).contains(&ratio) {
        s += &format!(
            "; the endpoint runs at {out_rate} Hz — setting it to {stream_rate} Hz in Sound settings › Advanced saves CPU"
        );
    }
    Some(s)
}

/// Plain-words meaning of a WASAPI/COM failure while opening an output,
/// and what the user can do. Codes from audioclient.h (checked against the
/// `windows` crate in `output/wasapi.rs`); meanings from Microsoft's
/// IAudioClient::Initialize documentation.
pub fn explain_hresult(code: u32) -> Option<(&'static str, &'static str)> {
    Some(match code {
        0x8889_000A => (
            "the endpoint is in use: another app holds it in exclusive mode (or it is in shared use and exclusive mode was asked for)",
            "close the app that holds it, or untick \"Allow applications to take exclusive control of this device\" for it in Sound settings › More sound settings › Properties › Advanced. A virtual cable that is also (or shares hardware with) the Windows default output is often held this way; choose another endpoint or another default",
        ),
        0x8889_0008 => (
            "the endpoint does not accept the audio format",
            "check its format in Sound settings › More sound settings › Properties › Advanced",
        ),
        0x8889_0004 | 0x8889_000F => (
            "the endpoint was unplugged, disabled or reconfigured",
            "Tightwire opens it again when Windows reports it back",
        ),
        0x8889_000E => (
            "exclusive use of this endpoint is turned off in Windows",
            "use shared mode, or allow exclusive control in the endpoint's Advanced properties",
        ),
        0x8889_0010 => (
            "the Windows Audio service is not running",
            "start it in Services (Windows Audio), or restart the PC",
        ),
        0x8889_0017 => (
            "the audio engine is overloaded (CPU)",
            "close heavy audio apps or use shared mode",
        ),
        0x8889_0026 => (
            "the stream's resources were invalidated (stream suspended or an exclusive stream disconnected)",
            "Tightwire reopens it",
        ),
        0x8889_0028 => (
            "another app holds the shared engine at a different period",
            "Tightwire uses the period in force",
        ),
        0x8007_0005 => ("access denied", "check Windows privacy settings for audio"),
        _ => return None,
    })
}

/// `msg` with the plain-words meaning and hint for any `0x8889xxxx`-style
/// code it contains, e.g. "could not open X: 0x8889000A …".
pub fn describe_open_error(msg: &str) -> String {
    let code = msg
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|w| w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")))
        .find_map(|h| u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 8));
    match code.and_then(explain_hresult) {
        Some((what, hint)) => format!("{msg} — {what}. {hint}."),
        None => msg.to_owned(),
    }
}

/// Balloon text when nothing is chosen (Windows caps balloons at 255 chars).
pub const CHOOSE_OUTPUT_BALLOON: &str = "Choose where the Mac's audio goes: right-click › Output device. Pick a dedicated virtual endpoint (a Wave Link channel, or VB-Cable's \"CABLE Input\"), not your speakers.";

/// Longer first-run guidance for `doctor` (README has the same text).
pub const OUTPUT_GUIDANCE: &str = "\
Tightwire plays the Mac's audio into the one output endpoint you choose, and
never into anything you did not choose. Use a dedicated virtual endpoint, not
speakers or headphones:
  - Wave Link: create a channel for Tightwire in Wave Link; Windows lists its
    endpoints as \"<channel name> (Elgato Virtual Audio)\". Apps then hear it
    as part of Wave Link's mix output, i.e. whichever Wave Link microphone or
    mix device they record from.
  - VB-Cable: choose \"CABLE Input\"; apps record from \"CABLE Output\".
Whatever plays into the chosen endpoint is what Windows apps hear as the mic.
Shared mode is the default. Exclusive mode (tray › Exclusive mode) keeps other
apps from playing into the endpoint and can lower latency, but is
experimental for virtual devices: if it misbehaves, Tightwire switches back
to shared mode and says why.";

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str, name: &str, def: bool) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: name.into(),
            is_default: def,
        }
    }

    /// The endpoint list from a first real run (ids invented).
    fn sample_pc() -> Vec<Endpoint> {
        vec![
            ep(
                "{0.0.0.00000000}.{a1}",
                "Headset Microphone (Wireless Headset Dongle) (Elgato Virtual Audio)",
                false,
            ),
            ep(
                "{0.0.0.00000000}.{a2}",
                "Microphone (6- USB Microphone) (Elgato Virtual Audio)",
                false,
            ),
            ep(
                "{0.0.0.00000000}.{a3}",
                "Games (Elgato Virtual Audio)",
                false,
            ),
            ep(
                "{0.0.0.00000000}.{tv}",
                "Living Room TV (HD Audio Device)",
                true,
            ),
        ]
    }

    #[test]
    fn no_choice_never_opens_anything_not_even_the_default() {
        let p = plan(&TraySettings::default(), &sample_pc());
        assert_eq!(p, Plan::ChooseOutput);
        // Including when a "Wave Link"-looking endpoint exists.
        let mut eps = sample_pc();
        eps.push(ep("x", "Wave Link Stream", false));
        assert_eq!(plan(&TraySettings::default(), &eps), Plan::ChooseOutput);
    }

    #[test]
    fn chosen_endpoint_opens_by_id_in_shared_mode_by_default() {
        let mut s = TraySettings::default();
        s.choose(&sample_pc()[2]);
        assert_eq!(
            plan(&s, &sample_pc()),
            Plan::Open {
                id: "{0.0.0.00000000}.{a3}".into(),
                name: "Games (Elgato Virtual Audio)".into(),
                exclusive: false,
                rebound: false
            }
        );
        // So does the TV, if explicitly chosen.
        s.choose(&sample_pc()[3]);
        assert!(matches!(
            plan(&s, &sample_pc()),
            Plan::Open {
                exclusive: false,
                ..
            }
        ));
    }

    /// Exclusive mode is opt-in also after migrating 0.1.0's name-only
    /// tray.conf; only a saved toggle turns it on.
    #[test]
    fn virtual_endpoint_defaults_to_shared_also_after_migration() {
        let aux = ep(
            "{0.0.0.00000000}.{aux1}",
            "Aux 1 (Elgato Virtual Audio)",
            false,
        );
        let (old, warn) = TraySettings::parse("device = Aux 1 (Elgato Virtual Audio)\n");
        assert!(warn.is_empty());
        assert!(matches!(
            plan(&old, std::slice::from_ref(&aux)),
            Plan::Open {
                exclusive: false,
                rebound: true,
                ..
            }
        ));
        assert!(old.exclusive_source(&aux.id).contains("opt-in"));
        let mut toggled = old.clone();
        toggled.exclusive.insert(aux.id.clone(), true);
        assert!(matches!(
            plan(&toggled, std::slice::from_ref(&aux)),
            Plan::Open {
                exclusive: true,
                ..
            }
        ));
        assert!(toggled.exclusive_source(&aux.id).contains("toggle"));
    }

    #[test]
    fn exclusive_is_opt_in_for_every_name() {
        let mut s = TraySettings::default();
        for name in [
            "Games (Elgato Virtual Audio)",
            "CABLE Input (VB-Audio Virtual Cable)",
            "Speakers (Realtek(R) Audio)",
        ] {
            assert!(!s.exclusive_for(name));
        }
        s.choose(&sample_pc()[2]);
        s.exclusive.insert("{0.0.0.00000000}.{a3}".into(), true);
        assert!(matches!(
            plan(&s, &sample_pc()),
            Plan::Open {
                exclusive: true,
                ..
            }
        ));
    }

    #[test]
    fn absent_choice_waits_and_is_never_substituted() {
        let mut s = TraySettings::default();
        s.choose(&sample_pc()[1]);
        let without: Vec<_> = sample_pc()
            .into_iter()
            .filter(|e| !e.name.contains("USB Microphone"))
            .collect();
        assert_eq!(
            plan(&s, &without),
            Plan::Waiting {
                name: "Microphone (6- USB Microphone) (Elgato Virtual Audio)".into()
            }
        );
        assert_eq!(
            plan_status(&plan(&s, &without)),
            "Waiting for Microphone (6- USB Microphone) (Elgato Virtual Audio)"
        );
        // Back again: opens.
        assert!(matches!(
            plan(&s, &sample_pc()),
            Plan::Open { rebound: false, .. }
        ));
        // Nothing at all.
        assert!(matches!(plan(&s, &[]), Plan::Waiting { .. }));
    }

    #[test]
    fn new_id_same_unique_name_rebinds_but_ambiguous_names_wait() {
        let s = TraySettings {
            output: Some(OutputChoice {
                id: "{old}".into(),
                name: "Games (Elgato Virtual Audio)".into(),
            }),
            exclusive: [("{old}".to_string(), false)].into(),
        };
        assert_eq!(
            plan(&s, &sample_pc()),
            Plan::Open {
                id: "{0.0.0.00000000}.{a3}".into(),
                name: "Games (Elgato Virtual Audio)".into(),
                exclusive: false,
                rebound: true
            }
        );
        let mut twice = sample_pc();
        twice.push(ep("{dup}", "Games (Elgato Virtual Audio)", false));
        assert!(matches!(plan(&s, &twice), Plan::Waiting { .. }));
    }

    #[test]
    fn settings_round_trip_and_migrate_from_0_1_0() {
        let mut s = TraySettings::default();
        s.choose(&sample_pc()[1]);
        s.exclusive.insert(sample_pc()[1].id.clone(), false);
        s.exclusive.insert("{other}".into(), true);
        let (back, warn) = TraySettings::parse(&s.render());
        assert_eq!(back, s);
        assert!(warn.is_empty());

        // 0.1.0 wrote `device = <name>` (or `device = ` for automatic).
        let (old, _) =
            TraySettings::parse("device = Microphone (6- USB Microphone) (Elgato Virtual Audio)\n");
        assert_eq!(
            old.output,
            Some(OutputChoice {
                id: String::new(),
                name: "Microphone (6- USB Microphone) (Elgato Virtual Audio)".into()
            })
        );
        assert!(old.is_chosen(&sample_pc()[1]));
        assert!(matches!(
            plan(&old, &sample_pc()),
            Plan::Open { rebound: true, .. }
        ));
        let (auto, _) = TraySettings::parse("device = \r\n");
        assert_eq!(auto.output, None);
    }

    #[test]
    fn parse_tolerates_junk_and_crlf() {
        let (s, warn) = TraySettings::parse(
            "# c\r\noutput_id = {x}\r\nbogus\r\ncolour = red\r\nexclusive.{x} = maybe\r\nexclusive.{x} = off\r\n",
        );
        assert_eq!(s.output.as_ref().unwrap().id, "{x}");
        assert_eq!(s.exclusive.get("{x}"), Some(&false));
        assert_eq!(warn.len(), 3, "{warn:?}");
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = std::env::temp_dir().join(format!("tw-outsel-{}", std::process::id()));
        let path = dir.join("tray.conf");
        let _ = fs::remove_file(&path);
        assert_eq!(
            TraySettings::load(&path).unwrap().0,
            TraySettings::default()
        );
        let mut s = TraySettings::default();
        s.choose(&sample_pc()[2]);
        s.save(&path).unwrap();
        assert_eq!(TraySettings::load(&path).unwrap().0, s);
        // Choosing again replaces, and survives a "restart" (fresh load).
        s.choose(&sample_pc()[1]);
        s.save(&path).unwrap();
        let (again, _) = TraySettings::load(&path).unwrap();
        assert!(again.is_chosen(&sample_pc()[1]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_mark_default_and_escape_ampersands() {
        assert_eq!(
            menu_label(&sample_pc()[3]),
            "Living Room TV (HD Audio Device) — system default, not recommended"
        );
        assert_eq!(menu_label(&ep("i", "R&D Speakers", false)), "R&&D Speakers");
    }

    #[test]
    fn open_errors_are_explained_in_plain_words() {
        let m = describe_open_error("could not open \"Example 8-ch Cable\": 0x8889000A");
        assert!(
            m.contains("in use") && m.contains("exclusive control"),
            "{m}"
        );
        let m = describe_open_error("Error 0x88890004 while starting");
        assert!(m.contains("unplugged"), "{m}");
        assert_eq!(describe_open_error("timeout"), "timeout");
        assert_eq!(describe_open_error("0x1234"), "0x1234");
    }

    #[test]
    fn resample_note_only_when_rates_differ() {
        assert_eq!(resample_note(48_000, 48_000), None);
        assert_eq!(resample_note(0, 48_000), None);
        let n = resample_note(48_000, 192_000).unwrap();
        assert!(
            n.starts_with("resampling 48000 Hz → 192000 Hz (ratio 4.0000)"),
            "{n}"
        );
        assert!(n.contains("Sound settings"));
        let n = resample_note(48_000, 44_100).unwrap();
        assert!(
            n.contains("ratio 0.9187") && !n.contains("Sound settings"),
            "{n}"
        );
        assert!(CHOOSE_OUTPUT_BALLOON.encode_utf16().count() < 256);
    }
}
