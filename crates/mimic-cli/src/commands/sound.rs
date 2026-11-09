//! `mimic sound`: the soundboard.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::client::connect;
use crate::ui::print_json;

pub fn run(what: String, args: Vec<String>, json_out: bool) -> Result<()> {
    let mut c = connect()?;
    let out = match what.as_str() {
        "stop" => c.call("soundboard.stop", Value::Null)?,
        "list" | "status" => c.call("soundboard.status", Value::Null)?,
        "set" => {
            let slot: u64 = args.first().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("usage: mimic sound set <1-9> <file>"))?;
            let file = std::path::PathBuf::from(args.get(1..).map(|a| a.join(" ")).unwrap_or_default());
            let file = std::path::absolute(&file).unwrap_or(file);
            c.call("config.set", json!({"path": format!("soundboard.slots.{}.file", slot.saturating_sub(1)), "value": file.display().to_string()}))?
        }
        n => {
            let slot: u64 = n.parse().map_err(|_| anyhow!("give a slot 1-9, `stop`, `list` or `set`"))?;
            c.call("soundboard.play", json!({"slot": slot}))?
        }
    };
    if json_out || !(out.is_null() || out == json!({})) {
        print_json(&out);
    }
    Ok(())
}
