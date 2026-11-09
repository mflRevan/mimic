//! Continuous phrase segmentation for always-on streams (your mic, what you hear), without
//! sessions. Voice is detected with Silero VAD when it is loaded (robust to noise, music and
//! keyboard clicks), else with the energy VAD dictation uses. Positions are absolute 16 kHz
//! sample counts since the stream started, so phrases can be matched against state spans
//! (talking to the agent, dictating, the agent speaking) and against the native-rate mic audio.
//!
//! A short pause raises a one-time "probe" ([`Segmenter::probe_due`]): the caller can ask the
//! end-of-turn model and close the phrase at once ([`Segmenter::flush`]) instead of waiting for
//! the full pause.

use std::collections::VecDeque;

use crate::dsp::{Vad, BLOCK, SR};
use crate::voiceaware;

/// Silero speech probability that counts as voice (and the lower one that keeps it: hysteresis).
const SPEECH_ON: f32 = 0.5;
const SPEECH_KEEP: f32 = 0.35;

const fn s(ms: usize) -> usize {
    ms * SR / 1000
}

#[derive(Debug, Clone)]
pub struct Phrase {
    pub start: usize,
    pub end: usize,
    /// First and last voiced sample (without lead-in/tail).
    pub speech: (usize, usize),
    pub audio: Vec<f32>,
}

#[derive(Debug, Default, Clone)]
struct Open {
    start: usize,
    speech_start: Option<usize>,
    speech_end: usize,
    voiced_blocks: usize,
    silence: usize,
}

pub struct Segmenter {
    vad: Vad,
    silero: Option<voiceaware::Vad>,
    /// Silero frames not yet consumed by blocks: (end position, probability).
    probs: VecDeque<(usize, f32)>,
    sil_pos: usize,
    was_voiced: bool,
    /// The pause probe was raised for the current silence.
    probed: bool,
    /// Audio kept for phrase extraction; `buf[0]` is absolute sample `base`.
    buf: Vec<f32>,
    base: usize,
    pos: usize,
    block_pos: usize,
    open: Open,
    pause: usize,
    max_len: usize,
    /// Consecutive voiced blocks right now (barge-in detection).
    run: usize,
    last_voiced: Option<usize>,
}

impl Segmenter {
    pub fn new(pause_ms: u32, max_s: u32) -> Self {
        Self {
            vad: Vad::default(),
            silero: None,
            probs: VecDeque::new(),
            sil_pos: 0,
            was_voiced: false,
            probed: false,
            buf: Vec::new(),
            base: 0,
            pos: 0,
            block_pos: 0,
            open: Open::default(),
            pause: s(pause_ms.max(150) as usize),
            max_len: SR * max_s.max(5) as usize,
            run: 0,
            last_voiced: None,
        }
    }

    /// Use Silero VAD from now on (its frames line up with the stream from the next chunk).
    pub fn set_silero(&mut self, vad: Option<voiceaware::Vad>) {
        self.silero = vad;
        self.probs.clear();
        self.sil_pos = self.pos;
    }

    /// Once per pause inside a phrase, after `ms` of silence: time to ask whether the turn is over.
    pub fn probe_due(&mut self, ms: usize) -> bool {
        if self.in_phrase() && !self.probed && self.open.silence >= s(ms) {
            self.probed = true;
            return true;
        }
        false
    }

    /// Seconds from the first to the last voiced sample of the open phrase.
    pub fn open_speech_s(&self) -> f32 {
        self.open.speech_start.map_or(0.0, |s| self.open.speech_end.saturating_sub(s) as f32 / SR as f32)
    }

    /// The last `secs` seconds of audio (for the end-of-turn model).
    pub fn tail(&self, secs: f32) -> Vec<f32> {
        let n = ((secs * SR as f32) as usize).min(self.buf.len());
        self.buf[self.buf.len() - n..].to_vec()
    }

    pub fn set_pause(&mut self, pause_ms: u32) {
        self.pause = s(pause_ms.max(150) as usize);
    }

    /// Absolute position (samples fed so far).
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Someone is speaking now: at least `min_ms` of continuous voice up to the latest block.
    pub fn voiced_for(&self, min_ms: usize) -> bool {
        self.run * BLOCK >= s(min_ms)
    }

    /// A phrase is open (voice seen and not yet closed by a pause).
    pub fn in_phrase(&self) -> bool {
        self.open.voiced_blocks >= 2
    }

