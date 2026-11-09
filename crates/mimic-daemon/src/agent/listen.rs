//! Listening: your mic (native rate, for the voice bank too) and any number of hearing sources
//! (one app such as Discord, or an output device), each segmented into phrases, transcribed
//! with the dictation engine and labelled with where it came from.

use std::sync::Arc;

use mimic_core::agent::{wake, HearSource, StreamStatus, Who};
use mimic_core::text::Transcript;

use super::segment::{Phrase, Segmenter, Spans};
use super::{now_ms, pending_status, Agent, Msg};
use crate::audio::{self, AudioCmd, AudioEv, Tap, TapReader};
use crate::audio_app::{self, AppCapture};
use crate::dsp::{Resampler, SR};
use crate::engine::{DecodeOpts, EngineCmd, Job, JobKind};

/// A phrase being transcribed.
pub struct Pending {
    /// `None` = your mic; else the hearing stream it came from.
    pub hear: Option<u64>,
    pub label: String,
    /// You were talking to the agent (shortcut held, or autopilot muting your mic).
    pub private: bool,
    /// Part of a held-shortcut utterance (collected until release).
    pub talk: bool,
    /// Counts as your conversation (not dictation, not echo).
    pub convo: bool,
    /// Native-rate audio for the voice bank.
    pub native: Option<(Vec<f32>, u32)>,
    pub at: u64,
    /// Which voice said it (hearing streams).
    pub speaker: Option<u32>,
    /// The end-of-turn model said they're done: answer right away.
    pub turn_done: bool,
    /// Seconds of speech in the phrase.
    pub secs: f32,
    /// Seconds of context audio decoded in front of the phrase (its words are dropped).
    pub ctx_s: f32,
}

/// Recent speech of one stream, kept as context for the next phrase. Short replies ("ja,
/// genau", "okay") decoded on their own leave a multilingual model guessing the language
/// from a second of audio; with the conversation's last few seconds in front of them it hears
/// the language (and the voices) it has been hearing.
#[derive(Default)]
pub struct SttContext {
    audio: Vec<f32>,
    at: Option<std::time::Instant>,
}

/// Context kept per stream (seconds), and how long it stays relevant after the last phrase.
const CTX_S: f32 = 8.0;
const CTX_FRESH: std::time::Duration = std::time::Duration::from_secs(120);
/// Silence between context pieces and before the phrase, so words don't run together.
const CTX_GAP_S: f32 = 0.3;

impl SttContext {
    /// The context to put in front of a phrase (empty if stale).
    fn prefix(&self) -> &[f32] {
        if self.at.is_some_and(|t| t.elapsed() < CTX_FRESH) {
            &self.audio
        } else {
            &[]
        }
    }

    fn push(&mut self, phrase: &[f32]) {
        if !self.at.is_some_and(|t| t.elapsed() < CTX_FRESH) {
            self.audio.clear();
        }
        if !self.audio.is_empty() {
            self.audio.extend(std::iter::repeat_n(0.0, (CTX_GAP_S * SR as f32) as usize));
        }
        self.audio.extend_from_slice(phrase);
        let max = (CTX_S * SR as f32) as usize;
        if self.audio.len() > max {
            self.audio.drain(..self.audio.len() - max);
        }
        self.at = Some(std::time::Instant::now());
    }
}

/// The native-rate mic, resampled to 16 kHz here so both are sample-aligned.
pub struct MicFeed {
    reader: TapReader,
    epoch: u64,
    rate: u32,
    rs: Resampler,
    native: Vec<f32>,
    /// Native index of `native[0]` within the current epoch.
    native_base: usize,
    /// 16 kHz stream position where the current epoch started.
    epoch_start: usize,
}

impl MicFeed {
    pub fn new(tap: &Tap) -> Self {
        Self { reader: tap.reader(2.0), epoch: u64::MAX, rate: 0, rs: Resampler::new(48_000, 1), native: Vec::new(), native_base: 0, epoch_start: 0 }
    }

