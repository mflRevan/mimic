//! Audio capture streams, each on its own thread: the microphone, and (for the agent) a
//! loopback of an output device ("what you hear"). The cpal callback only resamples to
//! 16 kHz mono and hands the chunk on; nothing in the callback can block on the rest of
//! the app. A stream also feeds a [`Tap`]: the same audio as native-rate mono, lock-free,
//! for the virtual-mic passthrough and voice-clone references.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use mimic_core::protocol::DeviceInfo;

use crate::app::Ev;
use crate::dsp::Resampler;

pub enum AudioCmd {
    /// Open (or re-open) the device. Empty name = system default.
    Open {
        device: String,
        gain: f32,
    },
    Close,
    SetGain(f32),
}

pub enum AudioEv {
    /// 16 kHz mono.
    Chunk(Vec<f32>),
    Opened {
        device: String,
        sample_rate: u32,
    },
    Closed,
    Error(String),
}

/// Mic level for the overlay/meters, stored as f32 bits (lock-free).
#[derive(Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Where a stream's events go.
pub type Sink = Arc<dyn Fn(AudioEv) + Send + Sync>;

// ------------------------------------------------------------------ tap

/// Native-rate mono copy of a stream for real-time consumers. Each consumer registers its own
/// ring; the capture callback never blocks: it skips the copy while a registration is in
/// progress and drops samples a consumer is too slow to take.
#[derive(Clone, Default)]
pub struct Tap(Arc<TapInner>);

#[derive(Default)]
struct TapInner {
    sinks: parking_lot::Mutex<Vec<(u64, rtrb::Producer<f32>)>>,
    rate: AtomicU32,
    /// Bumped whenever the stream (re)opens: sample positions restart.
    epoch: AtomicU64,
    next: AtomicU64,
}

pub struct TapReader {
    tap: Tap,
    id: u64,
    pub rx: rtrb::Consumer<f32>,
}

impl Drop for TapReader {
    fn drop(&mut self) {
        self.tap.0.sinks.lock().retain(|(i, _)| *i != self.id);
    }
}

impl TapReader {
    /// Take everything buffered, appending to `out`.
    pub fn drain_into(&mut self, out: &mut Vec<f32>) {
        let n = self.rx.slots();
        if let Ok(chunk) = self.rx.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out.extend_from_slice(a);
            out.extend_from_slice(b);
            chunk.commit_all();
        }
    }
    pub fn tap(&self) -> &Tap {
        &self.tap
    }
}

impl Tap {
    /// A new reader buffering up to `secs` of audio (at 48 kHz).
    pub fn reader(&self, secs: f32) -> TapReader {
        let (p, c) = rtrb::RingBuffer::new(((secs * 48_000.0) as usize).max(1024));
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        self.0.sinks.lock().push((id, p));
        TapReader { tap: self.clone(), id, rx: c }
    }
    /// Sample rate of the stream feeding the tap (0 = not open).
    pub fn rate(&self) -> u32 {
        self.0.rate.load(Ordering::Relaxed)
    }
    pub fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Relaxed)
    }
    fn opened(&self, rate: u32) {
        self.0.rate.store(rate, Ordering::Relaxed);
        self.0.epoch.fetch_add(1, Ordering::Relaxed);
    }
    /// Real-time safe: never blocks.
    fn push(&self, mono: &[f32]) {
        if let Some(mut g) = self.0.sinks.try_lock() {
            for (_, p) in g.iter_mut() {
                let n = mono.len().min(p.slots());
                if n == 0 {
                    continue;
                }
                if let Ok(mut chunk) = p.write_chunk(n) {
                    let (a, b) = chunk.as_mut_slices();
                    let k = a.len();
                    a.copy_from_slice(&mono[..k]);
                    b.copy_from_slice(&mono[k..n]);
                    chunk.commit_all();
                }
            }
        }
    }
}

// ------------------------------------------------------------------ streams

/// What a stream captures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// An input device (microphone).
    Input,
    /// Loopback of an output device: everything it plays.
    Loopback,
}

/// The microphone stream; its events go to the controller.
pub fn spawn(ev: Sender<Ev>) -> (Sender<AudioCmd>, Tap) {
    let sink: Sink = Arc::new(move |e| {
        let _ = ev.try_send(Ev::Audio(e));
    });
    spawn_stream("mimic-audio", Kind::Input, sink)
}

pub fn spawn_stream(name: &str, kind: Kind, sink: Sink) -> (Sender<AudioCmd>, Tap) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let me = tx.clone();
    let tap = Tap::default();
    let t2 = tap.clone();
    std::thread::Builder::new().name(name.into()).spawn(move || run(rx, me, sink, kind, t2)).expect("spawn audio thread");
    (tx, tap)
}

