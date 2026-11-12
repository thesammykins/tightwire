//! WASAPI backend (Windows only). Exclusive mode at the device's minimum
//! period when allowed; otherwise shared mode at the smallest engine period
//! (IAudioClient3), which is the expected path for virtual endpoints such as
//! Wave Link's that refuse exclusive mode. Event-driven in both modes.
//!
//! UNTESTED ON HARDWARE: this file is compiled and linted for
//! x86_64-pc-windows-msvc on macOS but has never run (SPEC §7.6).
//!
//! COM objects stay on the render thread that created them: `open` starts that
//! thread, which negotiates the format, reports it back, then waits for the
//! engine and runs the render loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};
use windows::core::{HSTRING, Interface, w};

use super::{Output, OutputFormat, Result, Running};
use crate::clock;
use crate::engine::Engine;
use crate::outsel::Endpoint;
use crate::protocol::OutputMode;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
    /// 32-bit container, `valid` significant bits (24 or 32), MSB-aligned.
    I32,
}

#[derive(Debug, Clone, Copy)]
struct DeviceFormat {
    rate: u32,
    channels: usize,
    kind: SampleKind,
    bits: u16,
}

fn com_init() -> Result<()> {
    // SAFETY: initialises COM for this thread; paired with CoUninitialize by the caller.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    Ok(())
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM activation of the system device enumerator.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

fn friendly_name(dev: &IMMDevice) -> String {
    // SAFETY: read-only property store access; the PROPVARIANT is owned and dropped here.
    unsafe {
        dev.OpenPropertyStore(STGM_READ)
            .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
            .and_then(|v| PropVariantToBSTR(&v))
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "?".into())
    }
}

fn render_devices() -> Result<Vec<(IMMDevice, String)>> {
    let e = enumerator()?;
    // SAFETY: enumerating active render endpoints; indices are within GetCount.
    unsafe {
        let coll = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut out = vec![];
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let name = friendly_name(&d);
            out.push((d, name));
        }
        Ok(out)
    }
}

/// Copies the device mix format out of COM memory.
fn mix_format(client: &IAudioClient) -> Result<(WAVEFORMATEXTENSIBLE, DeviceFormat)> {
    // SAFETY: GetMixFormat returns a CoTaskMem block holding a WAVEFORMATEX, or a
    // WAVEFORMATEXTENSIBLE when wFormatTag says so; we copy it before freeing.
    unsafe {
        let p = client.GetMixFormat()?;
        let base = std::ptr::read_unaligned(p);
        let mut ext = WAVEFORMATEXTENSIBLE {
            Format: base,
            ..Default::default()
        };
        if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            ext = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
        }
        CoTaskMemFree(Some(p as *const _));
        let fmt = describe_format(&ext).ok_or("device mix format is not PCM or float")?;
        Ok((ext, fmt))
    }
}

fn describe_format(ext: &WAVEFORMATEXTENSIBLE) -> Option<DeviceFormat> {
    let f = ext.Format;
    let (is_float, is_pcm) = if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
        let sub = ext.SubFormat;
        (
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            sub == KSDATAFORMAT_SUBTYPE_PCM,
        )
    } else {
        (
            f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT,
            f.wFormatTag == WAVE_FORMAT_PCM,
        )
    };
    let kind = match (is_float, is_pcm, f.wBitsPerSample) {
        (true, _, 32) => SampleKind::F32,
        (_, true, 16) => SampleKind::I16,
        (_, true, 32) => SampleKind::I32,
        _ => return None,
    };
    Some(DeviceFormat {
        rate: f.nSamplesPerSec,
        channels: f.nChannels as usize,
        kind,
        bits: f.wBitsPerSample,
    })
}

fn make_format(
    rate: u32,
    channels: usize,
    kind: SampleKind,
    valid_bits: u16,
) -> WAVEFORMATEXTENSIBLE {
    let container: u16 = if kind == SampleKind::I16 { 16 } else { 32 };
    let block = channels as u16 * container / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: channels as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: container,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: match channels {
            1 => 0x4,
            2 => 0x3,
            n => (1u32 << n) - 1,
        },
        SubFormat: if kind == SampleKind::F32 {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
    }
}

fn device_id(d: &IMMDevice) -> Option<String> {
    // SAFETY: GetId returns a CoTaskMem string that we copy and free.
    unsafe {
        d.GetId().ok().and_then(|p| {
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        })
    }
}

