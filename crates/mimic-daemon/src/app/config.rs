//! Applying a new configuration and saving rule changes.

use std::time::{Duration, Instant};

use mimic_core::config::{Config, Residency};
use mimic_core::protocol::*;
use serde_json::json;

use crate::agent::Msg as AgentMsg;
use crate::audio::AudioCmd;
use crate::dsp::Ring;
use crate::engine::{self, EngineCmd, EngineSpec};
use crate::hotkeys;
use crate::platform;

use super::{App, S};

impl App {
    // ------------------------------------------------------------- config

    pub(super) fn apply_config(&mut self, next: Config) -> Result<(), String> {
        let prev = std::mem::replace(&mut self.cfg, next);
        self.cfg.save().map_err(|e| format!("{e:#}"))?;
        if prev.hotkeys != self.cfg.hotkeys {
            hotkeys::set_bindings(&self.cfg.hotkeys);
        }
        let spec_prev = EngineSpec::from_config(&prev);
        let spec = EngineSpec::from_config(&self.cfg);
        if spec_prev != spec || (prev.model.device != self.cfg.model.device) {
            self.model = ModelStatus { id: spec.id.clone(), name: spec.name(), ..Default::default() };
            let _ = self.engine.send(EngineCmd::Configure(spec));
            if self.cfg.general.onboarded && self.cfg.model.residency != Residency::OnDemand && engine::installed(&self.cfg.model.id) {
                let _ = self.engine.send(EngineCmd::Load { urgent: false });
            }
        }
        if prev.model.residency != self.cfg.model.residency {
            match self.cfg.model.residency {
                Residency::Always if self.model.phase == ModelPhase::Unloaded => {
                    let _ = self.engine.send(EngineCmd::Load { urgent: false });
                }
                Residency::OnDemand if self.session.is_none() => self.unload_at = Some(Instant::now() + Duration::from_secs(20)),
                _ => {}
            }
        }
        if prev.audio.device != self.cfg.audio.device && self.mic_open {
            let _ = self.audio.send(AudioCmd::Open { device: self.cfg.audio.device.clone(), gain: self.cfg.audio.gain });
        }
        if prev.audio.gain != self.cfg.audio.gain {
            let _ = self.audio.send(AudioCmd::SetGain(self.cfg.audio.gain));
        }
        if prev.audio.preroll_ms != self.cfg.audio.preroll_ms {
            self.ring = Ring::new(S(self.cfg.audio.preroll_ms as f32 / 1000.0).max(1));
        }
        // The agent listens continuously: it needs the mic open.
        let want_open = (self.cfg.audio.keep_mic_open && self.cfg.general.onboarded) || self.cfg.agent.enabled;
        if want_open && !self.mic_open {
            self.open_mic();
        } else if !want_open && self.mic_open && self.session.is_none() && !self.hub.wants_levels() {
            self.close_mic();
        }
        if prev.general.autostart != self.cfg.general.autostart {
            if let Err(e) = platform::set_autostart(self.cfg.general.autostart) {
                log::warn!("autostart: {e:#}");
            }
        }
        if prev.ui != self.cfg.ui {
            self.overlay.configure(&self.cfg.ui);
        }
        if !prev.general.onboarded && self.cfg.general.onboarded && self.cfg.model.residency != Residency::OnDemand && self.model.phase == ModelPhase::Unloaded
        {
            let _ = self.engine.send(EngineCmd::Load { urgent: false });
        }
        if prev.agent != self.cfg.agent || prev.model.language != self.cfg.model.language || prev.audio.device != self.cfg.audio.device {
            self.agent.send(AgentMsg::Config(Box::new(self.cfg.clone())));
        }
        self.hub.broadcast("config", self.cfg.to_value());
        self.publish();
        Ok(())
    }

    pub(super) fn rules_changed(&mut self) {
        self.matcher = self.rules.matcher();
        if let Err(e) = self.rules.save() {
            log::error!("saving rules: {e:#}");
        }
        self.hub.broadcast("rules", json!({"count": self.rules.rules.len()}));
    }
}