    /// Pull new audio; returns 16 kHz samples for the segmenter.
    fn pull(&mut self, seg_pos: usize) -> Vec<f32> {
        let tap = self.reader.tap().clone();
        let (rate, epoch) = (tap.rate(), tap.epoch());
        let mut raw = Vec::new();
        self.reader.drain_into(&mut raw);
        if rate == 0 {
            return Vec::new();
        }
        if epoch != self.epoch || rate != self.rate {
            self.epoch = epoch;
            self.rate = rate;
            self.rs = Resampler::new(rate, 1);
            self.native.clear();
            self.native_base = 0;
            self.epoch_start = seg_pos;
        }
        self.native.extend_from_slice(&raw);
        let cap = rate as usize * 30;
        if self.native.len() > cap + rate as usize * 5 {
            let cut = self.native.len() - cap;
            self.native.drain(..cut);
            self.native_base += cut;
        }
        let mut out = Vec::with_capacity(raw.len() / 2);
        self.rs.process(&raw, &mut out);
        out
    }

    /// Native-rate audio for the 16 kHz range `[a, b)`, if still buffered.
    fn clip(&self, a: usize, b: usize) -> Option<(Vec<f32>, u32)> {
        if self.rate == 0 || a < self.epoch_start {
            return None;
        }
        let r = self.rate as usize;
        let ia = ((a - self.epoch_start) * r / SR).checked_sub(self.native_base)?;
        let ib = ((b - self.epoch_start) * r / SR).saturating_sub(self.native_base).min(self.native.len());
        (ib > ia).then(|| (self.native[ia..ib].to_vec(), self.rate))
    }
}

/// Recent audio at 16 kHz as i16 (half the memory), for snapshots.
pub struct History {
    buf: Vec<i16>,
    head: usize,
    len: usize,
}

impl History {
    pub fn new(secs: usize) -> Self {
        Self { buf: vec![0; (secs * SR).max(1)], head: 0, len: 0 }
    }
    pub fn push(&mut self, xs: &[f32]) {
        let cap = self.buf.len();
        if cap <= 1 {
            return;
        }
        for &x in xs {
            self.buf[self.head] = (x.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.head = (self.head + 1) % cap;
        }
        self.len = (self.len + xs.len()).min(cap);
    }
    pub fn last(&self, n: usize) -> Vec<f32> {
        let n = n.min(self.len);
        let cap = self.buf.len();
        let start = (self.head + cap - n) % cap;
        (0..n).map(|i| self.buf[(start + i) % cap] as f32 / 32768.0).collect()
    }
}

enum Handle {
    Device(crossbeam_channel::Sender<AudioCmd>),
    App(#[allow(dead_code)] AppCapture),
}

/// One hearing source with its own segmentation and history.
pub struct HearStream {
    pub id: u64,
    pub src: HearSource,
    pub label: String,
    handle: Handle,
    pub seg: Segmenter,
    pub hist: History,
    /// When the agent was speaking, in this stream's positions (echo filtering).
    pub speak: Spans,
    pub status: StreamStatus,
}

impl Drop for HearStream {
    fn drop(&mut self) {
        if let Handle::Device(tx) = &self.handle {
            let _ = tx.send(AudioCmd::Close);
        }
    }
}

impl HearStream {
    /// Could the agent's own voice come back through this source? (It plays on `monitor`.)
    pub fn echoes(&self, monitor: &Option<String>) -> bool {
        let Some(m) = monitor else { return false };
        match &self.src {
            HearSource::Device(d) => d.is_empty() && *m == audio::default_output_name() || !d.is_empty() && m.to_lowercase().contains(&d.to_lowercase()),
            _ => false,
        }
    }
}

impl Agent {
    /// (Re)open the hearing sources from the config.
    pub(super) fn open_hears(&mut self) {
        self.hears.clear();
        let h = self.a().hear.clone();
        if !h.enabled {
            return;
        }
        let history = self.a().history_min as usize * 60;
        for s in h.sources.iter().filter_map(|s| HearSource::parse(s)) {
            let id = self.next_hear;
            self.next_hear += 1;
            let tx = self.tx.clone();
            let sink: audio::Sink = Arc::new(move |ev| {
                let _ = tx.send(Msg::Hear { id, ev });
            });
            let label = s.label();
            let handle = match &s {
                HearSource::App(exe) => Handle::App(audio_app::spawn(exe, sink)),
                HearSource::Device(name) => {
                    let (cmd, _) = audio::spawn_stream(&format!("mimic-hear-{id}"), audio::Kind::Loopback, sink);
                    let _ = cmd.send(AudioCmd::Open { device: name.clone(), gain: 1.0 });
                    Handle::Device(cmd)
                }
                HearSource::File(path) => {
                    let (cmd, _) = audio::spawn_stream(&format!("mimic-hear-{id}"), audio::Kind::Loopback, sink);
                    let _ = cmd.send(AudioCmd::Open { device: format!("file:{path}"), gain: 1.0 });
                    Handle::Device(cmd)
                }
            };
            log::info!("hearing {label} ({s:?})");
            self.hears.push(HearStream {
                id,
                status: pending_status(&label),
                label,
                src: s,
                handle,
                seg: Segmenter::new(h.pause_ms, 28),
                hist: History::new(history),
                speak: Spans::default(),
            });
        }
        if self.turn.is_some() || self.embed.is_some() {
            self.attach_silero();
        }
    }

