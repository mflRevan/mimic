//! `mimic transcribe`: run a WAV file through the current model and your rules.

use anyhow::{Context, Result};
use serde_json::json;

use crate::client::start_daemon;
use crate::ui::{dim, print_json};

pub fn run(file: std::path::PathBuf, raw: bool, json_out: bool) -> Result<()> {
    let mut c = start_daemon()?;
    let path = std::fs::canonicalize(&file).with_context(|| format!("{} not found", file.display()))?;
    let v = c.call("transcribe.file", json!({"path": path.display().to_string()}))?;
    if json_out {
        print_json(&v);
    } else {
        println!("{}", if raw { v["raw"].as_str().unwrap_or("") } else { v["text"].as_str().unwrap_or("") });
        eprintln!("{}", dim(&format!("decoded in {} ms", v["decode_ms"])));
    }
    Ok(())
}
