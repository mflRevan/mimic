//! Phrase segmentation: splitting session audio at pauses, final decodes, live previews.

use std::sync::Arc;

use mimic_core::models::{self, EngineKind};
use mimic_core::protocol::*;

use crate::dsp::{BLOCK, SR};
use crate::engine::{DecodeOpts, EngineCmd, Job, JobKind};
use crate::platform;

use super::{App, Open, Phrase, RELEASE_PAUSE_MS, S, SEAM_TAIL_S};

impl App {
    // ------------------------------------------------------------- segmentation

    pub(super) fn segment(&mut self) {
        let pause_ms = {
            let s = self.session.as_ref().unwrap();
            if s.released.is_some() {
                RELEASE_PAUSE_MS.min(self.cfg.audio.pause_ms)
            } else {
                self.cfg.audio.pause_ms
            }
        };
        let pause = S(pause_ms as f32 / 1000.0);
        let max_phrase = S(self.cfg.audio.max_phrase_s.max(5) as f32);
        let mut closes = Vec::new();
        {
            let s = self.session.as_mut().unwrap();
            let mut blocks = Vec::new();
            let from = s.block_pos;
            let avail = s.pos() - from;
            let n = avail / BLOCK * BLOCK;
            if n == 0 {
                return;
            }
            let pcm = s.slice(from, from + n);
            s.vad.feed(&pcm, &mut blocks);
            for (i, b) in blocks.iter().enumerate() {
                let bs = from + i * BLOCK;
                let be = bs + BLOCK;
                let o = &mut s.open;
                if b.voiced {
                    if o.speech_start.is_none() {
                        o.speech_start = Some(bs);
                    }
                    o.speech_end = be;
                    o.voiced_blocks += 1;
                    o.silence = 0;
                } else {
                    o.silence += BLOCK;
                    if o.speech_start.is_none() && be - o.start > S(0.45) {
                        // Keep only a little lead-in before speech starts.
                        o.start = be - S(0.4);
                    }
                }
                let voiced = o.voiced_blocks >= 2;
                if (voiced && o.silence >= pause) || be - o.start >= max_phrase {
                    let closed = std::mem::replace(&mut s.open, Open { start: be, ..Default::default() });
                    if closed.voiced_blocks >= 2 {
                        closes.push((closed, be));
                    }
                }
            }
            s.block_pos = from + n;
        }
        for (o, end) in closes {
            self.finalize(o, end);
        }
        self.maybe_preview();
        self.trim_session_audio();
    }

    pub(super) fn flush_open(&mut self, force: bool) {
        let Some(s) = self.session.as_mut() else { return };
        let o = std::mem::take(&mut s.open);
        let pos = s.pos();
        s.open = Open { start: pos, ..Default::default() };
        if o.voiced_blocks >= 2 || (force && o.voiced_blocks >= 1) {
            self.finalize(o, pos);
        }
    }

