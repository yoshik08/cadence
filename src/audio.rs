//! Cadence's audio output: a librespot `Sink` on WASAPI shared mode.
//!
//! Why not librespot's rodio backend: rodio 0.21 / cpal 0.16 (what librespot 0.8.0 pins) bind the
//! stream to the physical device that was the default at creation, and librespot builds its sink
//! once. Bluetooth headphones connected after launch were therefore ignored.
//!
//! Here a render thread owns the WASAPI client and reopens it whenever the wanted endpoint
//! changes: the Windows default (checked every 500 ms) when "System Default" is selected, or the
//! chosen device (falling back to the default while it is missing). Device loss
//! (`AUDCLNT_E_DEVICE_INVALIDATED`) reopens on the new default. Windows resamples 44.1 kHz to the
//! device rate (`AUTOCONVERTPCM` + `SRC_DEFAULT_QUALITY`).
//!
//! Memory: a 0.3 s ring buffer (~106 KB) between librespot's player thread and the render thread.

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::logln;

const RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
/// Ring capacity in samples (0.3 s).
const CAP: usize = (RATE as usize * CHANNELS as usize * 3) / 10;
/// WASAPI buffer, in 100 ns units (50 ms).
const BUFFER_HNS: i64 = 500_000;

struct State {
    ring: VecDeque<f32>,
    /// librespot wants sound (between `start` and `stop`).
    running: bool,
    /// Endpoint ID the user picked; `None` = follow the Windows default.
    selected: Option<String>,
    /// A device is open and consuming the ring.
    open: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when the ring has room or `running` changes.
    cv: Condvar,
}

fn shared() -> &'static Arc<Shared> {
    static S: OnceLock<Arc<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        let s = Arc::new(Shared {
            state: Mutex::new(State { ring: VecDeque::with_capacity(CAP), running: false, selected: None, open: false }),
            cv: Condvar::new(),
        });
        let t = s.clone();
        let _ = std::thread::Builder::new().name("cadence-audio".into()).spawn(move || render_thread(&t));
        s
    })
}

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.state.lock().unwrap_or_else(|e| e.into_inner())
}

#[allow(dead_code)] // Settings > Output device (CHECKLIST 3.12).
/// Bluetooth output changes, for "pause when Bluetooth disconnects" / "resume on reconnect".
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceEvent {
    BluetoothLost,
    BluetoothBack,
}

static HOOK: Mutex<Option<Box<dyn Fn(DeviceEvent) + Send>>> = Mutex::new(None);

pub fn on_device_event(f: impl Fn(DeviceEvent) + Send + 'static) {
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

fn fire(ev: DeviceEvent) {
    logln!("audio: {ev:?}");
    if let Some(f) = HOOK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        f(ev);
    }
}

pub fn set_speed(speed: f32) {
    crate::dsp::set_speed(speed);
}

/// The device enumerator name ("BTHENUM" / "BTHLEDevice" for Bluetooth).
unsafe fn is_bluetooth(dev: &IMMDevice) -> bool {
    const PKEY_DEVICE_ENUMERATOR_NAME: windows::Win32::Foundation::PROPERTYKEY = windows::Win32::Foundation::PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 24,
    };
    unsafe {
        let Ok(store) = dev.OpenPropertyStore(STGM_READ) else { return false };
        let Ok(v) = store.GetValue(&PKEY_DEVICE_ENUMERATOR_NAME) else { return false };
        v.to_string().to_ascii_uppercase().starts_with("BTH")
    }
}

/// Picks the output: `None` = System Default (follows Windows), else an endpoint ID from `devices()`.
pub fn set_device(id: Option<String>) {
    let s = shared();
    lock(s).selected = id;
    s.cv.notify_all();
}

/// Active output endpoints as (ID, friendly name), the Windows default first.
pub fn devices() -> Vec<(String, String)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let default = default_id(&en);
        let mut out = Vec::new();
        if let Ok(list) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(dev) = list.Item(i) else { continue };
                let Some(id) = device_id(&dev) else { continue };
                out.push((id, friendly_name(&dev).unwrap_or_else(|| "Unknown device".into())));
            }
        }
        out.sort_by_key(|(id, _)| Some(id) != default.as_ref());
        out
    }
}

unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let p = dev.GetId().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

#[allow(dead_code)]
unsafe fn default_id(en: &IMMDeviceEnumerator) -> Option<String> {
    unsafe { device_id(&en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?) }
}

unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let v = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let s = v.to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// The endpoint to use now: the selected one if active, else the default.
unsafe fn wanted(en: &IMMDeviceEnumerator, selected: &Option<String>) -> Option<(String, IMMDevice)> {
    unsafe {
        if let Some(id) = selected
            && let Ok(dev) = en.GetDevice(&windows::core::HSTRING::from(id.as_str()))
            && dev.GetState().is_ok_and(|s| s == DEVICE_STATE_ACTIVE)
        {
            return Some((id.clone(), dev));
        }
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        Some((device_id(&dev)?, dev))
    }
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
    event: HANDLE,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

unsafe fn open(dev: &IMMDevice) -> windows::core::Result<Stream> {
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let block = CHANNELS * 4;
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_HNS,
            0,
            &fmt,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        Ok(Stream { client, render, frames, event })
    }
}

