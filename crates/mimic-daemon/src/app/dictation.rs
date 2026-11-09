//! Dictation sessions (start, release, stop, cancel, finish) and the push-to-talk / hands-free hotkeys that drive them.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mimic_core::config::Residency;
use mimic_core::protocol::*;
use serde_json::json;

use crate::agent::Msg as AgentMsg;
use crate::engine::EngineCmd;
use crate::hotkeys::{self, HotkeyEv};
use crate::overlay::OvState;
use crate::platform;
use crate::sounds::{self, Sound};

use super::{now_ms, App, Mode, Open, Session, RELEASE_PAUSE_MS, S, TAP_MAX};

impl App {
    // ------------------------------------------------------------- sessions

    pub(super) fn start_session(&mut self, mode: Mode) {
        if !self.cfg.general.onboarded && !self.mic_open {
            // First use before onboarding finished still works.
        }
        self.last_used = Instant::now();
        self.unload_at = None;
        match self.model.phase {
            ModelPhase::Ready | ModelPhase::Loading => {}
            ModelPhase::Installing => {
                self.overlay.notice("The model is still installing…", true);
                return;
            }
            ModelPhase::NotInstalled => {
                self.overlay.notice("Model not installed. Open Mimic to set it up.", true);
                sounds::play(Sound::Error, &self.cfg.general);
                platform::open_ui(&["--page", "model"]);
                return;
            }
            _ => {
                let _ = self.engine.send(EngineCmd::Load { urgent: true });
            }
        }
        self.load_at = None;
        let mut audio = Vec::new();
        if self.mic_open && self.cfg.audio.keep_mic_open {
            audio = self.ring.last(S(self.cfg.audio.preroll_ms as f32 / 1000.0));
        }
        self.open_mic();
        let ctx = platform::focus_ctx();
        let id = self.next_id;
        self.next_id += 1;
        let start_pos = 0usize;
        let pre = audio.len();
        self.agent.send(AgentMsg::Dictating(true));
        self.session = Some(Session {
            id,
            mode,
            started: Instant::now(),
            audio,
            base: start_pos,
            vad: self.vad_mem.carry(),
            block_pos: 0,
            open: Open { start: 0, ..Default::default() },
            phrases: Vec::new(),
            emitted: 0,
            preview_inflight: false,
            preview_from: usize::MAX,
            preview_end: 0,
            last_preview: None,
            preview_text: String::new(),
            capturing: true,
            released: None,
            stop_at: None,
            tap_since: None,
            ignore_next_up: false,
            add_rule_after: false,
            cancelled: false,
            pending_edits: 0,
            last_done: None,
            text: String::new(),
            raw: String::new(),
            app: ctx.app.clone(),
            error: None,
        });
        log::debug!("session {id} started ({mode:?}, {pre} samples pre-roll)");
        hotkeys::ACTIVE.store(true, Ordering::Relaxed);
        sounds::play(Sound::Start, &self.cfg.general);
        self.overlay.show(self.overlay_state());
        if pre > 0 {
            self.segment();
        }
        self.publish();
    }

    pub(super) fn overlay_state(&self) -> OvState {
        let Some(s) = &self.session else { return OvState::Hidden };
        if self.model.phase != ModelPhase::Ready {
            return OvState::Loading { progress: self.model.progress };
        }
        if s.released.is_some() || !s.capturing {
            return OvState::Finishing;
        }
        match s.mode {
            Mode::Hold => OvState::Listening,
            Mode::HandsFree => OvState::HandsFree,
        }
    }

    /// Stop listening (release / hands-free stop). Post-roll keeps capturing.
    pub(super) fn release(&mut self) {
        let postroll = Duration::from_millis(self.cfg.audio.postroll_ms as u64);
        let Some(s) = self.session.as_mut() else { return };
        if s.released.is_some() {
            return;
        }
        s.released = Some(Instant::now());
        s.stop_at = Some(Instant::now() + postroll);
        s.tap_since = None;
        // Already silent: finalize right now, post-roll only catches stragglers.
        let quiet = s.open.voiced_blocks >= 1 && s.open.silence >= S(RELEASE_PAUSE_MS as f32 / 1000.0);
        if quiet {
            self.flush_open(false);
        }
        self.overlay.show(self.overlay_state());
        self.publish();
    }

    pub(super) fn stop_capture(&mut self) {
        let Some(s) = self.session.as_mut() else { return };
        if !s.capturing {
            return;
        }
        s.capturing = false;
        self.vad_mem = s.vad.carry();
        self.flush_open(true);
        if !self.cfg.audio.keep_mic_open {
            self.mic_close_at = Some(Instant::now() + Duration::from_secs(4));
        }
        self.maybe_finish();
    }

    pub(super) fn cancel(&mut self, quiet: bool) {
        if let Some(s) = self.session.take() {
            let _ = self.engine.send(EngineCmd::Cancel(s.id));
            log::debug!("session {} cancelled", s.id);
        }
        hotkeys::ACTIVE.store(false, Ordering::Relaxed);
        if quiet {
            self.overlay.hide();
        } else {
            sounds::play(Sound::Cancel, &self.cfg.general);
            self.overlay.flash("Cancelled", false);
        }
        self.after_session();
        self.publish();
    }