    pub(super) fn on_hear(&mut self, id: u64, e: AudioEv) {
        let Some(i) = self.hears.iter().position(|h| h.id == id) else { return };
        match e {
            AudioEv::Chunk(c) => {
                let h = &mut self.hears[i];
                h.hist.push(&c);
                let mut out = Vec::new();
                h.seg.feed(&c, &mut out);
                for p in out {
                    self.on_phrase(Some(id), p, false);
                }
                // A short pause: if the end-of-turn model says they're done, close the phrase now.
                if self.probe_turn(i) {
                    let mut out = Vec::new();
                    self.hears[i].seg.flush(&mut out);
                    for p in out {
                        self.on_phrase(Some(id), p, true);
                    }
                }
            }
            AudioEv::Opened { device, .. } => {
                let h = &mut self.hears[i];
                h.status = StreamStatus { label: h.label.clone(), device, open: true, error: None };
            }
            AudioEv::Closed => self.hears[i].status.open = false,
            AudioEv::Error(e) => {
                let h = &mut self.hears[i];
                h.status.open = false;
                h.status.error = Some(e);
            }
        }
    }

    pub(super) fn pump_mic(&mut self) {
        let Some(m) = self.mic.as_mut() else { return };
        let pcm = m.pull(self.mic_seg.pos());
        if pcm.is_empty() {
            return;
        }
        self.mic_hist.push(&pcm);
        let mut out = Vec::new();
        self.mic_seg.feed(&pcm, &mut out);
        for p in out {
            self.on_phrase(None, p, false);
        }
    }

    pub(super) fn flush_mic(&mut self) {
        self.pump_mic();
        let mut out = Vec::new();
        self.mic_seg.flush(&mut out);
        for p in out {
            self.on_phrase(None, p, false);
        }
    }