fn run(rx: Receiver<AudioCmd>, me: Sender<AudioCmd>, ev: Sink, kind: Kind, tap: Tap) {
    let mut stream: Option<cpal::Stream> = None;
    let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let mut want: Option<String> = None;
    // A command that interrupted a file feed, handled next.
    let mut next: Option<AudioCmd> = None;
    loop {
        // While a device is wanted but failed (unplugged), retry every 2 s.
        let cmd = if let Some(c) = next.take() {
            Some(c)
        } else {
            match (stream.is_none(), want.is_some()) {
                (true, true) => match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(c) => Some(c),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                    Err(_) => return,
                },
                _ => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            }
        };
        match cmd {
            Some(AudioCmd::Open { device, gain: g }) => {
                gain.store(g.to_bits(), Ordering::Relaxed);
                stream = None;
                want = Some(device);
            }
            Some(AudioCmd::Close) => {
                stream = None;
                want = None;
                tap.0.rate.store(0, Ordering::Relaxed);
                ev(AudioEv::Closed);
                continue;
            }
            Some(AudioCmd::SetGain(g)) => {
                gain.store(g.to_bits(), Ordering::Relaxed);
                continue;
            }
            None => {}
        }
        if let (None, Some(name)) = (&stream, &want) {
            if let Some(path) = name.strip_prefix("file:") {
                // Test/dev input: stream a WAV in real time as if it were a device.
                next = feed_file(std::path::PathBuf::from(path), ev.clone(), rx.clone(), tap.clone());
                want = None;
                continue;
            }
            match open(name, kind, ev.clone(), gain.clone(), me.clone(), tap.clone()) {
                Ok((s, dev, rate)) => {
                    log::info!("{kind:?} stream open: {dev} @ {rate} Hz");
                    ev(AudioEv::Opened { device: dev, sample_rate: rate });
                    stream = Some(s);
                }
                Err(e) => {
                    log::warn!("{kind:?} stream: {e:#}");
                    ev(AudioEv::Error(format!("{e:#}")));
                }
            }
        }
    }
}

