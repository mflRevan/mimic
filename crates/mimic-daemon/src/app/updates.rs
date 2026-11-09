//! App updates: when to look for them, when installing is safe, and update events.

use crate::updater::{Outcome, UpdateEv};

use super::App;

impl App {
    // ------------------------------------------------------------- updates

    /// Updates are looked for (to offer, or to install by themselves).
    pub(super) fn updates_on(&self) -> bool {
        self.cfg.general.check_updates || self.cfg.general.auto_update
    }

    /// Nothing going on that an update (a restart) would interrupt: the agent off, or not
    /// talking, thinking, on autopilot or in a conversation for the last 3 minutes.
    pub(super) fn idle_for_update(&self) -> bool {
        let a = self.agent.status();
        let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        !a.enabled || (!a.autopilot && !a.talking && !a.thinking && !a.speaking && now_s.saturating_sub(a.last_active_s) >= 180)
    }

    pub(super) fn on_update(&mut self, u: UpdateEv) {
        match u {
            UpdateEv::Checked(r) => {
                let (waiters, outcome) = self.updater.on_checked(r);
                let st = self.updater.status(self.updates_on());
                for w in waiters {
                    w.ok(serde_json::to_value(&st).unwrap_or_default());
                }
                let from_tray = std::mem::take(&mut self.tray_check) && self.session.is_none();
                match outcome {
                    Outcome::Available(v) if self.cfg.general.auto_update => match self.updater.install_auto(&self.tx) {
                        Ok(_) => log::info!("update {v}: downloading; installs when Mimic is idle"),
                        Err(e) => self.notice("info", &format!("Mimic {v} is available ({e})")),
                    },
                    Outcome::Available(v) => self.notice("info", &format!("Mimic {v} is available")),
                    Outcome::Unsupported(msg) => {
                        let text = msg.unwrap_or_else(|| "This version of Mimic is no longer supported. Please update.".into());
                        self.notice("error", &text);
                        if self.session.is_none() && !from_tray {
                            self.overlay.notice(&text, true);
                        }
                    }
                    Outcome::Nothing => {}
                }
                if from_tray {
                    let text = match (&st.error, st.available, &st.latest) {
                        (Some(_), _, _) => "Couldn't check for updates".to_string(),
                        (_, true, Some(v)) => format!("Mimic {v} is available. Open Mimic to install it."),
                        _ => format!("Mimic {} is up to date", st.current),
                    };
                    self.overlay.notice(&text, st.error.is_some());
                }
                self.publish();
            }
            UpdateEv::Progress(p) => {
                self.updater.on_progress(p);
                self.publish();
            }
            UpdateEv::Downloaded(r) => {
                if let Err(e) = self.updater.on_downloaded(r) {
                    self.notice("error", &format!("Update failed: {}", e.lines().next().unwrap_or("")));
                }
                self.publish();
            }
        }
    }
}
