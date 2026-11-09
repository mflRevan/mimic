//! `mimic devices`: list microphones.

use anyhow::Result;
use mimic_core::config::Config;
use serde_json::Value;

use crate::client::start_daemon;
use crate::ui::{dim, print_json};

pub fn run(json_out: bool) -> Result<()> {
    let mut c = start_daemon()?;
    let v = c.call("devices.list", Value::Null)?;
    if json_out {
        print_json(&v);
        return Ok(());
    }
    let cfg = Config::load().unwrap_or_default();
    for d in v.as_array().cloned().unwrap_or_default() {
        let name = d["name"].as_str().unwrap_or("");
        let mut tags = Vec::new();
        if d["default"].as_bool() == Some(true) {
            tags.push("system default");
        }
        if name == cfg.audio.device {
            tags.push("selected");
        }
        println!("  {name} {}", dim(&if tags.is_empty() { String::new() } else { format!("({})", tags.join(", ")) }));
    }
    if cfg.audio.device.is_empty() {
        println!("{}", dim("Using the system default. Choose one with: mimic config set audio.device \"<name>\""));
    }
    Ok(())
}