fn default_render_id(e: &IMMDeviceEnumerator) -> Option<String> {
    // SAFETY: default endpoint query; no ownership beyond the returned COM pointer.
    unsafe { e.GetDefaultAudioEndpoint(eRender, eConsole) }
        .ok()
        .and_then(|d| device_id(&d))
}

fn endpoints_on_this_thread() -> Result<Vec<(IMMDevice, Endpoint)>> {
    let e = enumerator()?;
    let default_id = default_render_id(&e);
    Ok(render_devices()?
        .into_iter()
        .map(|(d, name)| {
            let id = device_id(&d).unwrap_or_default();
            let is_default = !id.is_empty() && Some(&id) == default_id.as_ref();
            (
                d,
                Endpoint {
                    id,
                    name,
                    is_default,
                },
            )
        })
        .collect())
}

/// Active render endpoints with ids and the system-default flag (tray
/// picker, doctor). Opens nothing.
pub fn render_endpoints() -> Result<Vec<Endpoint>> {
    com_init()?;
    let r = endpoints_on_this_thread().map(|v| v.into_iter().map(|(_, e)| e).collect());
    // SAFETY: balances com_init above.
    unsafe { CoUninitialize() };
    r
}

/// `--list-devices` / doctor lines: name, markers, mix format and periods,
/// and the endpoint id. Reads formats only; nothing is opened for playback.
pub fn list_devices() -> Result<Vec<String>> {
    com_init()?;
    let result = (|| {
        let mut out = vec![];
        for (d, ep) in endpoints_on_this_thread()? {
            // SAFETY: activation of a client just to read its formats; released on drop.
            let detail = unsafe { d.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .map_err(|e| e.to_string())
                .and_then(|c| {
                    let (ext, f) = mix_format(&c).map_err(|e| e.to_string())?;
                    let mut def = 0i64;
                    let mut min = 0i64;
                    // SAFETY: out-pointers to locals.
                    unsafe { c.GetDevicePeriod(Some(&mut def), Some(&mut min)) }
                        .map_err(|e| e.to_string())?;
                    // GetDevicePeriod's minimum is the *exclusive-mode*
                    // minimum; shared mode's range comes from IAudioClient3.
                    let shared = match shared_periods(&c, &ext) {
                        Some(p) => p.describe(f.rate),
                        None => "shared engine period range unavailable (no IAudioClient3)".into(),
                    };
                    Ok(format!(
                        "{} Hz, {} ch, {:?} {}-bit; shared default period {:.2} ms; exclusive min period {:.2} ms; {shared}",
                        f.rate,
                        f.channels,
                        f.kind,
                        f.bits,
                        def as f64 / 10_000.0,
                        min as f64 / 10_000.0
                    ))
                })
                .unwrap_or_else(|e| format!("({e})"));
            let mark = if ep.is_default {
                "  — system default, not recommended"
            } else {
                ""
            };
            out.push(format!(
                "{}{mark}\n    [{detail}]\n    id {}",
                ep.name, ep.id
            ));
        }
        Ok(out)
    })();
    // SAFETY: balances com_init above.
    unsafe { CoUninitialize() };
    result
}

/// How the caller names the endpoint. There is deliberately no "default"
/// variant: Tightwire never opens an endpoint the user did not pick.
#[derive(Debug, Clone)]
pub enum DeviceSel {
    /// Endpoint id saved by the tray.
    Id(String),
    /// CLI `--device`: case-insensitive substring of the friendly name.
    Name(String),
}

fn pick_device(sel: &DeviceSel) -> Result<(IMMDevice, String)> {
    match sel {
        DeviceSel::Id(id) => {
            let e = enumerator()?;
            // SAFETY: lookup by id string; state checked before use.
            let d = unsafe { e.GetDevice(&HSTRING::from(id.as_str())) }
                .map_err(|err| format!("output endpoint {id} not found ({err})"))?;
            // SAFETY: plain state query on a valid device.
            let state = unsafe { d.GetState() }?;
            if state != DEVICE_STATE_ACTIVE {
                return Err(format!(
                    "output \"{}\" is not active (state {:#x})",
                    friendly_name(&d),
                    state.0
                )
                .into());
            }
            let n = friendly_name(&d);
            Ok((d, n))
        }
        DeviceSel::Name(w) => {
            let wl = w.to_lowercase();
            render_devices()?
                .into_iter()
                .find(|(_, n)| n.to_lowercase().contains(&wl))
                .ok_or_else(|| {
                    format!("no output device matching \"{w}\" (see --list-devices)").into()
                })
        }
    }
}

struct Negotiated {
    client: IAudioClient,
    format: DeviceFormat,
    mode: String,
    output_mode: OutputMode,
    period_frames: usize,
    buffer_frames: u32,
    exclusive: bool,
}

/// 100-ns units for `frames` at `rate`, rounded to nearest.
fn hns(frames: u32, rate: u32) -> i64 {
    ((10_000_000.0 * frames as f64 / rate as f64) + 0.5) as i64
}

fn try_exclusive(dev: &IMMDevice, mix: &DeviceFormat) -> Result<Negotiated> {
    // SAFETY: COM calls on objects owned by this thread; format pointers
    // reference locals that outlive each call.
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let candidates = [
            (SampleKind::F32, 32),
            (SampleKind::I32, 32),
            (SampleKind::I32, 24),
            (SampleKind::I16, 16),
        ];
        let (fmt, kind) = candidates
            .iter()
            .map(|&(k, v)| (make_format(mix.rate, mix.channels, k, v), k))
            .find(|(f, _)| {
                client
                    .IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &f.Format, None)
                    .is_ok()
            })
            .ok_or("device accepts none of f32/s32/s24-in-32/s16 in exclusive mode")?;
        let mut min = 0i64;
        client.GetDevicePeriod(None, Some(&mut min))?;
        let init = |c: &IAudioClient, period: i64| {
            c.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                period,
                &fmt.Format,
                None,
            )
        };
        let client = match init(&client, min) {
            Ok(()) => client,
            Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
                // Documented recovery: use the aligned frame count the driver
                // reports, on a fresh client.
                let frames = client.GetBufferSize()?;
                let c2: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
                init(&c2, hns(frames, mix.rate))?;
                c2
            }
            Err(e) => return Err(e.into()),
        };
        let buffer_frames = client.GetBufferSize()?;
        let format = DeviceFormat {
            kind,
            bits: fmt.Format.wBitsPerSample,
            ..*mix
        };
        Ok(Negotiated {
            client,
            format,
            mode: format!(
                "exclusive, {buffer_frames} frames ({:.2} ms)",
                buffer_frames as f64 * 1000.0 / mix.rate as f64
            ),
            output_mode: OutputMode::WasapiExclusive,
            period_frames: buffer_frames as usize,
            buffer_frames,
            exclusive: true,
        })
    }
}