/// Plays `path` (then silence) as 10 ms chunks until a Close/Open command arrives; returns
/// that command so the caller acts on it.
fn feed_file(path: std::path::PathBuf, ev: Sink, rx: Receiver<AudioCmd>, tap: Tap) -> Option<AudioCmd> {
    let pcm = match crate::dsp::read_wav(&path) {
        Ok(p) => p,
        Err(e) => {
            ev(AudioEv::Error(format!("{e:#}")));
            return None;
        }
    };
    tap.opened(crate::dsp::SR as u32);
    ev(AudioEv::Opened { device: format!("file:{}", path.display()), sample_rate: 16_000 });
    let start = std::time::Instant::now();
    let mut sent = 0usize;
    loop {
        match rx.try_recv() {
            Ok(cmd @ (AudioCmd::Close | AudioCmd::Open { .. })) => {
                ev(AudioEv::Closed);
                return Some(cmd);
            }
            Ok(AudioCmd::SetGain(_)) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => return None,
            Err(_) => {}
        }
        let due = (start.elapsed().as_secs_f64() * 16_000.0) as usize;
        while sent + 160 <= due {
            let chunk: Vec<f32> = (sent..sent + 160).map(|i| pcm.get(i).copied().unwrap_or(0.0)).collect();
            tap.push(&chunk);
            ev(AudioEv::Chunk(chunk));
            sent += 160;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn device_name(d: &cpal::Device) -> String {
    d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| "Unknown device".into())
}

fn find_device(host: &cpal::Host, name: &str) -> anyhow::Result<cpal::Device> {
    if !name.is_empty() {
        if let Ok(devs) = host.input_devices() {
            for d in devs {
                if device_name(&d) == name {
                    return Ok(d);
                }
            }
        }
        log::warn!("microphone `{name}` not found, using the default");
    }
    host.default_input_device().ok_or_else(|| anyhow::anyhow!("no microphone found"))
}

/// An output device by name (exact, else case-insensitive substring); empty = the default output.
pub fn find_output(host: &cpal::Host, name: &str) -> anyhow::Result<cpal::Device> {
    let name = name.trim();
    if !name.is_empty() {
        let devs: Vec<cpal::Device> = host.output_devices().map(|d| d.collect()).unwrap_or_default();
        if let Some(d) = devs.iter().find(|d| device_name(d) == name) {
            return Ok(d.clone());
        }
        let l = name.to_lowercase();
        if let Some(d) = devs.iter().find(|d| device_name(d).to_lowercase().contains(&l)) {
            return Ok(d.clone());
        }
        anyhow::bail!("output device `{name}` not found");
    }
    host.default_output_device().ok_or_else(|| anyhow::anyhow!("no output device found"))
}

pub fn default_input_name() -> String {
    cpal::default_host().default_input_device().map(|d| device_name(&d)).unwrap_or_default()
}

pub fn default_output_name() -> String {
    cpal::default_host().default_output_device().map(|d| device_name(&d)).unwrap_or_default()
}

fn open(name: &str, kind: Kind, ev: Sink, gain: Arc<AtomicU32>, me: Sender<AudioCmd>, tap: Tap) -> anyhow::Result<(cpal::Stream, String, u32)> {
    let host = cpal::default_host();
    let (dev, supported) = match kind {
        Kind::Input => {
            let d = find_device(&host, name)?;
            let c = d.default_input_config()?;
            (d, c)
        }
        // WASAPI captures an output device in loopback mode when an input stream is built on it.
        Kind::Loopback => {
            let d = find_output(&host, name)?;
            let c = d.default_output_config()?;
            (d, c)
        }
    };
    let dev_name = device_name(&dev);
    let rate: u32 = supported.sample_rate();
    let channels = supported.channels() as usize;
    let fmt = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_ev = ev.clone();
    let want = name.to_string();
    let g2 = gain.clone();
    let on_err = move |e: cpal::Error| {
        use cpal::ErrorKind::*;
        match e.kind() {
            // The device went away or changed: report it and reopen (retries every 2 s).
            DeviceNotAvailable | DeviceChanged | StreamInvalidated | DeviceBusy | PermissionDenied | HostUnavailable => {
                log::warn!("{kind:?} stream lost: {e}; reopening");
                err_ev(AudioEv::Error(e.to_string()));
                let _ = me.send(AudioCmd::Open { device: want.clone(), gain: f32::from_bits(g2.load(Ordering::Relaxed)) });
            }
            // Overruns and similar glitches are harmless.
            _ => log::debug!("audio glitch: {e}"),
        }
    };

    let mut rs = Resampler::new(rate, channels);
    let mut out = Vec::with_capacity(1024);
    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    let mut scratch: Vec<f32> = Vec::new();
    tap.opened(rate);
    let ch = channels.max(1);
    let mut push = move |data: &[f32]| {
        let g = f32::from_bits(gain.load(Ordering::Relaxed));
        rs.set_gain(g);
        out.clear();
        rs.process(data, &mut out);
        if !out.is_empty() {
            ev(AudioEv::Chunk(out.clone()));
        }
        mono.clear();
        let k = g / ch as f32;
        mono.extend(data.chunks(ch).map(|f| f.iter().sum::<f32>() * k));
        tap.push(&mono);
    };

    let stream = match fmt {
        cpal::SampleFormat::F32 => dev.build_input_stream(config, move |d: &[f32], _: &_| push(d), on_err, None)?,
        cpal::SampleFormat::I16 => dev.build_input_stream(
            config,
            move |d: &[i16], _: &_| {
                scratch.clear();
                scratch.extend(d.iter().map(|s| *s as f32 / 32768.0));
                push(&scratch)
            },
            on_err,
            None,
        )?,
        cpal::SampleFormat::I32 => dev.build_input_stream(
            config,
            move |d: &[i32], _: &_| {
                scratch.clear();
                scratch.extend(d.iter().map(|s| *s as f32 / 2147483648.0));
                push(&scratch)
            },
            on_err,
            None,
        )?,
        cpal::SampleFormat::U16 => dev.build_input_stream(
            config,
            move |d: &[u16], _: &_| {
                scratch.clear();
                scratch.extend(d.iter().map(|s| (*s as f32 - 32768.0) / 32768.0));
                push(&scratch)
            },
            on_err,
            None,
        )?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, dev_name, rate))
}

pub fn list_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_input_device().map(|d| device_name(&d)).unwrap_or_default();
    let mut out: Vec<DeviceInfo> =
        host.input_devices().map(|it| it.map(|d| device_name(&d)).map(|n| DeviceInfo { default: n == default, name: n }).collect()).unwrap_or_default();
    out.dedup_by(|a, b| a.name == b.name);
    out
}

pub fn list_outputs() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = default_output_name();
    let mut out: Vec<DeviceInfo> =
        host.output_devices().map(|it| it.map(|d| device_name(&d)).map(|n| DeviceInfo { default: n == default, name: n }).collect()).unwrap_or_default();
    out.dedup_by(|a, b| a.name == b.name);
    out
}
