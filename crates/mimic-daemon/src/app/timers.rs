//! The periodic `tick`: model load/unload timers, update scheduling, mic close and other deadlines.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mimic_core::config::Residency;
use mimic_core::protocol::*;

use crate::engine::{self, EngineCmd};
use crate::hotkeys;

use super::App;

impl App {
    // ------------------------------------------------------------- timers

    pub(super) fn tick(&mut self) {
        let now = Instant::now();
        if self.updater.auto_pref != self.cfg.general.auto_update {
            self.updater.auto_pref = self.cfg.general.auto_update;
            self.updater.set_auto(self.updater.auto_pref);
        }
        if self.updater.due(self.updates_on()) {
            self.updater.check(&self.tx, None);
        }
        // A downloaded update waits for the current dictation to finish (an automatic one also
        // for the agent to be idle).
        if self.session.is_none() && (!self.updater.waits_for_idle() || self.idle_for_update()) {
            if let Some((path, reopen)) = self.updater.take_ready() {
                match crate::updater::launch_installer(&path, reopen) {
                    Ok(()) => {
                        self.notice("info", "Installing the update. Mimic restarts in a moment.");
                        self.quit = true;
                        return;
                    }
                    Err(e) => {
                        log::error!("update: {e:#}");
                        self.updater.set_error(format!("{e:#}"));
                        self.notice("error", "Couldn't start the update installer");
                        self.publish();
                    }
                }
            }
        }
        if let Some(t) = self.load_at {
            if now >= t {
                self.load_at = None;
                if engine::installed(&self.cfg.model.id) && self.model.phase != ModelPhase::Ready {
                    let _ = self.engine.send(EngineCmd::Load { urgent: false });
                }
            }
        }
        if let Some(t) = self.unload_at {
            if now >= t && self.session.is_none() {
                self.unload_at = None;
                let _ = self.engine.send(EngineCmd::Unload);
            }
        }
        if self.cfg.model.residency == Residency::IdleUnload
            && self.model.phase == ModelPhase::Ready
            && self.session.is_none()
            && self.last_used.elapsed() >= Duration::from_secs(self.cfg.model.idle_unload_min.max(1) as u64 * 60)
        {
            log::info!("idle for {} min: unloading model", self.cfg.model.idle_unload_min);
            self.last_used = now;
            let _ = self.engine.send(EngineCmd::Unload);
        }
        if let Some(t) = self.mic_close_at {
            if now >= t && self.session.is_none() {
                self.mic_close_at = None;
                if !self.cfg.agent.enabled {
                    self.close_mic();
                }
            }
        }
        if let Some((_, since)) = &self.record {
            if since.elapsed() > Duration::from_secs(12) {
                let (r, _) = self.record.take().unwrap();
                hotkeys::RECORDING.store(false, Ordering::SeqCst);
                r.err("No shortcut pressed");
            }
        }
        if let Some(t) = self.apply_at {
            if now >= t {
                self.apply_at = None;
                self.apply_last_fix();
            }
        }
        // Session timers.
        let mut release = false;
        let mut stop = false;
        let mut cancel_quiet = false;
        if let Some(s) = self.session.as_mut() {
            if let Some(t) = s.tap_since {
                if t.elapsed() >= Duration::from_millis(self.cfg.hotkeys.double_tap_ms as u64) {
                    s.tap_since = None;
                    if s.open.voiced_blocks == 0 && s.phrases.is_empty() {
                        cancel_quiet = true;
                    } else {
                        release = true;
                    }
                }
            }
            if let Some(t) = s.stop_at {
                if now >= t && s.capturing {
                    stop = true;
                }
            }
        }
        if cancel_quiet {
            self.cancel(true);
        } else if release {
            self.release();
        }
        if stop {
            self.stop_capture();
        }
    }
}