    /// A phrase closed: decide what it is and send it to transcription.
    fn on_phrase(&mut self, hear: Option<u64>, p: Phrase, turn_done: bool) {
        let secs = (p.speech.1.saturating_sub(p.speech.0)) as f32 / SR as f32;
        let slack = SR * 4 / 10;
        let (label, private, talk, convo, native) = match hear {
            None => {
                let talk = self.talk.overlaps(p.speech.0, p.speech.1, slack / 2);
                let echo = self.speak_mic.overlaps(p.speech.0, p.speech.1, slack);
                let dictation = self.dictating.overlaps(p.speech.0, p.speech.1, 0);
                let muted = self.autopilot && self.a().mute_mic_in_autopilot;
                let native = if echo { None } else { self.mic.as_ref().and_then(|m| m.clip(p.speech.0.saturating_sub(SR / 10), p.speech.1 + SR / 10)) };
                // The agent's own voice leaking into your mic (also in autopilot, where everything
                // else from the mic counts as talking to the agent). Only the talk key overrides.
                if echo && !talk {
                    log::debug!("mic: dropped a phrase over the agent's own speech");
                    return;
                }
                if !talk && !muted && dictation {
                    // Dictation: meant for a text field, but still your voice for the clone.
                    (String::new(), false, false, false, native)
                } else {
                    (String::new(), talk || muted, talk, !(talk || muted), native)
                }
            }
            Some(id) => {
                let monitor = self.monitor_device();
                let Some(h) = self.hears.iter().find(|h| h.id == id) else { return };
                if h.echoes(&monitor) && h.speak.overlaps(p.speech.0, p.speech.1, slack) {
                    return;
                }
                (h.label.clone(), false, false, true, None)
            }
        };
        let speaker = if hear.is_some() { self.identify(&p.audio, secs) } else { None };
        // Decode with the stream's recent speech in front (engines with word timestamps only:
        // their words tell where the phrase starts). Private talk isn't conversation context.
        let timestamps = mimic_core::models::find(&self.cfg.model.id).is_some_and(|m| m.engine == mimic_core::models::EngineKind::Parakeet);
        let ctx = self.stt_ctx.entry(hear).or_default();
        let (audio, ctx_s) = if timestamps && !private && !talk && !ctx.prefix().is_empty() {
            let pre = ctx.prefix();
            let gap = (CTX_GAP_S * SR as f32) as usize;
            let mut a = Vec::with_capacity(pre.len() + gap + p.audio.len());
            a.extend_from_slice(pre);
            a.extend(std::iter::repeat_n(0.0, gap));
            a.extend_from_slice(&p.audio);
            (a, (pre.len() + gap) as f32 / SR as f32)
        } else {
            (p.audio.clone(), 0.0)
        };
        if !private && !talk {
            ctx.push(&p.audio);
        }
        let id = self.next_job;
        self.next_job += 1;
        self.pending.insert(id, Pending { hear, label, private, talk, convo, native, at: now_ms(), speaker, turn_done, secs, ctx_s });
        let language = match self.cfg.model.language.trim() {
            "" | "auto" => None,
            l => Some(l.to_string()),
        };
        let _ = self.engine.send(EngineCmd::Job(Job {
            id,
            session: 0,
            phrase: 0,
            kind: JobKind::Agent,
            audio: Arc::new(audio),
            range: (p.start, p.end),
            opts: DecodeOpts { hotwords: vec![], language },
        }));
    }