    pub(super) fn maybe_finish(&mut self) {
        let done = match &self.session {
            Some(s) => !s.capturing && s.emitted == s.phrases.len() && s.pending_edits == 0,
            None => false,
        };
        if !done {
            return;
        }
        let s = self.session.take().unwrap();
        hotkeys::ACTIVE.store(false, Ordering::Relaxed);
        let typed = !s.text.trim().is_empty();
        if typed {
            sounds::play(Sound::Stop, &self.cfg.general);
            let duration_s = s.started.elapsed().as_secs_f32();
            let latency = match (s.released, s.last_done) {
                (Some(r), Some(d)) if d >= r => d.duration_since(r).as_millis() as u32,
                _ => 0,
            };
            if latency > 0 {
                self.record_latency(latency);
            }
            self.stats.dictations += 1;
            self.stats.words += s.text.split_whitespace().count() as u64;
            self.stats.audio_seconds += duration_s as f64;
            if self.cfg.general.history {
                let item =
                    HistoryItem { id: s.id, at: now_ms(), text: s.text.clone(), raw: s.raw.clone(), app: s.app.clone(), duration_s, latency_ms: latency };
                self.hub.broadcast("dictation", serde_json::to_value(&item).unwrap_or_default());
                self.history.push_front(item);
                self.history.truncate(50);
            }
            self.overlay.hide();
        } else if s.add_rule_after {
            self.overlay.hide();
        } else {
            match &s.error {
                Some(e) => self.overlay.flash(e, true),
                None => self.overlay.flash("No speech detected", false),
            }
        }
        log::debug!("session {} finished: {:?}", s.id, s.text);
        if s.add_rule_after {
            self.reply_popup();
        }
        self.after_session();
        self.publish();
    }

    /// Soundboard slot `i` (0-based): play, or stop if playing.
    fn play_sound(&mut self, i: usize) {
        if let Err(e) = self.soundboard.toggle(&self.cfg.soundboard, i) {
            self.overlay.notice(&format!("{e:#}"), true);
        }
    }

    fn after_session(&mut self) {
        self.agent.send(AgentMsg::Dictating(false));
        self.last_used = Instant::now();
        if self.cfg.model.residency == Residency::OnDemand {
            self.unload_at = Some(Instant::now() + Duration::from_secs(20));
        }
    }

    fn record_latency(&mut self, ms: u32) {
        self.latencies.push_back(ms);
        if self.latencies.len() > 50 {
            self.latencies.pop_front();
        }
        self.stats.last_latency_ms = Some(ms);
    }

    // ------------------------------------------------------------- hotkeys

    pub(super) fn on_hotkey(&mut self, h: HotkeyEv) {
        if let HotkeyEv::Recorded(hk) = &h {
            if let Some((r, _)) = self.record.take() {
                hotkeys::RECORDING.store(false, Ordering::SeqCst);
                match hk {
                    Some(k) => r.ok(json!({"hotkey": k.to_string()})),
                    None => r.err("That isn't a usable shortcut. Use a key, or at least two modifiers (e.g. Ctrl+Win)."),
                }
            }
            return;
        }
        log::debug!("hotkey {h:?}");
        match h {
            HotkeyEv::PttDown => match self.session.as_mut() {
                None => self.start_session(Mode::Hold),
                Some(s) if s.tap_since.is_some() => {
                    // Double tap: lock hands-free.
                    s.tap_since = None;
                    s.mode = Mode::HandsFree;
                    s.ignore_next_up = true;
                    self.overlay.show(self.overlay_state());
                    self.publish();
                }
                Some(s) if s.mode == Mode::HandsFree && s.released.is_none() => {
                    s.ignore_next_up = true;
                    self.release();
                }
                Some(_) => {}
            },
            HotkeyEv::PttUp => {
                let Some(s) = self.session.as_mut() else { return };
                if s.ignore_next_up {
                    s.ignore_next_up = false;
                    return;
                }
                if s.mode != Mode::Hold || s.released.is_some() {
                    return;
                }
                if self.cfg.hotkeys.double_tap_lock && s.started.elapsed() < TAP_MAX && s.open.voiced_blocks < 3 {
                    s.tap_since = Some(Instant::now());
                    return;
                }
                self.release();
            }
            HotkeyEv::HandsFree => match self.session.as_mut() {
                None => self.start_session(Mode::HandsFree),
                Some(s) if s.released.is_none() && s.mode == Mode::Hold => {
                    s.mode = Mode::HandsFree;
                    s.tap_since = None;
                    s.ignore_next_up = true;
                    self.overlay.show(self.overlay_state());
                    self.publish();
                }
                Some(s) if s.released.is_none() => {
                    let _ = s;
                    self.release();
                }
                Some(_) => {}
            },
            HotkeyEv::Cancel => {
                if self.session.is_some() {
                    self.cancel(false);
                }
            }
            HotkeyEv::OtherKey => {
                // Ctrl+Win+Arrow and friends: the user wanted a system shortcut.
                let quiet_cancel = self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.mode == Mode::Hold && s.released.is_none() && s.emitted == 0 && s.started.elapsed() < Duration::from_millis(1500));
                if quiet_cancel {
                    self.cancel(true);
                }
            }
            HotkeyEv::AddRule => self.open_rule_popup(),
            HotkeyEv::Agent(k, down) => {
                // Ctrl+Win (push-to-talk) is held on the way to Ctrl+Win+<key>: drop that fresh dictation.
                let fresh = self.session.as_ref().is_some_and(|s| s.mode == Mode::Hold && s.emitted == 0 && s.started.elapsed() < Duration::from_millis(1500));
                if down && fresh {
                    self.cancel(true);
                }
                self.agent.send(AgentMsg::Hotkey(k, down));
            }
            HotkeyEv::Sound(i) => self.play_sound(i),
            HotkeyEv::Recorded(_) => {}
        }
    }
}
