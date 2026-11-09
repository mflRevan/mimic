//! Model downloads/installs and their progress.

use mimic_core::config::Residency;
use mimic_core::models;
use mimic_core::protocol::*;

use crate::engine::{EngineCmd, EngineSpec};
use crate::platform;

use super::{App, Ev};

impl App {
    // ------------------------------------------------------------- install

    pub(super) fn start_install(&mut self, id: String) -> Result<(), String> {
        if let Some(i) = &self.installing {
            return Err(format!("already installing {i}"));
        }
        if models::find(&id).is_none() {
            return Err(format!("unknown model `{id}`"));
        }
        self.installing = Some(id.clone());
        if id == self.cfg.model.id {
            let _ = self.engine.send(EngineCmd::Unload);
            self.model.phase = ModelPhase::Installing;
            self.model.progress = Some(0.0);
            self.model.detail = "Starting".into();
            self.publish();
        }
        let tx = self.tx.clone();
        let device = self.cfg.model.device;
        std::thread::Builder::new()
            .name("mimic-install".into())
            .spawn(move || {
                #[cfg(windows)]
                let _bg = platform::ThreadPriorityGuard::background(true);
                let send = |step: &str, p: Option<f32>, done: bool, err: Option<String>| {
                    let _ = tx.send(Ev::Install(InstallProgress { id: id.clone(), step: step.into(), progress: p, done, error: err }));
                };
                let progress = |step: &str, p: Option<f32>| send(step, p, false, None);
                match crate::provision::install(&id, device, &progress) {
                    Ok(()) => send("Installed", Some(1.0), true, None),
                    Err(e) => {
                        log::error!("install {id} failed: {e:#}");
                        send("Installation failed", None, true, Some(format!("{e:#}")));
                    }
                }
            })
            .ok();
        Ok(())
    }

    pub(super) fn on_install(&mut self, p: InstallProgress) {
        self.hub.broadcast("install", serde_json::to_value(&p).unwrap_or_default());
        let current = p.id == self.cfg.model.id;
        if p.done {
            self.installing = None;
            if let Some(e) = &p.error {
                self.notice("error", &format!("Installing failed: {}", e.lines().next().unwrap_or("")));
                if current {
                    self.model.phase = ModelPhase::Error;
                    self.model.error = Some(e.clone());
                }
            } else {
                self.notice("success", &format!("{} is installed", models::find(&p.id).map(|m| m.name).unwrap_or("Model")));
                if current {
                    self.model.phase = ModelPhase::Unloaded;
                    self.model.error = None;
                    let _ = self.engine.send(EngineCmd::Configure(EngineSpec::from_config(&self.cfg)));
                    if self.cfg.model.residency != Residency::OnDemand {
                        let _ = self.engine.send(EngineCmd::Load { urgent: false });
                    }
                }
            }
        } else if current {
            self.model.phase = ModelPhase::Installing;
            self.model.progress = p.progress;
            self.model.detail = p.step.clone();
        }
        if current {
            self.publish();
        }
    }
}