    pub(super) fn on_stt(&mut self, id: u64, result: Result<Transcript, String>) {
        let Some(p) = self.pending.remove(&id) else { return };
        let text = match result {
            // Only the phrase itself, not the context decoded in front of it.
            Ok(t) if p.ctx_s > 0.0 => t.after(p.ctx_s - 0.05),
            Ok(t) => t.text.trim().to_string(),
            Err(e) => {
                log::warn!("agent transcription: {e}");
                String::new()
            }
        };
        if let Some((pcm, rate)) = p.native {
            if !text.is_empty() && !self.a().tts.pinned {
                self.shared.voice.lock().offer(pcm, rate, &text);
            }
        }
        if text.is_empty() || !self.enabled {
            if p.talk {
                self.maybe_finish_talk();
            }
            return;
        }
        log::debug!("agent heard {}{}: {text}", if p.label.is_empty() { "me" } else { &p.label }, if p.private { " (private)" } else { "" });
        if p.hear.is_some() {
            let name = p.speaker.map(|s| self.speakers.label(s)).unwrap_or_default();
            self.push_voice_line(p.at, Who::Them, &p.label, p.speaker, &name, &text);
            self.on_heard_while_speaking(&text, p.secs);
            if self.autopilot && !self.talking {
                self.schedule_reply(&text, p.turn_done);
            }
        } else if p.talk {
            self.talk_texts.push(text);
            self.maybe_finish_talk();
        } else if p.private {
            // Autopilot hears everything you say as meant for it: skip "yeah", "mm", noise, and
            // anything that is just the agent's own words coming back.
            use mimic_core::agent::turn::{decide, Turn};
            if decide(&text) == Turn::Skip || text.chars().filter(|c| c.is_alphanumeric()).count() < 4 || self.echoes_agent(&text) {
                log::debug!("mic (autopilot): ignored {text:?}");
                return;
            }
            self.push_line(p.at, Who::Private, "", &text);
            self.private(text);
        } else if p.convo {
            let ww = self.a().wakeword.clone();
            match wake::strip(&text, &ww) {
                Some(rest) if !rest.is_empty() => {
                    self.push_line(p.at, Who::Private, "", &rest);
                    self.private(rest);
                }
                Some(_) => self.overlay.notice("Yes?", false),
                None => {
                    self.push_line(p.at, Who::Me, "", &text);
                }
            }
        }
    }

    /// Is `text` (heard on your mic) one of the agent's recent lines coming back?
    fn echoes_agent(&self, text: &str) -> bool {
        let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        let t = norm(text);
        if t.len() < 4 {
            return false;
        }
        let since = now_ms().saturating_sub(30_000);
        self.timeline.lines().rev().filter(|l| l.who == Who::Agent && l.at >= since).take(4).any(|l| {
            let a = norm(&mimic_core::agent::stream::spoken_words(&l.text));
            a.contains(&t) || (t.len() >= a.len() / 2 && t.contains(&a) && !a.is_empty())
        })
    }

    /// Whether and when to answer what they just said: acknowledgements get no answer,
    /// questions always do; `turn_done` (the end-of-turn model said they've finished) answers
    /// at once, otherwise after the configured quiet (longer if they sound mid-sentence).
    pub(super) fn schedule_reply(&mut self, text: &str, turn_done: bool) {
        use mimic_core::agent::turn::{decide, Turn};
        let turn = decide(text);
        log::debug!("turn: {turn:?}{} for {text:?}", if turn_done { " (finished)" } else { "" });
        let wait = self.a().reply_after_ms as u64;
        let quiet = self.hears.iter().filter_map(|h| h.seg.silence_ms()).min().unwrap_or(0);
        let (extra, forced) = match turn {
            Turn::Skip => {
                self.reply_due = None;
                return;
            }
            Turn::Answer => (0, true),
            Turn::Maybe => (0, false),
            // The model heard a finished turn even without punctuation.
            Turn::Wait if turn_done => (0, false),
            // Probably mid-sentence: give them longer to go on.
            Turn::Wait => (1200, false),
        };
        if turn_done {
            self.reply_forced = forced;
            self.reply_due = Some(std::time::Instant::now());
            return;
        }
        self.reply_forced = forced;
        self.reply_due = Some(std::time::Instant::now() + std::time::Duration::from_millis(wait.saturating_sub(quiet) + extra));
    }

    /// After the talk shortcut is released and every phrase is transcribed: act on it.
    pub(super) fn maybe_finish_talk(&mut self) {
        if !self.talk_released || self.pending.values().any(|p| p.talk) {
            return;
        }
        self.talk_released = false;
        let text = std::mem::take(&mut self.talk_texts).join(" ");
        if text.trim().is_empty() {
            self.overlay.notice("Didn't catch that", true);
            return;
        }
        self.push_line(now_ms(), Who::Private, "", &text);
        self.private(text);
    }
}