    /// Milliseconds since the last voiced block (None = nothing voiced yet).
    pub fn silence_ms(&self) -> Option<u64> {
        self.last_voiced.map(|v| (self.block_pos.saturating_sub(v) * 1000 / SR) as u64)
    }

    pub fn feed(&mut self, chunk: &[f32], out: &mut Vec<Phrase>) {
        self.buf.extend_from_slice(chunk);
        self.pos += chunk.len();
        // Silero sees every sample (before the early return below: chunks are smaller than blocks).
        if let Some(v) = self.silero.as_mut() {
            let mut p = Vec::new();
            match v.feed(chunk, &mut p) {
                Ok(()) => {
                    for prob in p {
                        self.sil_pos += voiceaware::VAD_FRAME;
                        self.probs.push_back((self.sil_pos, prob));
                    }
                }
                Err(e) => {
                    log::warn!("silero vad failed ({e:#}); using the energy VAD");
                    self.silero = None;
                    self.probs.clear();
                }
            }
        }
        let n = (self.pos - self.block_pos) / BLOCK * BLOCK;
        if n == 0 {
            return;
        }
        let from = self.block_pos - self.base;
        let pcm: Vec<f32> = self.buf[from..from + n].to_vec();
        let mut blocks = Vec::new();
        self.vad.feed(&pcm, &mut blocks);
        for (i, b) in blocks.iter().enumerate() {
            let bs = self.block_pos + i * BLOCK;
            let be = bs + BLOCK;
            let voiced_now = if self.silero.is_some() {
                // The strongest Silero frame ending inside this block (hysteresis across blocks).
                let mut best: Option<f32> = None;
                while let Some(&(end, p)) = self.probs.front() {
                    if end > be {
                        break;
                    }
                    if end > bs {
                        best = Some(best.map_or(p, |b: f32| b.max(p)));
                    }
                    self.probs.pop_front();
                }
                match best {
                    Some(p) => p >= if self.was_voiced { SPEECH_KEEP } else { SPEECH_ON },
                    None => self.was_voiced && b.voiced,
                }
            } else {
                b.voiced
            };
            self.was_voiced = voiced_now;
            let o = &mut self.open;
            if voiced_now {
                self.probed = false;
                if o.speech_start.is_none() {
                    o.speech_start = Some(bs);
                }
                o.speech_end = be;
                o.voiced_blocks += 1;
                o.silence = 0;
                self.run += 1;
                self.last_voiced = Some(be);
            } else {
                o.silence += BLOCK;
                self.run = 0;
                if o.speech_start.is_none() && be - o.start > s(450) {
                    o.start = be - s(400);
                }
                // A single voiced block (a click, a knock) followed by silence is not a phrase.
                if o.voiced_blocks < 2 && o.silence >= self.pause {
                    *o = Open { start: be - s(400).min(be), ..Default::default() };
                }
            }
            let o = &self.open;
            let voiced = o.voiced_blocks >= 2;
            if (voiced && o.silence >= self.pause) || (voiced && be - o.start >= self.max_len) {
                let closed = std::mem::replace(&mut self.open, Open { start: be, ..Default::default() });
                out.push(self.extract(&closed, be));
            }
        }
        self.block_pos += n;
        self.trim();
    }

    /// Close the open phrase now (e.g. the talk-to-agent key was released).
    pub fn flush(&mut self, out: &mut Vec<Phrase>) {
        if self.open.voiced_blocks >= 2 {
            let end = self.block_pos;
            let closed = std::mem::replace(&mut self.open, Open { start: end, ..Default::default() });
            out.push(self.extract(&closed, end));
        }
    }

    fn extract(&self, o: &Open, end: usize) -> Phrase {
        let ss = o.speech_start.unwrap_or(o.start);
        let a = o.start.max(ss.saturating_sub(s(300))).max(self.base);
        let b = end.min(o.speech_end + s(350)).max(a);
        Phrase { start: a, end: b, speech: (ss, o.speech_end), audio: self.buf[a - self.base..b - self.base].to_vec() }
    }

    /// Bound memory: keep the open phrase, a little before it, and at least the last 8 s.
    fn trim(&mut self) {
        let keep_from = self.open.start.saturating_sub(s(1000)).min(self.pos.saturating_sub(SR * 8)).max(self.base);
        if keep_from - self.base > SR * 10 {
            self.buf.drain(..keep_from - self.base);
            self.base = keep_from;
        }
    }
}

