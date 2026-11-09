//! Engine results: decoded jobs, in-order post-processing and typing of finals, injection acks.

use std::time::{Duration, Instant};

use mimic_core::chunk;
use mimic_core::continuation::{repair_heuristic, repair_with_context, split_joint, Seam};
use mimic_core::protocol::*;
use mimic_core::text::{self, Context};
use serde_json::json;

use crate::agent::Msg as AgentMsg;
use crate::dsp::SR;
use crate::engine::{EngineEv, JobDone, JobKind};
use crate::hotkeys;
use crate::inject::Edit;
use crate::platform::{self, FocusCtx};

use super::{App, LastInsert, S, SEAM_TAIL_S};

impl App {
    // ------------------------------------------------------------- engine results

    pub(super) fn on_engine(&mut self, e: EngineEv) {
        match e {
            EngineEv::State { id, phase, detail, progress, device, memory_mb, error } => {
                if id != self.cfg.model.id {
                    return; // a late event from the previously selected model
                }
                let was_ready = self.model.phase == ModelPhase::Ready;
                if self.installing.as_deref() == Some(self.cfg.model.id.as_str()) && phase != ModelPhase::Loading {
                    return; // the installer owns the status meanwhile
                }
                self.model.phase = phase;
                self.model.detail = detail;
                self.model.progress = progress;
                if !device.is_empty() || phase != ModelPhase::Loading {
                    self.model.device = device;
                }
                self.model.memory_mb = memory_mb;
                self.model.error = error.clone();
                if phase == ModelPhase::Error {
                    if let Some(e) = &error {
                        self.notice("error", &format!("Model failed to load: {}", e.lines().next().unwrap_or("")));
                        if self.session.is_some() {
                            self.overlay.notice("Model failed to load — see Mimic settings", true);
                        }
                    }
                }
                if phase == ModelPhase::Ready && !was_ready {
                    if let Some(own) = platform::own_memory_mb() {
                        log::info!("daemon working set {own} MB");
                    }
                }
                if self.session.is_some() {
                    self.overlay.show(self.overlay_state());
                    if phase == ModelPhase::Ready {
                        self.maybe_preview();
                    }
                }
                self.publish();
            }
            EngineEv::Done(d) => self.on_job_done(d),
        }
    }

    fn on_job_done(&mut self, d: JobDone) {
        if d.kind == JobKind::Agent {
            self.agent.send(AgentMsg::Stt { id: d.id, result: d.result });
            return;
        }
        if d.kind == JobKind::File {
            if let Some(r) = self.file_jobs.remove(&d.id) {
                match d.result {
                    Ok(tr) => {
                        let p = text::process(&tr.words, &self.cfg.text, Some(&self.matcher), Context { sentence_start: true, is_final: true });
                        r.ok(json!({"text": p.text, "raw": tr.text, "words": tr.words, "decode_ms": d.decode_ms}));
                    }
                    Err(e) => r.err(&e),
                }
            }
            return;
        }
        let Some(s) = self.session.as_mut() else { return };
        if s.id != d.session || s.cancelled {
            return;
        }
        match d.kind {
            JobKind::Preview => {
                s.preview_inflight = false;
                if let Ok(tr) = d.result {
                    if d.range.0 == s.open.start {
                        s.preview_text = tr.text.clone();
                        s.last_preview = Some((d.range, tr));
                        if self.cfg.ui.overlay_preview {
                            let shown = format!("{}{}", if s.phrases.len() > s.emitted { "… " } else { "" }, s.preview_text);
                            self.overlay.text(&shown);
                        }
                        self.hub.broadcast("partial", json!({"text": s.preview_text}));
                    }
                }
                self.maybe_preview();
            }
            JobKind::Final => {
                if let Some(p) = s.phrases.get_mut(d.phrase) {
                    p.result = Some(d.result);
                }
                self.emit_ready();
                self.maybe_finish();
            }
            JobKind::File | JobKind::Agent => {}
        }
    }

    pub(super) fn continuation_valid(&self, now: &FocusCtx) -> bool {
        if !self.cfg.output.continuation {
            return false;
        }
        let Some(l) = &self.last else { return false };
        // In test-sink mode the sink file is the text field.
        let test = std::env::var_os("MIMIC_TEST_SINK").is_some();
        (test || (l.ctx == *now && l.epoch == hotkeys::epoch())) && l.at.elapsed() < Duration::from_secs(self.cfg.output.continuation_window_s as u64)
    }