/// Copies from the ring into the device buffer, padding with silence. Err = device gone.
unsafe fn fill(st: &Stream, s: &Shared) -> windows::core::Result<()> {
    unsafe {
        let padding = st.client.GetCurrentPadding()?;
        let n = st.frames.saturating_sub(padding);
        if n == 0 {
            return Ok(());
        }
        let buf = st.render.GetBuffer(n)? as *mut f32;
        let out = std::slice::from_raw_parts_mut(buf, n as usize * CHANNELS as usize);
        {
            let mut g = lock(s);
            let take = out.len().min(g.ring.len());
            for (o, v) in out.iter_mut().zip(g.ring.drain(..take)) {
                *o = v;
            }
            out[take..].fill(0.0);
        }
        s.cv.notify_all();
        st.render.ReleaseBuffer(n, 0)
    }
}

fn render_thread(s: &Shared) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(e) => {
                logln!("audio: no device enumerator: {e}");
                return;
            }
        };
        let mut current: Option<(String, Stream)> = None;
        // A Bluetooth device that went away (to notice it coming back).
        let mut lost_bt: Option<String> = None;
        let mut current_bt = false;
        let mut last_check = Instant::now() - Duration::from_secs(1);
        loop {
            // Idle while librespot is paused: stop the client so the device can sleep.
            {
                let mut g = lock(s);
                if !g.running {
                    if let Some((_, st)) = &current {
                        let _ = st.client.Stop();
                    }
                    while !g.running {
                        g = s.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                    last_check = Instant::now() - Duration::from_secs(1);
                }
            }
            if last_check.elapsed() >= Duration::from_millis(500) {
                last_check = Instant::now();
                let selected = lock(s).selected.clone();
                let want = wanted(&en, &selected);
                let same = matches!((&current, &want), (Some((a, _)), Some((b, _))) if a == b);
                if !same {
                    let old_id = current.as_ref().map(|c| c.0.clone());
                    current = None;
                    if current_bt && let Some(old) = old_id {
                        lost_bt = Some(old);
                        fire(DeviceEvent::BluetoothLost);
                    }
                    current_bt = false;
                    if let Some((id, dev)) = want {
                        match open(&dev) {
                            Ok(st) => {
                                logln!("audio: output -> {}", friendly_name(&dev).unwrap_or_default());
                                current_bt = is_bluetooth(&dev);
                                if current_bt && lost_bt.as_deref() == Some(id.as_str()) {
                                    lost_bt = None;
                                    fire(DeviceEvent::BluetoothBack);
                                }
                                current = Some((id, st));
                            }
                            Err(e) => logln!("audio: cannot open device: {e}"),
                        }
                    }
                    lock(s).open = current.is_some();
                    s.cv.notify_all();
                }
                if let Some((_, st)) = &current {
                    let _ = st.client.Start();
                }
            }
            let Some((_, st)) = &current else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            if let Err(e) = fill(st, s) {
                if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    logln!("audio: device lost, reopening");
                } else {
                    logln!("audio: render error {e}, reopening");
                }
                if current_bt {
                    lost_bt = current.as_ref().map(|c| c.0.clone());
                    current_bt = false;
                    fire(DeviceEvent::BluetoothLost);
                }
                current = None;
                lock(s).open = false;
                last_check = Instant::now() - Duration::from_secs(1);
                continue;
            }
            if WaitForSingleObject(st.event, 200) != WAIT_OBJECT_0 {
                // No event in 200 ms: device stalled or stopped; the periodic check reopens it.
            }
        }
    }
}

/// The librespot sink. Every instance shares the one render thread.
pub struct CadenceSink {
    s: Arc<Shared>,
}

pub fn sink() -> Box<dyn Sink> {
    Box::new(CadenceSink { s: shared().clone() })
}

impl Sink for CadenceSink {
    fn start(&mut self) -> SinkResult<()> {
        lock(&self.s).running = true;
        self.s.cv.notify_all();
        Ok(())
    }

    /// Lets the buffered audio play out (max 1 s), then idles the device.
    fn stop(&mut self) -> SinkResult<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut g = lock(&self.s);
        while !g.ring.is_empty() && g.open && Instant::now() < deadline {
            g = self.s.cv.wait_timeout(g, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner()).0;
        }
        g.ring.clear();
        g.running = false;
        drop(g);
        self.s.cv.notify_all();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet.samples().map_err(|e| SinkError::OnWrite(e.to_string()))?;
        let mut data = converter.f64_to_f32(samples);
        crate::dsp::process(&mut data);
        let mut g = lock(&self.s);
        let mut waited = Duration::ZERO;
        while g.ring.len() + data.len() > CAP {
            // No device for a while: drop the oldest audio so playback time keeps moving
            // instead of freezing librespot.
            if !g.open && waited >= Duration::from_millis(500) {
                let excess = (g.ring.len() + data.len()).saturating_sub(CAP);
                let n = excess.min(g.ring.len());
                g.ring.drain(..n);
                break;
            }
            let (ng, _) = self.s.cv.wait_timeout(g, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner());
            g = ng;
            waited += Duration::from_millis(20);
        }
        g.ring.extend(data.iter().copied());
        Ok(())
    }
}