/// Spans of a state over stream positions ("talk key held", "dictating", "agent speaking").
#[derive(Debug, Default)]
pub struct Spans {
    done: Vec<(usize, usize)>,
    open: Option<usize>,
}

impl Spans {
    pub fn set(&mut self, on: bool, pos: usize) {
        match (on, self.open) {
            (true, None) => self.open = Some(pos),
            (false, Some(s)) => {
                self.done.push((s, pos));
                self.open = None;
            }
            _ => {}
        }
    }

    /// Does the state overlap `[a, b)` (with `slack` samples of tolerance after a span ends)?
    pub fn overlaps(&self, a: usize, b: usize, slack: usize) -> bool {
        self.open.is_some_and(|s| s < b) || self.done.iter().any(|&(s, e)| s < b && e + slack > a)
    }

    pub fn prune(&mut self, before: usize) {
        self.done.retain(|&(_, e)| e >= before);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amp: f32) -> Vec<f32> {
        (0..s(ms)).map(|i| (i as f32 * 0.3).sin() * amp).collect()
    }

    #[test]
    fn phrases_close_on_pause() {
        let mut g = Segmenter::new(600, 28);
        let mut out = Vec::new();
        let mut audio = tone(400, 0.0);
        audio.extend(tone(1200, 0.3));
        audio.extend(tone(800, 0.0));
        audio.extend(tone(900, 0.3));
        audio.extend(tone(800, 0.0));
        for c in audio.chunks(160) {
            g.feed(c, &mut out);
        }
        assert_eq!(out.len(), 2);
        let p = &out[0];
        assert!(p.speech.0 >= s(350) && p.speech.0 <= s(450), "{:?}", p.speech);
        assert!(p.audio.len() > s(1200) && p.audio.len() < s(2000));
        assert!(out[1].start > p.end);
        assert!(!g.in_phrase());
        assert!(g.silence_ms().unwrap() >= 600);
    }

    #[test]
    fn clicks_are_ignored_and_barge_in_detects() {
        let mut g = Segmenter::new(600, 28);
        let mut out = Vec::new();
        let mut audio = tone(1000, 0.3); // sets the VAD's loudness reference
        audio.extend(tone(1000, 0.0));
        audio.extend(tone(50, 0.3)); // a click
        audio.extend(tone(1000, 0.0));
        for c in audio.chunks(160) {
            g.feed(c, &mut out);
        }
        assert_eq!(out.len(), 1);
        let mut out = Vec::new();
        for c in tone(300, 0.3).chunks(160) {
            g.feed(c, &mut out);
        }
        assert!(g.voiced_for(250));
        assert!(g.in_phrase());
        g.flush(&mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn spans() {
        let mut sp = Spans::default();
        sp.set(true, 100);
        sp.set(false, 200);
        assert!(sp.overlaps(150, 300, 0));
        assert!(!sp.overlaps(250, 300, 0));
        assert!(sp.overlaps(250, 300, 60));
        sp.set(true, 500);
        assert!(sp.overlaps(600, 700, 0));
        sp.prune(400);
        assert!(!sp.overlaps(0, 300, 0));
    }

    /// `MIMIC_SEG_WAV=<16 kHz wav>` (with the voice models installed): phrases found with Silero.
    #[test]
    fn silero_segments_a_file() {
        let Ok(path) = std::env::var("MIMIC_SEG_WAV") else { return };
        let audio = crate::dsp::read_wav(std::path::Path::new(&path)).unwrap();
        let mut g = Segmenter::new(900, 28);
        let mut v = voiceaware::Vad::load().unwrap();
        let mut probs = Vec::new();
        v.feed(&audio, &mut probs).unwrap();
        println!(
            "rms {:.4} frames {} speech {}",
            (audio.iter().map(|x| x * x).sum::<f32>() / audio.len() as f32).sqrt(),
            probs.len(),
            probs.iter().filter(|p| **p >= 0.5).count()
        );
        g.set_silero(Some(voiceaware::Vad::load().unwrap()));
        let mut out = Vec::new();
        for c in audio.chunks(160) {
            g.feed(c, &mut out);
        }
        assert_eq!(g.sil_pos, probs.len() * voiceaware::VAD_FRAME);
        for p in &out {
            println!("phrase {:.2}-{:.2}s", p.speech.0 as f32 / SR as f32, p.speech.1 as f32 / SR as f32);
        }
        assert!(!out.is_empty());
    }
}
