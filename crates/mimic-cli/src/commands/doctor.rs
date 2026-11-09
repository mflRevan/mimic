//! `mimic doctor`: check that everything is set up correctly.

use anyhow::Result;
use mimic_core::config::Config;
use mimic_core::ipc::Client;
use mimic_core::protocol::{ModelPhase, Status};
use mimic_core::rules::RuleSet;
use serde_json::Value;

use crate::commands::status::{update_line, version_skew};
use crate::ui::{bold, paint};

pub fn run() -> Result<()> {
    let ok = |s: &str| println!("  {} {s}", paint("✓", "32"));
    let bad = |s: &str| println!("  {} {s}", paint("✗", "31"));
    let warn = |s: &str| println!("  {} {s}", paint("!", "33"));
    println!("{}", bold("Mimic doctor"));
    match Config::load() {
        Ok(_) => ok(&format!("config is valid ({})", mimic_core::paths::config_file().display())),
        Err(e) => bad(&format!("config: {e:#}")),
    }
    match RuleSet::load() {
        Ok(r) => ok(&format!("dictionary: {} rules", r.rules.len())),
        Err(e) => bad(&format!("rules file: {e:#}")),
    }
    match Client::connect() {
        Ok(mut c) => {
            ok("background app is running");
            let st: Status = serde_json::from_value(c.call("status", Value::Null)?)?;
            match version_skew(&st) {
                Some(w) => warn(&w),
                None => ok(&format!("version {} ({})", st.version, if st.install.kind.is_empty() { "unknown install" } else { &st.install.kind })),
            }
            if let (Some(dir), "portable" | "dev") = (&st.install.registered_dir, st.install.kind.as_str()) {
                warn(&format!("running from {} while Mimic {} is installed in {dir}", st.install.dir, st.install.registered_version.as_deref().unwrap_or("")));
            }
            match update_line(&st.update) {
                Some(u) => warn(&u),
                None if st.update.checked_at.is_some() => ok("no updates pending"),
                None => {}
            }
            match st.model.phase {
                ModelPhase::Ready => ok(&format!("{} is loaded on {}", st.model.name, st.model.device)),
                ModelPhase::NotInstalled => bad(&format!("{} is not installed → mimic model install", st.model.name)),
                ModelPhase::Error => bad(&format!("model error: {}", st.model.error.unwrap_or_default())),
                p => warn(&format!("{} is {:?}", st.model.name, p)),
            }
            if st.mic.open {
                ok(&format!("microphone: {}", st.mic.device));
            } else if let Some(e) = st.mic.error {
                bad(&format!("microphone: {e}"));
            } else {
                warn("microphone is closed (it opens when you dictate)");
            }
            let devs = c.call("devices.list", Value::Null)?;
            if devs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                bad("no microphones found");
            }
            if st.hotkeys.paused {
                warn("shortcuts are paused (tray menu)");
            } else {
                ok(&format!("shortcuts active: hold {}", st.hotkeys.push_to_talk));
            }
        }
        Err(_) => bad("background app is not running → mimic start"),
    }
    Ok(())
}
