//! Voice awareness for the agent: the end-of-turn model, Silero VAD for its streams, and the
//! speaker book that tells the voices on a call apart and remembers their names.
//!
//! The models load in the background when the agent turns on (downloaded once, ~30 MB); until
//! they are there the agent uses the energy VAD and fixed pauses, and lines aren't labelled.

use std::path::PathBuf;

use mimic_core::agent::route::SpeakerRef;
use mimic_core::agent::speakers::{Assigned, SpeakerBook};
use mimic_core::agent::Who;

use super::{now_ms, Agent, Msg, ServiceEv};
use crate::voiceaware::{self, Embedder, TurnDetector};

/// Smart Turn's "finished" threshold (probability). Measured: finished sentences 0.73-0.99,
/// unfinished ones 0.01-0.10.
pub const TURN_DONE: f32 = 0.5;
/// Silence before asking whether the turn is over.
pub const PROBE_MS: usize = 200;
/// ... after a short phrase ("Hey!", "So,"), which the model tends to call finished although a
/// greeting is usually followed by the actual turn.
pub const PROBE_SHORT_MS: usize = 550;
const SHORT_S: f32 = 1.2;
/// Shortest speech worth an embedding (shorter clips can't be told apart reliably).
const MIN_EMBED_S: f32 = 0.8;

pub struct Loaded {
    pub turn: Option<TurnDetector>,
    pub embed: Option<Embedder>,
}

pub(super) fn speakers_path() -> PathBuf {
    mimic_core::paths::data_dir().join("speakers.json")
}

pub(super) fn load_speakers() -> SpeakerBook {
    std::fs::read(speakers_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

impl Agent {
    /// Load (and if needed download) the voice models in the background.
    pub(super) fn start_voiceaware(&mut self) {
        if self.voiceaware_loading || (self.turn.is_some() && self.embed.is_some()) {
            return;
        }
        self.voiceaware_loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let t2 = tx.clone();
            let p = move |d: &str, f: Option<f32>| {
                let _ = t2.send(Msg::Service(ServiceEv::Progress { tts: false, detail: d.into(), progress: f }));
            };
            let r = voiceaware::ensure(&p).map(|()| Loaded {
                turn: TurnDetector::load().map_err(|e| log::warn!("end-of-turn model: {e:#}")).ok(),
                embed: Embedder::load().map_err(|e| log::warn!("speaker model: {e:#}")).ok(),
            });
            let _ = tx.send(Msg::Service(ServiceEv::Voiceaware(r.map_err(|e| format!("{e:#}")))));
        });
    }

    pub(super) fn on_voiceaware(&mut self, r: Result<Loaded, String>) {
        self.voiceaware_loading = false;
        match r {
            Ok(l) => {
                log::info!("voice models ready (turn: {}, speakers: {})", l.turn.is_some(), l.embed.is_some());
                self.turn = l.turn;
                self.embed = l.embed;
                self.attach_silero();
            }
            Err(e) => log::warn!("voice models unavailable: {e}"),
        }
    }

    /// Give every stream its own Silero VAD (they keep per-stream state).
    pub(super) fn attach_silero(&mut self) {
        if !voiceaware::path(&voiceaware::SILERO).exists() {
            return;
        }
        let mk = || voiceaware::Vad::load().map_err(|e| log::warn!("silero vad: {e:#}")).ok();
        self.mic_seg.set_silero(mk());
        for h in &mut self.hears {
            h.seg.set_silero(mk());
        }
    }

    /// After a short pause in stream `i`: is their turn over? Then close the phrase now.
    pub(super) fn probe_turn(&mut self, i: usize) -> bool {
        if !self.a().turn_detection {
            return false;
        }
        let Some(t) = self.turn.as_mut() else { return false };
        let h = &mut self.hears[i];
        let wait = if h.seg.open_speech_s() < SHORT_S { PROBE_SHORT_MS } else { PROBE_MS };
        if !h.seg.probe_due(wait) {
            return false;
        }
        let t0 = std::time::Instant::now();
        match t.complete(&h.seg.tail(8.0)) {
            Ok(p) => {
                log::debug!("turn probe ({}): {p:.2} in {} ms", h.label, t0.elapsed().as_millis());
                p >= TURN_DONE
            }
            Err(e) => {
                log::warn!("end-of-turn model failed ({e:#}); turning it off");
                self.turn = None;
                false
            }
        }
    }

    /// Who said this phrase (`audio` at 16 kHz, `secs` of speech in it).
    pub(super) fn identify(&mut self, audio: &[f32], secs: f32) -> Option<u32> {
        if !self.a().speaker_id || secs < MIN_EMBED_S {
            return None;
        }
        let e = self.embed.as_mut()?;
        let t0 = std::time::Instant::now();
        let emb = match e.embed(audio) {
            Ok(v) => v,
            Err(err) => {
                log::warn!("speaker model failed ({err:#}); turning it off");
                self.embed = None;
                return None;
            }
        };
        let r = self.speakers.assign(&emb, secs, now_ms());
        log::debug!("voice: {r:?} ({secs:.1}s, {} ms)", t0.elapsed().as_millis());
        if matches!(r, Assigned::New(_) | Assigned::Known(_)) {
            self.save_speakers();
        }
        r.id()
    }

    pub(super) fn save_speakers(&self) {
        if let Ok(b) = serde_json::to_vec(&self.speakers) {
            let _ = mimic_core::paths::write_atomic(&speakers_path(), &b);
        }
    }

    /// The voices in the recent conversation, as the model should see them.
    pub(super) fn recent_voices(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for l in self.timeline.lines().filter(|l| l.who == Who::Them) {
            if !l.name.is_empty() && !v.contains(&l.name) {
                v.push(l.name.clone());
            }
        }
        v
    }

    /// "That was Ben": name a voice and relabel what it said.
    pub(super) fn name_speaker(&mut self, target: SpeakerRef, name: &str) -> Result<String, String> {
        let id = match target {
            SpeakerRef::Last => self.timeline.lines().rev().find(|l| l.who == Who::Them).and_then(|l| l.speaker).ok_or("I haven't told anyone apart yet")?,
            SpeakerRef::Id(n) => n,
        };
        let old = self.speakers.label(id);
        if !self.speakers.rename(id, name) {
            return Err(format!("there is no {old}"));
        }
        self.save_speakers();
        let label = self.speakers.label(id);
        for l in self.timeline.rename_speaker(id, &label) {
            self.hub.broadcast("agent.line", serde_json::to_value(&l).unwrap_or_default());
        }
        self.publish(true);
        Ok(format!("{old} is {label}"))
    }

    /// Resolve "last" / "Speaker 2" / a name from a tool call.
    pub(super) fn speaker_ref(&self, s: &str) -> SpeakerRef {
        let t = s.trim().to_lowercase();
        if let Some(n) = t.strip_prefix("speaker").map(str::trim).and_then(|n| n.parse::<u32>().ok()) {
            return SpeakerRef::Id(n);
        }
        if let Some(id) = self.speakers.by_name(s) {
            return SpeakerRef::Id(id);
        }
        SpeakerRef::Last
    }
}
