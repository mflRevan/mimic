//! The virtual microphone: an output stream into a virtual cable ("Speakers (Steam Streaming
//! Microphone)", "CABLE Input", ...) whose other end apps like Discord use as their mic.
//! It carries your real mic (passthrough, at its native rate) and, when the agent talks, the
//! agent's voice instead, plus soundboard effects. A second, optional output ("monitor") plays
//! the agent's voice and the effects where you hear them (the default output unless chosen).
//!
//! Nothing in the output callbacks blocks: voice audio arrives through SPSC rings, the mic
//! through a [`Tap`] reader, effects through a list the callback only `try_lock`s, and control
//! through atomics. Gains ramp, so muting the mic for the agent never clicks.
//!
//! The voice is generated while it plays, sometimes slower than real time (a busy GPU). Each
//! output keeps a jitter buffer: an utterance starts once a lead is buffered (or it is fully
//! generated); if the buffer still runs dry mid-utterance, the voice fades out over a few
//! milliseconds instead of cutting off (which is what crackles), waits for the lead again and
//! fades back in. Every underrun raises the lead (shared, so the next utterance starts with
//! it); clean utterances lower it again.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use mimic_core::agent::{RouteCfg, StreamStatus};
use parking_lot::Mutex;

use crate::audio::{self, Tap, TapReader};
use crate::dsp::RateConv;

/// Rate of the voice audio pushed in (Drama 3 renders 44.1 kHz).
pub const VOICE_RATE: u32 = crate::agent::tts::RATE;
/// Rate sound effects are decoded to.
pub const FX_RATE: u32 = 48_000;

/// A sound effect playing (or about to).
struct Fx {
    slot: usize,
    clip: Arc<Vec<f32>>,
    gain: f32,
    monitor: bool,
    /// Read position per output (in clip samples).
    pos: [f64; 2],
}

pub struct Shared {
    /// Silence the passthrough mic (the agent is talking, or you are talking to the agent).
    pub mute_mic: AtomicBool,
    mic_gain: AtomicU32,
    voice_gain: AtomicU32,
    passthrough: AtomicBool,
    /// Bumped to drop all queued voice audio (barge-in, stop).
    flush: AtomicU64,
    /// Voice samples (at VOICE_RATE) not yet played, per output; max over outputs counts.
    pending: [AtomicUsize; 2],
    status: Mutex<StreamStatus>,
    fx: Mutex<Vec<Fx>>,
    /// Which outputs are open (bit 0: virtual mic, bit 1: monitor).
    active: std::sync::atomic::AtomicU8,
    /// An utterance is being generated: more voice audio is on its way.
    voice_open: AtomicBool,
    /// Jitter-buffer lead in voice samples (adaptive).
    lead: AtomicUsize,
    /// Underruns so far (for logging).
    pub underruns: AtomicU64,
}

/// Lead before an utterance starts playing: the minimum, and the most it adapts up to.
const LEAD_MIN_S: f32 = 0.25;
const LEAD_MAX_S: f32 = 2.5;
/// Fade length at an underrun (and when resuming).
const FADE_S: f32 = 0.004;

/// Cheap handle used by the agent.
#[derive(Clone)]
pub struct Router {
    pub shared: Arc<Shared>,
    tx: Sender<Cmd>,
    voice: Arc<Mutex<Vec<rtrb::Producer<f32>>>>,
}

enum Cmd {
    Configure(RouteCfg),
    Stop,
}