/// IAudioClient3::GetSharedModeEnginePeriod for `fmt`, in frames.
#[derive(Debug, Clone, Copy)]
struct SharedPeriods {
    default: u32,
    fundamental: u32,
    min: u32,
    max: u32,
}

impl SharedPeriods {
    fn describe(&self, rate: u32) -> String {
        let ms = |f: u32| f as f64 * 1000.0 / rate as f64;
        let mut s = format!(
            "shared engine periods (IAudioClient3): default {} / min {} / max {} frames, step {} ({:.2} / {:.2} / {:.2} ms)",
            self.default,
            self.min,
            self.max,
            self.fundamental,
            ms(self.default),
            ms(self.min),
            ms(self.max)
        );
        if self.min >= self.default {
            s += " — the driver offers no shorter shared period; only exclusive mode goes lower";
        }
        s
    }
}

fn shared_periods(client: &IAudioClient, fmt: &WAVEFORMATEXTENSIBLE) -> Option<SharedPeriods> {
    let c3 = client.cast::<IAudioClient3>().ok()?;
    let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: format points at a local copy; out-pointers to locals.
    unsafe {
        c3.GetSharedModeEnginePeriod(
            &fmt.Format,
            &mut default,
            &mut fundamental,
            &mut min,
            &mut max,
        )
    }
    .ok()?;
    Some(SharedPeriods {
        default,
        fundamental,
        min,
        max,
    })
}

/// The engine period actually in force after initialisation. Another client
/// already streaming to the endpoint can hold the engine at its own period
/// (Microsoft: "Low Latency Audio", AUDCLNT_E_ENGINE_PERIODICITY_LOCKED).
fn current_engine_period(client: &IAudioClient) -> Option<u32> {
    let c3 = client.cast::<IAudioClient3>().ok()?;
    let mut fmt: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut frames = 0u32;
    // SAFETY: the returned format is CoTaskMem-allocated and freed here.
    unsafe {
        c3.GetCurrentSharedModeEnginePeriod(&mut fmt, &mut frames)
            .ok()?;
        if !fmt.is_null() {
            CoTaskMemFree(Some(fmt as *const _));
        }
    }
    Some(frames)
}

