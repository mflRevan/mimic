//! This computer's Mimic install: start with Windows, uninstall.

use anyhow::{bail, Context, Result};
use mimic_core::config::Config;
use mimic_core::ipc::Client;
use serde_json::json;

use crate::cli::OnOff;

pub fn autostart(state: OnOff) -> Result<()> {
    let on = matches!(state, OnOff::On);
    match Client::connect() {
        Ok(mut c) => {
            c.call("autostart.set", json!({"enabled": on}))?;
        }
        Err(_) => {
            let mut cfg = Config::load()?;
            cfg.general.autostart = on;
            cfg.save()?;
        }
    }
    println!("Start with Windows: {}", if on { "on" } else { "off" });
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let dir = std::env::current_exe()?.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let un = dir.join("uninstall.exe");
    if !un.exists() {
        bail!("this copy of Mimic ({}) wasn't set up by the installer, so there is nothing to uninstall. Delete the folder to remove it.", dir.display());
    }
    std::process::Command::new(&un).spawn().with_context(|| format!("starting {}", un.display()))?;
    println!("The uninstaller is open. It asks whether to delete your settings, dictionary and models too.");
    Ok(())
}
