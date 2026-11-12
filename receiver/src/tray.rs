//! Windows tray app (SPEC §10): runs the receiver with no console and exposes
//! status, output device, exclusive mode, pairing, Stream Deck handoff,
//! start-at-login, the log and quit from the notification-area icon. Plain
//! Win32 through the `windows` crate already used for WASAPI, so no extra GUI
//! dependency.
//!
//! UNTESTED ON WINDOWS: compiled and linted for x86_64-pc-windows-msvc and
//! cross-linked for windows-gnu on macOS; never run. The output-selection
//! policy it follows lives in `outsel` and is unit-tested on any OS.
//!
//! Output rule: the tray opens only the endpoint the user picked (by
//! endpoint id). With no pick, or while the pick is absent, the receiver
//! still runs (pairing, liveness, handoff) but opens no output and discards
//! audio (SPEC deviation 15 explains why there is no automatic choice).
//!
//! Threading: everything Win32 lives on the main thread's message loop.
//! Worker threads (pairing, deck requests) report back through `NOTICES` and
//! wake the loop with `PostMessageW(WM_APP_WAKE)`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFY_ICON_INFOTIP_FLAGS, NOTIFYICONDATAW, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::net::KeySource;
use crate::outsel::{self, Endpoint, Plan, TraySettings};
use crate::pairing::{self, ConfigFile, PairEvent, PairOptions};
use crate::protocol::{self, DeckTarget};
use crate::runstatus::{self, RuntimeStatus};
use crate::service::{self, Receiver};
use crate::{applog, deck, twlog};

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_WAKE: u32 = WM_APP + 2;
const TIMER_ID: usize = 1;

const ID_STATUS: usize = 1;
const ID_PAIR: usize = 2;
const ID_DECK_MAC: usize = 3;
const ID_DECK_PC: usize = 4;
const ID_LOGIN: usize = 5;
const ID_OPEN_LOG: usize = 6;
const ID_FORGET: usize = 7;
const ID_QUIT: usize = 8;
const ID_EXCLUSIVE: usize = 9;
const ID_DEVICE_BASE: usize = 100;
const MAX_DEVICES: usize = 50;

/// How often a missing or failed output is looked for again.
const WAITING_RECHECK: Duration = Duration::from_secs(5);
const FAILED_RECHECK: Duration = Duration::from_secs(10);
/// After the Mac accepts a deck request, how long to wait for its handoff
/// offer/release before logging that none came.
const DECK_WATCH: Duration = Duration::from_secs(20);

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Tightwire");

/// The main window handle, for worker threads to wake the loop.
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static NOTICES: Mutex<Vec<Notice>> = Mutex::new(Vec::new());

enum Notice {
    Info(String),
    Error(String),
    /// Pairing changed: refresh the menu.
    Repaired,
    /// The Mac accepted a deck request: expect a handoff offer/release.
    DeckAccepted(DeckTarget),
}