/// Shared mode at the smallest engine period the driver offers. IAudioClient3
/// periods must be a multiple of the fundamental period within [min, max]
/// (IAudioClient3::InitializeSharedAudioStream docs); `min` always is. A
/// driver that does not declare small buffers reports min = default (480
/// frames at 48 kHz = 10 ms), and then nothing below 10 ms exists in shared
/// mode — only exclusive mode reaches GetDevicePeriod's minimum.
fn shared(dev: &IMMDevice) -> Result<Negotiated> {
    // SAFETY: as in try_exclusive; the mix format copy lives on this stack.
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let (mixext, mix) = mix_format(&client)?;
        match shared_periods(&client, &mixext) {
            Some(p) => {
                crate::twlog!("wasapi: {}", p.describe(mix.rate));
                let c3: IAudioClient3 = client.cast()?;
                match c3.InitializeSharedAudioStream(
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    p.min,
                    &mixext.Format,
                    None,
                ) {
                    Ok(()) => {
                        let buffer_frames = client.GetBufferSize()?;
                        let current = current_engine_period(&client).unwrap_or(p.min);
                        let mut mode = format!(
                            "shared (IAudioClient3), engine period {current} frames ({:.2} ms; requested the driver's minimum {}, default {})",
                            current as f64 * 1000.0 / mix.rate as f64,
                            p.min,
                            p.default
                        );
                        if current != p.min {
                            mode += "; another app holds the engine at its own period";
                        } else if p.min >= p.default {
                            mode += "; the driver offers nothing shorter in shared mode";
                        }
                        return Ok(Negotiated {
                            client,
                            format: mix,
                            mode,
                            output_mode: OutputMode::WasapiSharedLowLatency,
                            period_frames: current as usize,
                            buffer_frames,
                            exclusive: false,
                        });
                    }
                    Err(e) => crate::twlog!(
                        "wasapi: InitializeSharedAudioStream({} frames) failed ({e}); using the default shared period",
                        p.min
                    ),
                }
            }
            None => crate::twlog!(
                "wasapi: IAudioClient3 not available; using the default shared period"
            ),
        }
        // Classic shared mode: the engine period is the device default (~10 ms).
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            &mixext.Format,
            None,
        )?;
        let mut def = 0i64;
        client.GetDevicePeriod(Some(&mut def), None)?;
        let period = (def as f64 * mix.rate as f64 / 10_000_000.0).round() as usize;
        let buffer_frames = client.GetBufferSize()?;
        Ok(Negotiated {
            client,
            format: mix,
            mode: format!("shared, default engine period {period} frames"),
            output_mode: OutputMode::WasapiShared,
            period_frames: period,
            buffer_frames,
            exclusive: false,
        })
    }
}

fn negotiate(sel: &DeviceSel, exclusive: bool) -> Result<(Negotiated, String)> {
    let (dev, name) = pick_device(sel)?;
    if exclusive {
        // SAFETY: activation to read the mix format for exclusive-mode candidates.
        let probe: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
        let (_, mix) = mix_format(&probe)?;
        drop(probe);
        match try_exclusive(&dev, &mix) {
            Ok(n) => return Ok((n, name)),
            Err(e) => crate::twlog!(
                "exclusive mode refused by \"{name}\" ({e}); using shared mode instead"
            ),
        }
    }
    Ok((shared(&dev)?, name))
}

pub struct WasapiOutput {
    format: OutputFormat,
    describe: String,
    engine_tx: mpsc::Sender<Option<Engine>>,
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
}

impl WasapiOutput {
    /// Opens `sel`; `exclusive` tries exclusive mode first and falls back to
    /// shared mode (logged) when the endpoint refuses it.
    pub fn open(sel: DeviceSel, exclusive: bool) -> Result<Self> {
        let (fmt_tx, fmt_rx) =
            mpsc::channel::<std::result::Result<(OutputFormat, String), String>>();
        let (engine_tx, engine_rx) = mpsc::channel::<Option<Engine>>();
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(Mutex::new(None));
        let (stop2, failed2) = (stop.clone(), failed.clone());
        let thread = std::thread::Builder::new()
            .name("tightwire-wasapi".into())
            .spawn(move || {
                if let Err(e) = com_init() {
                    let _ = fmt_tx.send(Err(e.to_string()));
                    return;
                }
                let r = render_thread(&sel, exclusive, &fmt_tx, &engine_rx, &stop2);
                if let Err(e) = r {
                    let msg = e.to_string();
                    let _ = fmt_tx.send(Err(msg.clone()));
                    if let Ok(mut g) = failed2.lock() {
                        *g = Some(msg);
                    }
                }
                // SAFETY: balances com_init on this thread.
                unsafe { CoUninitialize() };
            })?;
        let (format, describe) = fmt_rx.recv().map_err(|_| "WASAPI thread exited")??;
        Ok(Self {
            format,
            describe,
            engine_tx,
            thread,
            stop,
            failed,
        })
    }
}