    pub(super) fn emit_ready(&mut self) {
        loop {
            let Some(s) = self.session.as_mut() else { return };
            let k = s.emitted;
            let ready = s.phrases.get(k).and_then(|p| p.result.as_ref()).is_some();
            if !ready {
                break;
            }
            s.emitted += 1;
            let p = &s.phrases[k];
            let tr = match p.result.as_ref().unwrap() {
                Ok(t) => t.clone(),
                Err(e) => {
                    log::warn!("phrase {k} failed: {e}");
                    let msg = format!("Transcription failed: {}", e.lines().next().unwrap_or(""));
                    self.overlay.notice(&msg, true);
                    self.notice("error", &msg);
                    if let Some(s) = self.session.as_mut() {
                        s.error = Some(msg);
                    }
                    continue;
                }
            };
            let boundary = p.boundary_s;
            let (ctx_words, new_words) = match boundary {
                Some(b) => split_joint(&tr.words, b),
                None => (Vec::new(), tr.words.clone()),
            };
            let tail_audio = {
                let a = p.speech_start.max(p.speech_end.saturating_sub(S(SEAM_TAIL_S)));
                s.slice(a, p.speech_end + S(0.12))
            };
            let env = chunk::envelope(&s.slice(p.decode_start, p.end));
            let gap_s = s.phrases.get(k.wrapping_sub(1)).map(|q| (p.speech_start.saturating_sub(q.speech_end)) as f32 / SR as f32).unwrap_or(1.0);
            let is_final = !s.capturing && k + 1 == s.phrases.len();
            if new_words.is_empty() {
                continue;
            }
            let now_ctx = platform::focus_ctx();
            let cont = self.continuation_valid(&now_ctx);
            let seam = if cont {
                let l = self.last.as_ref().unwrap();
                if boundary.is_some() && !ctx_words.is_empty() {
                    repair_with_context(&l.auto_tail, l.ended_sentence, &ctx_words)
                } else {
                    let gap = if k == 0 { l.at.elapsed().as_secs_f32() } else { gap_s };
                    repair_heuristic(&l.auto_tail, l.ended_sentence, &new_words[0].text, gap)
                }
            } else {
                Seam { sentence_start: true, ..Seam::default() }
            };
            let flags = Context { sentence_start: seam.sentence_start, is_final };
            let out = text::process(&new_words, &self.cfg.text, Some(&self.matcher), flags);
            if out.text.is_empty() && seam.is_noop() {
                continue;
            }
            let sep = if cont && self.cfg.output.smart_spacing && needs_space(&out.text) { " " } else { "" };
            let typed = format!("{}{}{}", seam.insert, sep, out.text);
            if !seam.is_noop() {
                log::debug!("seam: -{} +{:?}", seam.backspaces, seam.insert);
            }
            let tag = self.next_id;
            self.next_id += 1;
            let s = self.session.as_mut().unwrap();
            s.pending_edits += 1;
            self.edit_tags.insert(tag, s.id);
            let _ = self.inject.send(Edit {
                backspaces: seam.backspaces,
                text: typed.clone(),
                method: self.cfg.output.method,
                newline: self.cfg.output.newline,
                tag,
            });
            let raw_text = new_words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            for _ in 0..seam.backspaces {
                s.text.pop();
            }
            if !s.text.is_empty() && !typed.starts_with(['\n', ',', '.', ' ']) && !s.text.ends_with(['\n', ' ']) {
                s.text.push(' ');
            }
            s.text.push_str(&typed);
            if !s.raw.is_empty() {
                s.raw.push(' ');
            }
            s.raw.push_str(&raw_text);
            if !out.rules_hit.is_empty() {
                self.rules.record_use(&out.rules_hit);
                let _ = self.rules.save();
            }
            let ended_sentence = out.text.trim_end().ends_with(['.', '!', '?', '…']);
            self.last = Some(LastInsert {
                ctx: now_ctx,
                epoch: hotkeys::epoch(),
                at: Instant::now(),
                auto_tail: out.auto_tail.clone(),
                ended_sentence,
                tail_audio,
                words: new_words,
                env,
                text: out.text,
                flags,
            });
        }
    }

    pub(super) fn on_injected(&mut self, tag: u64, at: Instant) {
        let Some(sid) = self.edit_tags.remove(&tag) else { return };
        if let Some(s) = self.session.as_mut() {
            if s.id == sid {
                s.pending_edits = s.pending_edits.saturating_sub(1);
                s.last_done = Some(at);
                if s.capturing && s.released.is_none() && self.cfg.ui.overlay_preview {
                    self.overlay.text("");
                }
            }
        }
        self.maybe_finish();
    }
}

fn needs_space(t: &str) -> bool {
    !t.starts_with([',', '.', ';', ':', '!', '?', ')', ']', '}', '\n', '%'])
}