fn post(n: Notice) {
    match &n {
        Notice::Info(m) => twlog!("notice: {m}"),
        Notice::Error(m) => twlog!("problem: {m}"),
        Notice::Repaired | Notice::DeckAccepted(_) => {}
    }
    if let Ok(mut q) = NOTICES.lock() {
        q.push(n);
    }
    let h = HWND_RAW.load(Ordering::Relaxed);
    if h != 0 {
        // SAFETY: posting to our own window; the handle stays valid until exit.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

struct App {
    hwnd: HWND,
    receiver: Option<Receiver>,
    settings_path: Option<PathBuf>,
    settings: TraySettings,
    endpoints: Vec<Endpoint>,
    /// What the running receiver was started with.
    plan: Plan,
    /// The chosen endpoint is present but could not be opened.
    output_error: Option<String>,
    next_output_check: Instant,
    /// The receiver itself (UDP socket) failed to start.
    retry_at: Option<Instant>,
    pairing_busy: bool,
    choose_notified: bool,
    last_state: String,
    resample_logged: Option<(u32, u32)>,
    deck_watch: Option<(Instant, DeckTarget)>,
    last_handoff: String,
    next_status_write: Instant,
    icon: HICON,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn wide(s: &str, out: &mut [u16]) {
    let v: Vec<u16> = s.encode_utf16().take(out.len() - 1).collect();
    out[..v.len()].copy_from_slice(&v);
    out[v.len()] = 0;
}

/// A 32×32 status dot drawn at runtime (no resource compiler needed).
fn make_icon(rgb: (u8, u8, u8)) -> HICON {
    const N: i32 = 32;
    // SAFETY: standard DIB-section icon creation; bitmaps are deleted after
    // CreateIconIndirect copies them.
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: N,
                biHeight: -N,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        };
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (N * N) as usize);
        let c = (N as f32 - 1.0) / 2.0;
        for y in 0..N {
            for x in 0..N {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                let a = (13.5 - d).clamp(0.0, 1.0);
                let ring = if (d - 13.0).abs() < 1.5 { 0.6 } else { 1.0 };
                let (r, g, b) = (
                    (rgb.0 as f32 * ring * a) as u32,
                    (rgb.1 as f32 * ring * a) as u32,
                    (rgb.2 as f32 * ring * a) as u32,
                );
                px[(y * N + x) as usize] = ((a * 255.0) as u32) << 24 | r << 16 | g << 8 | b;
            }
        }
        let mask: HBITMAP = CreateBitmap(N, N, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn state_color(state: &str) -> (u8, u8, u8) {
    match state {
        "stopped" => (220, 60, 50),
        "streaming" => (40, 190, 90),
        "searching" => (230, 180, 40),
        "choose an output" | "waiting for output" | "output failed" => (235, 120, 40),
        _ => (140, 140, 150),
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uCallbackMessage: WM_APP_TRAY,
        ..Default::default()
    }
}

fn set_tip_and_icon(app: &mut App, tip: &str, icon: HICON) {
    let mut d = tray_data(app.hwnd);
    d.uFlags = NIF_TIP | NIF_ICON;
    d.hIcon = icon;
    wide(tip, &mut d.szTip);
    // SAFETY: valid NOTIFYICONDATAW for our icon id.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        if !app.icon.is_invalid() {
            let _ = DestroyIcon(app.icon);
        }
    }
    app.icon = icon;
}

fn balloon(hwnd: HWND, title: &str, text: &str, flags: NOTIFY_ICON_INFOTIP_FLAGS) {
    let mut d = tray_data(hwnd);
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = flags;
    wide(title, &mut d.szInfoTitle);
    wide(text, &mut d.szInfo);
    // SAFETY: valid NOTIFYICONDATAW for our icon id.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

fn save_settings(app: &App) {
    let Some(p) = &app.settings_path else {
        post(Notice::Error(
            "cannot find %APPDATA% to save settings".into(),
        ));
        return;
    };
    match app.settings.save(p) {
        Ok(()) => twlog!("settings saved to {}", p.display()),
        Err(e) => post(Notice::Error(format!("could not save settings: {e}"))),
    }
}

fn refresh_endpoints(app: &mut App) {
    match crate::output::wasapi::render_endpoints() {
        Ok(d) => app.endpoints = d,
        Err(e) => twlog!("could not list output endpoints: {e}"),
    }
}

fn log_endpoints(app: &App) {
    twlog!("active output endpoints ({}):", app.endpoints.len());
    for e in &app.endpoints {
        twlog!(
            "  {}{}{}  [{}]",
            if app.settings.is_chosen(e) {
                "[chosen] "
            } else {
                ""
            },
            e.name,
            if e.is_default {
                " (system default)"
            } else {
                ""
            },
            e.id
        );
    }
}

fn stop_receiver(app: &mut App) {
    if let Some(r) = app.receiver.take()
        && let Err(e) = r.stop()
    {
        twlog!("receiver stop: {e}");
    }
}

fn start_with(
    backend: service::Backend,
    device_id: Option<String>,
    exclusive: bool,
) -> Result<Receiver, String> {
    let path = pairing::default_config_path().ok_or("cannot find %APPDATA%")?;
    Receiver::start(service::Options {
        backend,
        device: None,
        device_id,
        shared: !exclusive,
        period: None,
        port: protocol::DEFAULT_PORT,
        bind: std::net::Ipv4Addr::UNSPECIFIED.into(),
        allow: None,
        target_ms: None,
        max_ms: None,
        keys: KeySource::Config(path),
        usbip: None,
    })
    .map_err(|e| e.to_string())
}

/// Starts the receiver for the current settings and endpoints. Opens an
/// output only for an explicit, present choice; otherwise runs without one.
fn apply_output(app: &mut App) {
    let mut plan = outsel::plan(&app.settings, &app.endpoints);
    if let Plan::Open {
        id,
        name,
        rebound: true,
        ..
    } = &plan
    {
        let old = app
            .settings
            .output
            .as_ref()
            .map(|o| o.id.clone())
            .unwrap_or_default();
        twlog!(
            "output \"{name}\" found under a new endpoint id {id} (was \"{old}\"); updating settings"
        );
        if let Some(pref) = app.settings.exclusive.remove(&old) {
            app.settings.exclusive.insert(id.clone(), pref);
        }
        if let Some(o) = app.settings.output.as_mut() {
            o.id = id.clone();
        }
        save_settings(app);
        plan = outsel::plan(&app.settings, &app.endpoints);
    }
    stop_receiver(app);
    // A retry of the same failing open is logged, not notified again.
    let was_failing = app.output_error.take().is_some() && app.plan == plan;
    app.resample_logged = None;
    let started = match &plan {
        Plan::Open {
            id,
            name,
            exclusive,
            ..
        } => {
            twlog!(
                "starting receiver on UDP {} → output \"{name}\" ({} requested)",
                protocol::DEFAULT_PORT,
                if *exclusive { "exclusive" } else { "shared" }
            );
            match start_with(service::Backend::Wasapi, Some(id.clone()), *exclusive) {
                Ok(r) => Ok(r),
                Err(e) => {
                    let msg = format!("could not open \"{name}\": {e}");
                    let line = format!("{msg}; retrying every {} s", FAILED_RECHECK.as_secs());
                    if was_failing {
                        twlog!("{line}");
                    } else {
                        post(Notice::Error(line));
                    }
                    app.output_error = Some(msg.clone());
                    app.next_output_check = Instant::now() + FAILED_RECHECK;
                    // Keep pairing and handoff alive without audio.
                    start_with(service::Backend::Discard { reason: msg }, None, false)
                }
            }
        }
        Plan::Waiting { name } => {
            twlog!(
                "starting receiver on UDP {} without output: waiting for \"{name}\" (rechecked every {} s)",
                protocol::DEFAULT_PORT,
                WAITING_RECHECK.as_secs()
            );
            if app.plan != plan {
                balloon(
                    app.hwnd,
                    "Tightwire",
                    &format!("Waiting for {name}. Audio is discarded until it is back."),
                    NIIF_WARNING,
                );
            }
            app.next_output_check = Instant::now() + WAITING_RECHECK;
            start_with(
                service::Backend::Discard {
                    reason: format!("waiting for \"{name}\""),
                },
                None,
                false,
            )
        }
        Plan::ChooseOutput => {
            twlog!(
                "starting receiver on UDP {} without output: no output chosen (the system default is never used)",
                protocol::DEFAULT_PORT
            );
            if !app.choose_notified {
                app.choose_notified = true;
                balloon(
                    app.hwnd,
                    "Tightwire: choose an output",
                    outsel::CHOOSE_OUTPUT_BALLOON,
                    NIIF_INFO,
                );
            }
            start_with(
                service::Backend::Discard {
                    reason: "no output chosen".into(),
                },
                None,
                false,
            )
        }
    };
    app.plan = plan;
    match started {
        Ok(r) => {
            twlog!("receiver running: {}", r.description);
            app.receiver = Some(r);
            app.retry_at = None;
        }
        Err(e) => {
            post(Notice::Error(format!(
                "receiver did not start: {e}; retrying in 10 s"
            )));
            app.retry_at = Some(Instant::now() + Duration::from_secs(10));
        }
    }
}

fn login_enabled() -> bool {
    let mut size = 0u32;
    // SAFETY: size query on a value under HKCU.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .is_ok()
    }
}

fn set_login(on: bool) {
    // SAFETY: writes/deletes our own value under HKCU\…\Run.
    let r = unsafe {
        if on {
            let exe = std::env::current_exe()
                .map(|p| format!("\"{}\"", p.display()))
                .unwrap_or_default();
            let data: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(data.as_ptr() as *const _),
                (data.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE)
        }
    };
    if r.is_err() {
        post(Notice::Error(format!(
            "could not change start at login: {r:?}"
        )));
    } else {
        twlog!("start at login {}", if on { "on" } else { "off" });
    }
}

fn ask(text: &str, title: &str) -> bool {
    // SAFETY: modal message box with no owner, on whatever thread asks.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn start_pairing(app: &mut App) {
    if app.pairing_busy {
        return;
    }
    app.pairing_busy = true;
    twlog!(
        "pairing started from the tray (UDP {})",
        protocol::PAIR_PORT
    );
    balloon(
        app.hwnd,
        "Pairing",
        "Open Tightwire on the Mac and pick this PC under Pair a receiver.",
        NIIF_INFO,
    );
    std::thread::spawn(|| {
        let result = (|| -> Result<String, String> {
            let path = pairing::default_config_path().ok_or("cannot find %APPDATA%")?;
            let mut config = ConfigFile::load(&path)?;
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = PairOptions {
                port: protocol::PAIR_PORT,
                bind: std::net::Ipv4Addr::UNSPECIFIED.into(),
                timeout: Duration::from_secs(120),
                name: pairing::host_name(),
                beacon: true,
            };
            let record = pairing::run_pairing(&opts, &config, rx, |ev| {
                if let PairEvent::ShowCode { code, peer_name } = ev {
                    let tx = tx.clone();
                    let (code, peer) = (code.clone(), peer_name.clone());
                    std::thread::spawn(move || {
                        let ok = ask(&pairing::code_question(&code, &peer), "Tightwire pairing");
                        let _ = tx.send(ok);
                    });
                }
            })?;
            let name = record.peer_name.clone();
            config.pairing = Some(record);
            config.save(&path)?;
            Ok(name)
        })();
        match result {
            Ok(name) => post(Notice::Info(format!("Paired with {name}"))),
            Err(e) => post(Notice::Error(format!("Pairing failed: {e}"))),
        }
        post(Notice::Repaired);
    });
}

fn send_deck(target: DeckTarget) {
    std::thread::spawn(move || {
        let r = pairing::default_config_path()
            .ok_or_else(|| "cannot find %APPDATA%".to_string())
            .and_then(|p| {
                deck::request(
                    &p,
                    target,
                    None,
                    protocol::HANDOFF_PORT,
                    Duration::from_secs(4),
                )
            });
        // deck::request already logged the request and the answer in full.
        match r {
            Ok(a) if a.accepted => {
                post(Notice::Info(format!(
                    "{} — the Mac accepted; watch for the handoff result.",
                    capitalise(deck::request_phrase(target))
                )));
                post(Notice::DeckAccepted(target));
            }
            Ok(a) => post(Notice::Error(format!("The Mac refused: {}", a.message))),
            Err(e) => post(Notice::Error(format!("Stream Deck request failed: {e}"))),
        }
    });
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

fn open_log() {
    if let Some(p) = applog::log_path() {
        // SAFETY: shell "open" on our own log file.
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                &HSTRING::from(p.as_os_str()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

fn paired_name() -> Option<String> {
    pairing::default_config_path()
        .and_then(|p| ConfigFile::load(&p).ok())
        .and_then(|c| c.pairing)
        .map(|p| p.peer_name)
}

/// Combined state for icon, tooltip and log: output problems first, then
/// the network phase.
fn tray_state(app: &App) -> &'static str {
    let Some(r) = &app.receiver else {
        return "stopped";
    };
    let net = service::phase(&r.stats, &r.net_status());
    if net == "unpaired" {
        return "unpaired";
    }
    match (&app.plan, &app.output_error) {
        (Plan::ChooseOutput, _) => "choose an output",
        (Plan::Waiting { .. }, _) => "waiting for output",
        (Plan::Open { .. }, Some(_)) => "output failed",
        (Plan::Open { .. }, None) => net,
    }
}

fn status_text(app: &App, paired: Option<&str>) -> String {
    let peer = paired.unwrap_or("the Mac");
    match tray_state(app) {
        "stopped" => "Receiver stopped (see log)".into(),
        "unpaired" => "Not paired".into(),
        "streaming" => format!(
            "Streaming from {peer} · {:.1} ms",
            app.receiver
                .as_ref()
                .map_or(0, |r| r.stats.depth_us.load(Ordering::Relaxed)) as f64
                / 1000.0
        ),
        "searching" => format!("Waiting for {peer}"),
        _ => outsel::plan_status(&app.plan),
    }
}

fn show_menu(app: &mut App) {
    refresh_endpoints(app);
    let paired = paired_name();
    let status = status_text(app, paired.as_deref());
    let output_line = match (&app.plan, &app.output_error) {
        (_, Some(e)) => format!("Output problem: {e}"),
        (p, None) => outsel::plan_status(p),
    };
    // SAFETY: menu built, shown and destroyed on the UI thread.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let grey = |text: String| {
            let t: String = text.replace('&', "&&").chars().take(120).collect();
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, ID_STATUS, &HSTRING::from(t));
        };
        grey(applog::build_label());
        grey(status);
        grey(output_line);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if let Ok(sub) = CreatePopupMenu() {
            // A saved choice that is not connected stays visible and checked.
            if let (Plan::Waiting { name }, Some(_)) = (&app.plan, &app.settings.output) {
                let _ = AppendMenuW(
                    sub,
                    MF_STRING | MF_GRAYED | MF_CHECKED,
                    ID_STATUS,
                    &HSTRING::from(format!("{} (not connected)", name.replace('&', "&&"))),
                );
            }
            for (i, e) in app.endpoints.iter().enumerate().take(MAX_DEVICES) {
                let flag = if app.settings.is_chosen(e) {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                let _ = AppendMenuW(
                    sub,
                    MF_STRING | flag,
                    ID_DEVICE_BASE + i,
                    &HSTRING::from(outsel::menu_label(e)),
                );
            }
            if app.endpoints.is_empty() {
                let _ = AppendMenuW(
                    sub,
                    MF_STRING | MF_GRAYED,
                    ID_STATUS,
                    w!("(no active outputs)"),
                );
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Output device"));
        }
        let (ex_flag, ex_on) = match &app.settings.output {
            Some(o) => {
                let on = app.settings.exclusive_for(&o.id, &o.name);
                (if on { MF_CHECKED } else { MF_UNCHECKED }, true)
            }
            None => (MF_UNCHECKED, false),
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING | ex_flag | if ex_on { MF_ENABLED } else { MF_GRAYED },
            ID_EXCLUSIVE,
            w!("Exclusive mode (lowest latency)"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let pair_label = if paired.is_some() {
            "Pair again…"
        } else {
            "Pair…"
        };
        let pf = if app.pairing_busy {
            MF_GRAYED
        } else {
            MF_ENABLED
        };
        let _ = AppendMenuW(menu, MF_STRING | pf, ID_PAIR, &HSTRING::from(pair_label));
        let df = if paired.is_some() {
            MF_ENABLED
        } else {
            MF_GRAYED
        };
        let _ = AppendMenuW(menu, MF_STRING | df, ID_DECK_MAC, w!("Stream Deck → Mac"));
        let _ = AppendMenuW(menu, MF_STRING | df, ID_DECK_PC, w!("Stream Deck → PC"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let lf = if login_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = AppendMenuW(menu, MF_STRING | lf, ID_LOGIN, w!("Start at login"));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_LOG, w!("Open log"));
        let _ = AppendMenuW(menu, MF_STRING | df, ID_FORGET, w!("Forget pairing"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(app.hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            app.hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

fn command(app: &mut App, id: usize) {
    match id {
        ID_PAIR => start_pairing(app),
        ID_DECK_MAC => {
            twlog!("tray: Stream Deck → Mac clicked");
            send_deck(DeckTarget::Mac)
        }
        ID_DECK_PC => {
            twlog!("tray: Stream Deck → PC clicked");
            send_deck(DeckTarget::Pc)
        }
        ID_LOGIN => set_login(!login_enabled()),
        ID_OPEN_LOG => open_log(),
        ID_FORGET => {
            if ask(
                "Forget the paired Mac? You will need to pair again.",
                "Tightwire",
            ) && let Some(p) = pairing::default_config_path()
            {
                match ConfigFile::load(&p).and_then(|mut c| {
                    c.pairing = None;
                    c.save(&p)
                }) {
                    Ok(()) => post(Notice::Info("Pairing forgotten".into())),
                    Err(e) => post(Notice::Error(e)),
                }
            }
        }
        ID_QUIT => {
            // SAFETY: ends the message loop.
            unsafe { DestroyWindow(app.hwnd).ok() };
        }
        ID_EXCLUSIVE => {
            if let Some(o) = app.settings.output.clone() {
                let on = !app.settings.exclusive_for(&o.id, &o.name);
                twlog!(
                    "exclusive mode {} for \"{}\" (id {})",
                    if on { "on" } else { "off" },
                    o.name,
                    o.id
                );
                app.settings.exclusive.insert(o.id.clone(), on);
                save_settings(app);
                apply_output(app);
            }
        }
        n if (ID_DEVICE_BASE..ID_DEVICE_BASE + MAX_DEVICES).contains(&n) => {
            if let Some(e) = app.endpoints.get(n - ID_DEVICE_BASE).cloned() {
                twlog!(
                    "output device chosen: \"{}\" (id {}{}), exclusive preference {}",
                    e.name,
                    e.id,
                    if e.is_default {
                        ", the system default"
                    } else {
                        ""
                    },
                    if app.settings.exclusive_for(&e.id, &e.name) {
                        "on"
                    } else {
                        "off"
                    }
                );
                app.settings.choose(&e);
                save_settings(app);
                apply_output(app);
            }
        }
        _ => {}
    }
}

fn write_status(app: &App, state: &str) {
    let Some(path) = runstatus::default_path() else {
        return;
    };
    let (output, paired_with, peer, last_auth) = match &app.receiver {
        Some(r) => {
            let n = r.net_status();
            (
                r.description.clone(),
                n.paired_with.unwrap_or_default(),
                n.peer.map(|p| p.to_string()).unwrap_or_default(),
                n.last_auth
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
            )
        }
        None => Default::default(),
    };
    let s = RuntimeStatus {
        pid: std::process::id(),
        build: applog::build_label(),
        written_unix: runstatus::unix_now(),
        phase: state.to_owned(),
        output,
        paired_with,
        peer,
        last_auth_unix: last_auth,
        last_handoff: app.last_handoff.clone(),
    };
    if let Err(e) = s.save(&path) {
        twlog!("could not write {}: {e}", path.display());
    }
}

fn tick(app: &mut App) {
    let now = Instant::now();
    if app.receiver.is_none() && app.retry_at.is_some_and(|t| now >= t) {
        refresh_endpoints(app);
        apply_output(app);
    }
    if let Some(e) = app.receiver.as_ref().and_then(Receiver::failed) {
        post(Notice::Error(format!("audio output stopped: {e}")));
        refresh_endpoints(app);
        apply_output(app);
    }
    // Missing or failed output: look again (sooner after a device change).
    let recheck = matches!(app.plan, Plan::Waiting { .. }) || app.output_error.is_some();
    if recheck && now >= app.next_output_check {
        refresh_endpoints(app);
        let next = outsel::plan(&app.settings, &app.endpoints);
        if next != app.plan || app.output_error.is_some() {
            apply_output(app);
        } else {
            app.next_output_check = now + WAITING_RECHECK;
        }
    }

    if let Some(r) = &app.receiver {
        for ev in r.take_handoff_events() {
            app.deck_watch = None;
            app.last_handoff = ev.text.clone();
            if ev.ok {
                balloon(app.hwnd, "Stream Deck", &ev.text, NIIF_INFO);
            } else {
                balloon(app.hwnd, "Stream Deck handoff failed", &ev.text, NIIF_ERROR);
            }
        }
        // Log the resampling ratio once per stream format.
        if matches!(app.plan, Plan::Open { .. }) && app.output_error.is_none() {
            let sr = r.stats.stream_rate.load(Ordering::Relaxed);
            let pair = (sr, r.format.rate);
            if sr != 0 && app.resample_logged != Some(pair) {
                app.resample_logged = Some(pair);
                match outsel::resample_note(sr, r.format.rate) {
                    Some(n) => twlog!("{n}"),
                    None => twlog!("stream and output both at {sr} Hz; no resampling"),
                }
            }
        }
    }
    if let Some((t, target)) = app.deck_watch
        && now.duration_since(t) > DECK_WATCH
    {
        app.deck_watch = None;
        twlog!(
            "deck: the Mac accepted \"{}\" {} s ago but no handoff offer or release reached this PC; \
             if the devices were not already there, check the Mac's Tightwire log (Longcable share step) \
             and that UDP 4810 from the Mac is allowed here",
            deck::request_phrase(target),
            DECK_WATCH.as_secs()
        );
    }

    let paired = paired_name();
    let state = tray_state(app);
    let tip = format!("Tightwire: {}", status_text(app, paired.as_deref()));
    if state != app.last_state {
        twlog!("state: {state} ({})", status_text(app, paired.as_deref()));
        app.last_state = state.to_owned();
        let icon = make_icon(state_color(state));
        set_tip_and_icon(app, &tip, icon);
    } else {
        let mut d = tray_data(app.hwnd);
        d.uFlags = NIF_TIP;
        wide(&tip, &mut d.szTip);
        // SAFETY: valid NOTIFYICONDATAW.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }
    if now >= app.next_status_write {
        app.next_status_write = now + Duration::from_secs(5);
        write_status(app, state);
    }
}

fn drain_notices(app: &mut App) {
    let list: Vec<Notice> = NOTICES
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    for n in list {
        match n {
            Notice::Info(m) => balloon(app.hwnd, "Tightwire", &m, NIIF_INFO),
            Notice::Error(m) => balloon(app.hwnd, "Tightwire problem", &m, NIIF_ERROR),
            Notice::Repaired => app.pairing_busy = false,
            Notice::DeckAccepted(t) => app.deck_watch = Some((Instant::now(), t)),
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = APP.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(app) = guard.as_mut() else {
            return false;
        };
        match msg {
            WM_APP_TRAY => {
                let ev = (lp.0 & 0xFFFF) as u32;
                if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP || ev == WM_CONTEXTMENU {
                    show_menu(app);
                }
                true
            }
            WM_COMMAND => {
                command(app, wp.0 & 0xFFFF);
                true
            }
            WM_TIMER => {
                tick(app);
                true
            }
            WM_APP_WAKE => {
                drain_notices(app);
                true
            }
            WM_DEVICECHANGE => {
                // Hardware changed (USB audio plugged in…): if we are waiting
                // for the chosen output, look again shortly. Not every audio
                // endpoint change produces this message; the 5 s recheck
                // covers the rest. DefWindowProc still answers it.
                let soon = Instant::now() + Duration::from_millis(1500);
                if soon < app.next_output_check {
                    app.next_output_check = soon;
                }
                false
            }
            WM_DESTROY => {
                twlog!("quitting");
                stop_receiver(app);
                let d = tray_data(hwnd);
                // SAFETY: removes our icon and ends the loop.
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &d);
                    PostQuitMessage(0);
                }
                true
            }
            _ => false,
        }
    });
    if handled {
        LRESULT(0)
    } else {
        // SAFETY: default handling for everything else.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }
}

/// Entry point for `tightwire.exe` with no arguments.
pub fn run() {
    let log = applog::default_log_path();
    if let Err(e) = applog::to_file(&log) {
        // Nowhere to log yet; surface it the only way left.
        ask(
            &format!(
                "Cannot write the log at {}: {e}\n\nContinue anyway?",
                log.display()
            ),
            "Tightwire",
        );
    }
    twlog!("{}", applog::start_header("tray", std::process::id()));
    twlog!(
        "pairing file: {}",
        pairing::default_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no %APPDATA%)".into())
    );
    let settings_path = outsel::tray_settings_path();
    let settings = match settings_path.as_deref().map(TraySettings::load) {
        Some(Ok((s, warnings))) => {
            for w in warnings {
                twlog!("settings warning: {w}");
            }
            s
        }
        Some(Err(e)) => {
            twlog!("could not read settings ({e}); starting with no output chosen");
            TraySettings::default()
        }
        None => TraySettings::default(),
    };
    twlog!(
        "settings: {}; saved output: {}",
        settings_path
            .as_deref()
            .map_or("(none)".into(), |p| p.display().to_string()),
        settings
            .output
            .as_ref()
            .map_or("none".into(), |o| format!("\"{}\" (id {})", o.name, o.id))
    );

    // SAFETY: standard window-class registration and message loop on this thread.
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            twlog!("GetModuleHandleW failed");
            return;
        };
        let class = w!("TightwireTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            twlog!("RegisterClassW failed");
            return;
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Tightwire"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                twlog!("CreateWindowExW failed: {e}");
                return;
            }
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);

        let icon = make_icon(state_color("unpaired"));
        let mut d = tray_data(hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.hIcon = icon;
        wide("Tightwire: starting", &mut d.szTip);
        if !Shell_NotifyIconW(NIM_ADD, &d).as_bool() {
            twlog!("Shell_NotifyIconW(NIM_ADD) failed");
        }

        let mut app = App {
            hwnd,
            receiver: None,
            settings_path,
            settings,
            endpoints: vec![],
            plan: Plan::ChooseOutput,
            output_error: None,
            next_output_check: Instant::now(),
            retry_at: None,
            pairing_busy: false,
            choose_notified: false,
            last_state: String::new(),
            resample_logged: None,
            deck_watch: None,
            last_handoff: String::new(),
            next_status_write: Instant::now(),
            icon,
        };
        refresh_endpoints(&mut app);
        log_endpoints(&app);
        apply_output(&mut app);
        if paired_name().is_none() {
            post(Notice::Info(
                "Not paired yet: right-click the tray icon › Pair…".into(),
            ));
        }
        APP.with(|c| *c.borrow_mut() = Some(app));
        SetTimer(Some(hwnd), TIMER_ID, 1000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