fn render_thread(
    sel: &DeviceSel,
    exclusive: bool,
    fmt_tx: &mpsc::Sender<std::result::Result<(OutputFormat, String), String>>,
    engine_rx: &mpsc::Receiver<Option<Engine>>,
    stop: &AtomicBool,
) -> Result<()> {
    let (n, name) = negotiate(sel, exclusive)?;
    let f = n.format;
    // SAFETY: event handle and render client belong to this thread's client.
    let (event, render): (HANDLE, IAudioRenderClient) = unsafe {
        let ev = CreateEventW(None, false, false, None)?;
        n.client.SetEventHandle(ev)?;
        (ev, n.client.GetService()?)
    };
    let out = OutputFormat {
        rate: f.rate,
        channels: f.channels,
        period_frames: n.period_frames,
        mode: n.output_mode,
    };
    let _ = fmt_tx.send(Ok((
        out,
        format!(
            "wasapi \"{name}\" ({} Hz, {} ch, {:?} {}-bit, {})",
            f.rate, f.channels, f.kind, f.bits, n.mode
        ),
    )));
    let Ok(Some(mut engine)) = engine_rx.recv() else {
        return Ok(()); // dropped without starting
    };

    // SAFETY: registers this thread with MMCSS; the task handle is released at
    // thread exit by the OS.
    let _mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut 0) };
    let ch = f.channels;
    let buffer = n.buffer_frames;
    // Preallocated so the loop never allocates.
    let mut scratch = vec![0f32; buffer as usize * ch];

    // SAFETY (whole block): GetBuffer returns a device buffer of exactly
    // `frames * nBlockAlign` bytes valid until ReleaseBuffer; we write within it.
    unsafe {
        if n.exclusive {
            // Exclusive event mode must start with a full buffer queued.
            let p = render.GetBuffer(buffer)?;
            std::ptr::write_bytes(p, 0, buffer as usize * ch * (f.bits as usize / 8));
            render.ReleaseBuffer(buffer, 0)?;
        }
        n.client.Start()?;
        let mut timeouts = 0;
        while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                timeouts += 1;
                if timeouts >= 10 {
                    return Err("device stopped signalling (2 s without a buffer event)".into());
                }
                continue;
            }
            timeouts = 0;
            let frames = if n.exclusive {
                buffer
            } else {
                buffer - n.client.GetCurrentPadding()?
            };
            if frames == 0 {
                continue;
            }
            let samples = frames as usize * ch;
            engine.render(&mut scratch[..samples], clock::now_s());
            let p = render.GetBuffer(frames)?;
            match f.kind {
                SampleKind::F32 => {
                    std::ptr::copy_nonoverlapping(scratch.as_ptr(), p as *mut f32, samples)
                }
                SampleKind::I16 => {
                    let d = std::slice::from_raw_parts_mut(p as *mut i16, samples);
                    for (o, s) in d.iter_mut().zip(&scratch[..samples]) {
                        *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                }
                SampleKind::I32 => {
                    let d = std::slice::from_raw_parts_mut(p as *mut i32, samples);
                    for (o, s) in d.iter_mut().zip(&scratch[..samples]) {
                        *o = (s.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32;
                    }
                }
            }
            render.ReleaseBuffer(frames, 0)?;
        }
        n.client.Stop()?;
    }
    Ok(())
}

impl Output for WasapiOutput {
    fn format(&self) -> OutputFormat {
        self.format
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }

    fn start(self: Box<Self>, engine: Engine) -> Result<Box<dyn Running>> {
        self.engine_tx
            .send(Some(engine))
            .map_err(|_| "WASAPI thread exited")?;
        Ok(Box::new(WasapiRunning {
            stop: self.stop,
            failed: self.failed,
            thread: self.thread,
        }))
    }
}

struct WasapiRunning {
    stop: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
    thread: JoinHandle<()>,
}

impl Running for WasapiRunning {
    fn failed(&self) -> Option<String> {
        if self.thread.is_finished() && !self.stop.load(Ordering::Relaxed) {
            return Some(
                self.failed
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_else(|| "render thread ended".into()),
            );
        }
        None
    }

    fn stop(self: Box<Self>) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().map_err(|_| "WASAPI thread panicked")?;
        Ok(())
    }
}