    fn finalize(&mut self, o: Open, end: usize) {
        let joint = self.cfg.output.continuation && self.cfg.output.seam_repair && self.engine_is_local();
        let cont_ok = self.continuation_valid(&platform::focus_ctx());
        let s = self.session.as_mut().unwrap();
        let speech_start = o.speech_start.unwrap_or(o.start);
        let speech_end = o.speech_end.max(speech_start + 1).min(end);
        let decode_start = o.start.max(speech_start.saturating_sub(S(0.3)));
        let decode_end = end.min(speech_end + S(0.35));
        let idx = s.phrases.len();

        // A preview that already covered the whole phrase is as good as a final.
        if let Some(((ps, pe), tr)) = &s.last_preview {
            let no_seam = !(joint && (idx > 0 || cont_ok));
            if *ps == o.start && *pe >= speech_end + S(0.25) && no_seam {
                let mut tr = tr.clone();
                let shift = (decode_start - o.start) as f32 / SR as f32;
                for w in tr.words.iter_mut() {
                    w.start = (w.start - shift).max(0.0);
                    w.end = (w.end - shift).max(0.0);
                }
                s.phrases.push(Phrase { end, speech_start, speech_end, decode_start, boundary_s: None, result: Some(Ok(tr)) });
                s.last_preview = None;
                self.emit_ready();
                return;
            }
        }

        // Seam context: previous phrase in this session, or the previous dictation.
        let mut ctx_audio: Vec<f32> = Vec::new();
        if joint {
            if idx > 0 {
                let p = &s.phrases[idx - 1];
                let a = p.speech_start.max(p.speech_end.saturating_sub(S(SEAM_TAIL_S)));
                ctx_audio = s.slice(a, p.speech_end + S(0.12));
            } else if cont_ok {
                if let Some(l) = &self.last {
                    ctx_audio = l.tail_audio.clone();
                }
            }
        }
        let s = self.session.as_mut().unwrap();
        let new_start = if ctx_audio.is_empty() { decode_start } else { decode_start.max(speech_start.saturating_sub(S(0.12))) };
        let mut audio = ctx_audio.clone();
        audio.extend(s.slice(new_start, decode_end));
        let boundary_s = (!ctx_audio.is_empty()).then(|| ctx_audio.len() as f32 / SR as f32);
        s.phrases.push(Phrase { end, speech_start, speech_end, decode_start: new_start, boundary_s, result: None });
        s.last_preview = None;
        let job = Job {
            id: self.next_id,
            session: s.id,
            phrase: idx,
            kind: JobKind::Final,
            audio: Arc::new(audio),
            range: (new_start, decode_end),
            opts: self.decode_opts(),
        };
        self.next_id += 1;
        let _ = self.engine.send(EngineCmd::Job(job));
    }

    pub(super) fn maybe_preview(&mut self) {
        if self.model.phase != ModelPhase::Ready {
            return;
        }
        let interval = S(self.cfg.audio.preview_interval_ms.max(150) as f32 / 1000.0);
        let opts = self.decode_opts();
        let Some(s) = self.session.as_mut() else { return };
        if s.preview_inflight || !s.capturing {
            return;
        }
        let o = &s.open;
        let Some(ss) = o.speech_start else { return };
        let pos = s.block_pos;
        let first = s.preview_from != o.start;
        let due = if first { pos.saturating_sub(ss) >= S(0.35) } else { pos.saturating_sub(s.preview_end) >= interval };
        // Eager: the speaker went quiet; decode everything now so a release can reuse it.
        let eager = o.silence >= S(0.25) && (first || s.preview_end < o.speech_end + S(0.25));
        if !(due || eager) || o.voiced_blocks < 2 {
            return;
        }
        let range = (o.start, pos);
        let audio = s.slice(range.0, range.1);
        s.preview_inflight = true;
        s.preview_from = o.start;
        s.preview_end = pos;
        let job = Job { id: self.next_id, session: s.id, phrase: usize::MAX, kind: JobKind::Preview, audio: Arc::new(audio), range, opts };
        self.next_id += 1;
        let _ = self.engine.send(EngineCmd::Job(job));
    }

    /// Long hands-free sessions must not grow memory forever.
    fn trim_session_audio(&mut self) {
        let Some(s) = self.session.as_mut() else { return };
        if s.audio.len() < S(90.0) {
            return;
        }
        let mut keep_from = s.open.start;
        if let Some(p) = s.phrases.last() {
            keep_from = keep_from.min(p.speech_end.saturating_sub(S(SEAM_TAIL_S + 0.5)));
        }
        keep_from = keep_from.saturating_sub(S(1.0)).max(s.base);
        let cut = keep_from - s.base;
        if cut > S(30.0) {
            s.audio.drain(..cut);
            s.base = keep_from;
        }
    }

    pub(super) fn decode_opts(&self) -> DecodeOpts {
        let language = match self.cfg.model.language.trim() {
            "" | "auto" => None,
            l => Some(l.to_string()),
        };
        let hotwords = if self.cfg.model.hotwords { self.rules.hotwords(25) } else { vec![] };
        DecodeOpts { hotwords, language }
    }

    fn engine_is_local(&self) -> bool {
        match models::find(&self.cfg.model.id).map(|m| m.engine) {
            Some(EngineKind::Custom) => {
                let u = self.cfg.custom.base_url.to_ascii_lowercase();
                u.contains("://127.") || u.contains("://localhost")
            }
            _ => true,
        }
    }
}