impl Router {
    pub fn spawn(mic: Tap) -> Self {
        let shared = Arc::new(Shared {
            mute_mic: AtomicBool::new(false),
            mic_gain: AtomicU32::new(1.0f32.to_bits()),
            voice_gain: AtomicU32::new(1.0f32.to_bits()),
            passthrough: AtomicBool::new(true),
            flush: AtomicU64::new(0),
            pending: [AtomicUsize::new(0), AtomicUsize::new(0)],
            status: Mutex::new(StreamStatus::default()),
            fx: Mutex::new(Vec::new()),
            active: std::sync::atomic::AtomicU8::new(0),
            voice_open: AtomicBool::new(false),
            lead: AtomicUsize::new((LEAD_MIN_S * VOICE_RATE as f32) as usize),
            underruns: AtomicU64::new(0),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let voice = Arc::new(Mutex::new(Vec::new()));
        let r = Self { shared: shared.clone(), tx, voice: voice.clone() };
        std::thread::Builder::new().name("mimic-router".into()).spawn(move || run(rx, shared, voice, mic)).expect("spawn router thread");
        r
    }

    pub fn configure(&self, cfg: &RouteCfg) {
        let _ = self.tx.send(Cmd::Configure(cfg.clone()));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn status(&self) -> StreamStatus {
        self.shared.status.lock().clone()
    }

    /// Play a sound effect (mono, FX_RATE), or stop it if `slot` is already playing.
    /// Returns whether it started.
    pub fn toggle_fx(&self, slot: usize, clip: Arc<Vec<f32>>, gain: f32, monitor: bool) -> bool {
        let mut fx = self.shared.fx.lock();
        if fx.iter().any(|f| f.slot == slot) {
            fx.retain(|f| f.slot != slot);
            return false;
        }
        fx.push(Fx { slot, clip, gain, monitor, pos: [0.0; 2] });
        true
    }

    pub fn stop_fx(&self) {
        self.shared.fx.lock().clear();
    }

    /// Slots playing right now.
    pub fn fx_playing(&self) -> Vec<usize> {
        self.shared.fx.lock().iter().map(|f| f.slot).collect()
    }

    /// Any output to play the agent's voice on.
    pub fn has_output(&self) -> bool {
        !self.voice.lock().is_empty()
    }

    /// Queue voice audio (mono, VOICE_RATE). Blocks (briefly sleeping) while the rings are
    /// full; returns false if `cancelled` became true meanwhile.
    pub fn push_voice(&self, pcm: &[f32], cancelled: &dyn Fn() -> bool) -> bool {
        let mut done = vec![0usize; self.voice.lock().len()];
        loop {
            if cancelled() {
                return false;
            }
            let mut all = true;
            {
                let mut v = self.voice.lock();
                if v.len() != done.len() {
                    // Outputs changed mid-way: just restart the counters for the new set.
                    done = vec![0; v.len()];
                }
                for (p, d) in v.iter_mut().zip(done.iter_mut()) {
                    let n = (pcm.len() - *d).min(p.slots());
                    if n > 0 {
                        if let Ok(mut c) = p.write_chunk(n) {
                            let (a, b) = c.as_mut_slices();
                            let k = a.len();
                            a.copy_from_slice(&pcm[*d..*d + k]);
                            b.copy_from_slice(&pcm[*d + k..*d + n]);
                            c.commit_all();
                            *d += n;
                        }
                    }
                    if *d < pcm.len() {
                        all = false;
                    }
                }
            }
            if all {
                return true;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    /// An utterance starts (`true`, more audio follows) or has been fully generated (`false`).
    pub fn voice_open(&self, open: bool) {
        self.shared.voice_open.store(open, Ordering::SeqCst);
    }

    /// An utterance played without an underrun: relax the lead a little.
    pub fn voice_clean(&self) {
        let min = (LEAD_MIN_S * VOICE_RATE as f32) as usize;
        let l = self.shared.lead.load(Ordering::Relaxed);
        self.shared.lead.store((l * 4 / 5).max(min), Ordering::Relaxed);
    }

    /// The current jitter-buffer lead in seconds.
    pub fn lead_s(&self) -> f32 {
        self.shared.lead.load(Ordering::Relaxed) as f32 / VOICE_RATE as f32
    }

    /// Drop all queued voice audio now.
    pub fn flush(&self) {
        self.shared.flush.fetch_add(1, Ordering::SeqCst);
    }

    /// Seconds of voice audio still to be played.
    pub fn pending_s(&self) -> f32 {
        let n = self.shared.pending.iter().map(|p| p.load(Ordering::Relaxed)).max().unwrap_or(0);
        n as f32 / VOICE_RATE as f32
    }
}

// ------------------------------------------------------------------ router thread

struct Outputs {
    _streams: Vec<OutStream>,
}

enum OutStream {
    /// Held to keep the stream playing.
    Device(#[allow(dead_code)] cpal::Stream),
    File(Arc<AtomicBool>),
}

fn run(rx: Receiver<Cmd>, shared: Arc<Shared>, voice: Arc<Mutex<Vec<rtrb::Producer<f32>>>>, mic: Tap) {
    let mut outs: Option<Outputs> = None;
    let mut want: Option<RouteCfg> = None;
    let mut retry_at: Option<Instant> = None;
    loop {
        let cmd = match retry_at {
            Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())).map(Some).or_else(|e| match e {
                crossbeam_channel::RecvTimeoutError::Timeout => Ok(None),
                crossbeam_channel::RecvTimeoutError::Disconnected => Err(()),
            }),
            None => rx.recv().map(Some).map_err(|_| ()),
        };
        let cmd = match cmd {
            Ok(c) => c,
            Err(()) => return,
        };
        match cmd {
            Some(Cmd::Configure(cfg)) => {
                shared.mic_gain.store(cfg.mic_gain.to_bits(), Ordering::Relaxed);
                shared.voice_gain.store(cfg.voice_gain.to_bits(), Ordering::Relaxed);
                shared.passthrough.store(cfg.passthrough, Ordering::Relaxed);
                let same_devices = want.as_ref().is_some_and(|w| w.output == cfg.output && w.monitor == cfg.monitor && w.monitor_device == cfg.monitor_device);
                want = Some(cfg);
                if same_devices && outs.is_some() {
                    continue;
                }
            }
            Some(Cmd::Stop) => {
                want = None;
            }
            None => {}
        }
        // (Re)build the outputs.
        voice.lock().clear();
        outs = None;
        retry_at = None;
        for p in &shared.pending {
            p.store(0, Ordering::Relaxed);
        }
        let Some(cfg) = want.clone() else {
            *shared.status.lock() = StreamStatus::default();
            continue;
        };
        match open_outputs(&cfg, &shared, &voice, &mic) {
            Ok((o, name)) => {
                *shared.status.lock() = StreamStatus { label: "Virtual mic".into(), device: name, open: true, error: None };
                outs = Some(o);
            }
            Err(e) => {
                log::warn!("virtual mic: {e:#}");
                *shared.status.lock() = StreamStatus { label: "Virtual mic".into(), device: cfg.output.clone(), open: false, error: Some(format!("{e:#}")) };
                retry_at = Some(Instant::now() + Duration::from_secs(3));
            }
        }
    }
}

fn open_outputs(cfg: &RouteCfg, shared: &Arc<Shared>, voice: &Arc<Mutex<Vec<rtrb::Producer<f32>>>>, mic: &Tap) -> anyhow::Result<(Outputs, String)> {
    let mut streams = Vec::new();
    let mut producers = Vec::new();
    let mut name = String::new();
    if !cfg.output.trim().is_empty() {
        let (p, c) = rtrb::RingBuffer::new(VOICE_RATE as usize * 4);
        let reader = cfg.passthrough.then(|| mic.reader(0.5));
        let (s, n) = open_one(&cfg.output, c, reader, shared.clone(), 0)?;
        streams.push(s);
        producers.push(p);
        name = n;
    }
    if cfg.monitor {
        let (p, c) = rtrb::RingBuffer::new(VOICE_RATE as usize * 4);
        match open_one(&cfg.monitor_device, c, None, shared.clone(), 1) {
            Ok((s, n)) => {
                streams.push(s);
                producers.push(p);
                if name.is_empty() {
                    name = format!("monitor: {n}");
                }
            }
            Err(e) => log::warn!("monitor output: {e:#}"),
        }
    }
    *voice.lock() = producers;
    let mask = u8::from(!cfg.output.trim().is_empty()) | if cfg.monitor { 2 } else { 0 };
    shared.active.store(mask, Ordering::Relaxed);
    Ok((Outputs { _streams: streams }, name))
}

/// Mixer state living in one output callback.
struct Mix {
    voice: rtrb::Consumer<f32>,
    voice_conv: RateConv,
    mic: Option<TapReader>,
    mic_conv: RateConv,
    mic_rate: u32,
    mic_epoch: u64,
    out_rate: u32,
    shared: Arc<Shared>,
    slot: usize,
    flush_seen: u64,
    mic_level: f32,
    /// Jitter buffer: the voice is playing (else waiting for the lead).
    primed: bool,
    /// Voice gain ramp (fades at underruns / resumes).
    vlevel: f32,
    vbuf: Vec<f32>,
    mbuf: Vec<f32>,
    tmp: Vec<f32>,
}

impl Mix {
    fn new(voice: rtrb::Consumer<f32>, mic: Option<TapReader>, out_rate: u32, shared: Arc<Shared>, slot: usize) -> Self {
        let flush_seen = shared.flush.load(Ordering::SeqCst);
        Self {
            voice,
            voice_conv: RateConv::new(VOICE_RATE, out_rate),
            mic,
            mic_conv: RateConv::new(48_000, out_rate),
            mic_rate: 0,
            mic_epoch: u64::MAX,
            out_rate,
            shared,
            slot,
            flush_seen,
            mic_level: 1.0,
            primed: false,
            vlevel: 0.0,
            vbuf: Vec::with_capacity(8192),
            mbuf: Vec::with_capacity(8192),
            tmp: Vec::with_capacity(8192),
        }
    }

    /// Fill `frames` mono samples at the output rate.
    fn render(&mut self, frames: usize, out: &mut Vec<f32>) {
        out.clear();
        out.resize(frames, 0.0);
        // Voice.
        let f = self.shared.flush.load(Ordering::SeqCst);
        if f != self.flush_seen {
            self.flush_seen = f;
            let n = self.voice.slots();
            if let Ok(c) = self.voice.read_chunk(n) {
                c.commit_all();
            }
            self.vbuf.clear();
            self.primed = false;
            self.vlevel = 0.0;
        }
        // Jitter buffer: wait for the lead (or the end of the utterance) before playing.
        let open = self.shared.voice_open.load(Ordering::SeqCst);
        if !self.primed {
            let buffered = self.voice.slots() + (self.vbuf.len() as f64 * VOICE_RATE as f64 / self.out_rate as f64) as usize;
            let lead = self.shared.lead.load(Ordering::Relaxed);
            if buffered > 0 && (buffered >= lead || !open) {
                self.primed = true;
            }
        }
        while self.primed && self.vbuf.len() < frames {
            let want = ((frames - self.vbuf.len()) as f64 * VOICE_RATE as f64 / self.out_rate as f64).ceil() as usize + 2;
            let n = want.min(self.voice.slots());
            if n == 0 {
                break;
            }
            self.tmp.clear();
            if let Ok(c) = self.voice.read_chunk(n) {
                let (a, b) = c.as_slices();
                self.tmp.extend_from_slice(a);
                self.tmp.extend_from_slice(b);
                c.commit_all();
            }
            let tmp = std::mem::take(&mut self.tmp);
            self.voice_conv.process(&tmp, &mut self.vbuf);
            self.tmp = tmp;
        }
        let vg = f32::from_bits(self.shared.voice_gain.load(Ordering::Relaxed));
        if self.primed {
            let take = self.vbuf.len().min(frames);
            let fade = ((self.out_rate as f32 * FADE_S) as usize).max(1);
            let step = 1.0 / fade as f32;
            // Running dry while more is coming: fade out the tail we have and wait again.
            let underrun = take < frames && open;
            for (k, (o, v)) in out.iter_mut().zip(self.vbuf.drain(..take)).enumerate() {
                let target = if underrun && k + fade >= take { 0.0 } else { 1.0 };
                if self.vlevel < target {
                    self.vlevel = (self.vlevel + step).min(target);
                } else if self.vlevel > target {
                    self.vlevel = (self.vlevel - step).max(target);
                }
                *o += v * vg * self.vlevel;
            }
            if underrun {
                self.primed = false;
                self.vlevel = 0.0;
                self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                // Ask for a longer lead (shared by all outputs and the next utterances).
                let max = (LEAD_MAX_S * VOICE_RATE as f32) as usize;
                let l = self.shared.lead.load(Ordering::Relaxed);
                self.shared.lead.store((l * 3 / 2 + VOICE_RATE as usize / 5).min(max), Ordering::Relaxed);
            } else if take == 0 && !open {
                // The utterance ended: the next one starts from silence again.
                self.primed = false;
                self.vlevel = 0.0;
            }
        }
        let queued = self.voice.slots() + (self.vbuf.len() as f64 * VOICE_RATE as f64 / self.out_rate as f64) as usize;
        self.shared.pending[self.slot].store(queued, Ordering::Relaxed);

        // Mic passthrough.
        if let Some(m) = self.mic.as_mut() {
            let tap = m.tap();
            let (rate, epoch) = (tap.rate(), tap.epoch());
            if rate != self.mic_rate || epoch != self.mic_epoch {
                self.mic_rate = rate;
                self.mic_epoch = epoch;
                self.mic_conv = RateConv::new(rate.max(1), self.out_rate);
                self.mbuf.clear();
            }
            self.tmp.clear();
            m.drain_into(&mut self.tmp);
            if rate > 0 {
                let tmp = std::mem::take(&mut self.tmp);
                self.mic_conv.process(&tmp, &mut self.mbuf);
                self.tmp = tmp;
            }
            // Keep latency bounded: if the mic runs ahead (clock drift, a stall), drop the excess.
            let max = frames + self.out_rate as usize / 25; // + 40 ms
            if self.mbuf.len() > max {
                let cut = self.mbuf.len() - frames;
                self.mbuf.drain(..cut);
            }
            let on = self.shared.passthrough.load(Ordering::Relaxed) && !self.shared.mute_mic.load(Ordering::Relaxed);
            let target = if on { f32::from_bits(self.shared.mic_gain.load(Ordering::Relaxed)) } else { 0.0 };
            // ~15 ms ramp.
            let step = 1.0 / (self.out_rate as f32 * 0.015);
            let take = self.mbuf.len().min(frames);
            for (o, x) in out.iter_mut().zip(self.mbuf.drain(..take)) {
                if self.mic_level < target {
                    self.mic_level = (self.mic_level + step).min(target);
                } else if self.mic_level > target {
                    self.mic_level = (self.mic_level - step).max(target);
                }
                *o += x * self.mic_level;
            }
        }
        self.mix_fx(out);
        for o in out.iter_mut() {
            *o = o.clamp(-1.0, 1.0);
        }
    }
}

impl Mix {
    /// Add playing sound effects (never waits: skipped for one block if the list is busy).
    fn mix_fx(&mut self, out: &mut [f32]) {
        let Some(mut fx) = self.shared.fx.try_lock() else { return };
        if fx.is_empty() {
            return;
        }
        let step = FX_RATE as f64 / self.out_rate as f64;
        let s = self.slot;
        for f in fx.iter_mut() {
            if s == 1 && !f.monitor {
                continue;
            }
            let n = f.clip.len();
            for o in out.iter_mut() {
                let i = f.pos[s] as usize;
                if i + 1 >= n {
                    f.pos[s] = n as f64;
                    break;
                }
                let t = (f.pos[s] - i as f64) as f32;
                *o += (f.clip[i] + (f.clip[i + 1] - f.clip[i]) * t) * f.gain;
                f.pos[s] += step;
            }
        }
        // Finished once every open output that plays it has reached the end.
        let active = self.shared.active.load(Ordering::Relaxed);
        fx.retain(|f| {
            let n = f.clip.len() as f64;
            let done0 = active & 1 == 0 || f.pos[0] >= n;
            let done1 = active & 2 == 0 || !f.monitor || f.pos[1] >= n;
            !(done0 && done1)
        });
    }
}

fn open_one(name: &str, voice: rtrb::Consumer<f32>, mic: Option<TapReader>, shared: Arc<Shared>, slot: usize) -> anyhow::Result<(OutStream, String)> {
    if let Some(path) = name.strip_prefix("file:") {
        return open_file(std::path::PathBuf::from(path), voice, mic, shared, slot);
    }
    let host = cpal::default_host();
    let dev = audio::find_output(&host, name)?;
    let dev_name = audio::device_name(&dev);
    let supported = dev.default_output_config()?;
    let rate: u32 = supported.sample_rate();
    let channels = supported.channels() as usize;
    let fmt = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let mut mix = Mix::new(voice, mic, rate, shared, slot);
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let on_err = |e: cpal::Error| log::warn!("output stream: {e}");
    let ch = channels.max(1);
    // Each arm owns the mixer; `conv` maps a mixed sample to the device format.
    macro_rules! out_stream {
        ($t:ty, $conv:expr) => {
            dev.build_output_stream(
                config,
                move |d: &mut [$t], _: &_| {
                    mix.render(d.len() / ch, &mut mono);
                    for (f, s) in d.chunks_mut(ch).zip(&mono) {
                        f.fill($conv(*s));
                    }
                },
                on_err,
                None,
            )?
        };
    }
    let stream = match fmt {
        cpal::SampleFormat::F32 => out_stream!(f32, |s: f32| s),
        cpal::SampleFormat::I16 => out_stream!(i16, |s: f32| (s * 32767.0) as i16),
        cpal::SampleFormat::I32 => out_stream!(i32, |s: f32| (s as f64 * 2147483647.0) as i32),
        other => anyhow::bail!("unsupported output format {other:?}"),
    };
    stream.play()?;
    log::info!("output open: {dev_name} @ {rate} Hz ({})", if slot == 0 { "virtual mic" } else { "monitor" });
    Ok((OutStream::Device(stream), dev_name))
}

/// Test sink: renders the output in real time at 48 kHz into a WAV file (rewritten every second).
fn open_file(
    path: std::path::PathBuf,
    voice: rtrb::Consumer<f32>,
    mic: Option<TapReader>,
    shared: Arc<Shared>,
    slot: usize,
) -> anyhow::Result<(OutStream, String)> {
    const RATE: u32 = 48_000;
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let p2 = path.clone();
    std::thread::Builder::new().name("mimic-router-file".into()).spawn(move || {
        let mut mix = Mix::new(voice, mic, RATE, shared, slot);
        let mut all: Vec<f32> = Vec::new();
        let mut buf = Vec::new();
        let start = Instant::now();
        let mut done = 0usize;
        let mut last_write = Instant::now();
        while !s2.load(Ordering::Relaxed) {
            let due = (start.elapsed().as_secs_f64() * RATE as f64) as usize;
            if due > done {
                mix.render(due - done, &mut buf);
                all.extend_from_slice(&buf);
                done = due;
            }
            if last_write.elapsed() > Duration::from_secs(1) {
                let _ = std::fs::write(&p2, crate::dsp::wav_bytes_at(&all, RATE));
                last_write = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::write(&p2, crate::dsp::wav_bytes_at(&all, RATE));
    })?;
    Ok((OutStream::File(stop), format!("file:{}", path.display())))
}

impl Drop for OutStream {
    fn drop(&mut self) {
        if let OutStream::File(stop) = self {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            mute_mic: AtomicBool::new(false),
            mic_gain: AtomicU32::new(1.0f32.to_bits()),
            voice_gain: AtomicU32::new(1.0f32.to_bits()),
            passthrough: AtomicBool::new(false),
            flush: AtomicU64::new(0),
            pending: [AtomicUsize::new(0), AtomicUsize::new(0)],
            status: Mutex::new(StreamStatus::default()),
            fx: Mutex::new(Vec::new()),
            active: std::sync::atomic::AtomicU8::new(1),
            voice_open: AtomicBool::new(false),
            lead: AtomicUsize::new((LEAD_MIN_S * VOICE_RATE as f32) as usize),
            underruns: AtomicU64::new(0),
        })
    }

    /// Generation slower than playback: the output must never jump (no clicks), the
    /// underruns are counted and the lead grows, so later utterances play through.
    #[test]
    fn slow_generation_does_not_click() {
        let sh = shared();
        let (mut p, c) = rtrb::RingBuffer::new(VOICE_RATE as usize * 4);
        let mut mix = Mix::new(c, None, 48_000, sh.clone(), 0);
        let tone = |i: usize| (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / VOICE_RATE as f32).sin() * 0.5;
        let lead0 = sh.lead.load(Ordering::Relaxed);
        sh.voice_open.store(true, Ordering::SeqCst);
        let (mut made, mut out, mut buf) = (0usize, Vec::new(), Vec::new());
        // 6 s of playback in 10 ms blocks; audio arrives in 0.3 s chunks at 0.7x real time.
        let chunk = (VOICE_RATE as f32 * 0.3) as usize;
        let total = VOICE_RATE as usize * 3;
        let mut credit = 0.0f64;
        for _ in 0..600 {
            credit += 0.7 * VOICE_RATE as f64 * 0.01;
            while credit >= chunk as f64 && made < total {
                let pcm: Vec<f32> = (made..made + chunk).map(tone).collect();
                p.write_chunk_uninit(chunk).unwrap().fill_from_iter(pcm);
                made += chunk;
                credit -= chunk as f64;
            }
            if made >= total {
                sh.voice_open.store(false, Ordering::SeqCst);
            }
            mix.render(480, &mut buf);
            out.extend_from_slice(&buf);
        }
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // A 220 Hz sine at 0.5 moves < 0.015 per sample at 48 kHz; a hard cut would jump ~0.5.
        assert!(max_step < 0.05, "click: step {max_step}");
        assert!(sh.underruns.load(Ordering::Relaxed) > 0);
        assert!(sh.lead.load(Ordering::Relaxed) > lead0);
        // Everything generated was played.
        let played = out.iter().filter(|x| x.abs() > 1e-6).count() as f32 / 48_000.0;
        assert!(played > 2.8, "played {played} s");
    }
}
