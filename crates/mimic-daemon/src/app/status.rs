//! Status snapshot sent to the UI and tray, and user-facing notices.

use std::sync::atomic::Ordering;

use mimic_core::config::Residency;
use mimic_core::protocol::*;
use serde_json::json;

use crate::hotkeys;

use super::{App, Mode};

impl App {
    // ------------------------------------------------------------- status

    pub(super) fn status(&self) -> Status {
        let phase = match &self.session {
            None => DictationPhase::Idle,
            Some(s) if !s.capturing || s.released.is_some() => DictationPhase::Finishing,
            Some(s) if s.mode == Mode::HandsFree => DictationPhase::HandsFree,
            Some(_) => DictationPhase::Listening,
        };
        let mut model = self.model.clone();
        if model.phase == ModelPhase::Ready && self.cfg.model.residency == Residency::IdleUnload && self.session.is_none() {
            let idle = self.last_used.elapsed().as_secs();
            model.unload_in_s = Some((self.cfg.model.idle_unload_min as u64 * 60).saturating_sub(idle));
        }
        let mut stats = self.stats.clone();
        if !self.latencies.is_empty() {
            let mut v: Vec<u32> = self.latencies.iter().copied().collect();
            v.sort_unstable();
            stats.median_latency_ms = Some(v[v.len() / 2]);
        }
        Status {
            version: mimic_core::VERSION.into(),
            pid: std::process::id(),
            uptime_s: self.started.elapsed().as_secs(),
            phase,
            model,
            mic: self.mic.clone(),
            stats,
            onboarded: self.cfg.general.onboarded,
            config_path: mimic_core::paths::config_file().display().to_string(),
            data_path: mimic_core::paths::data_dir().display().to_string(),
            hotkeys: HotkeyStatus {
                push_to_talk: self.cfg.hotkeys.push_to_talk.to_string(),
                hands_free: self.cfg.hotkeys.hands_free.to_string(),
                add_rule: self.cfg.hotkeys.add_rule.to_string(),
                paused: hotkeys::PAUSED.load(Ordering::Relaxed),
            },
            install: self.install_info.clone(),
            update: self.updater.status(self.updates_on()),
            agent: self.agent.status(),
        }
    }

    pub(super) fn publish(&mut self) {
        let st = self.status();
        self.tray.update(&st);
        self.hub.broadcast("status", serde_json::to_value(&st).unwrap_or_default());
    }

    pub(super) fn notice(&self, kind: &str, text: &str) {
        self.hub.broadcast("notice", json!({"kind": kind, "text": text}));
    }
}
