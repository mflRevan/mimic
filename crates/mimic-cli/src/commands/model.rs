//! `mimic model`: speech models: list, switch, install (with progress), load, unload.

use std::io::IsTerminal;

use anyhow::{bail, Result};
use clap::Subcommand;
use mimic_core::protocol::{InstallProgress, Message, ModelPhase, Status};
use serde_json::{json, Value};

use crate::client::start_daemon;
use crate::commands::status::phase_label;
use crate::ui::{bold, dim, paint, print_json};

#[derive(Subcommand)]
pub enum ModelCmd {
    /// Available models and which one is active.
    #[command(visible_alias = "ls")]
    List,
    /// Switch to a model: phonon-2.5, parakeet-v3, custom.
    Use { id: String },
    /// Download and set up a model (default: the active one). Shows progress.
    Install { id: Option<String> },
    /// Load the model into memory now.
    Load,
    /// Free the model's memory (it reloads on the next dictation).
    Unload,
}

pub fn run(m: ModelCmd, json_out: bool) -> Result<()> {
    let mut c = start_daemon()?;
    match m {
        ModelCmd::List => {
            let v = c.call("model.catalog", Value::Null)?;
            if json_out {
                print_json(&v);
                return Ok(());
            }
            for m in v.as_array().cloned().unwrap_or_default() {
                let active = m["active"].as_bool() == Some(true);
                let mark = if active { paint("●", "32") } else { " ".into() };
                let inst = if m["engine"] == "custom" {
                    dim("(configure in settings)")
                } else if m["installed"].as_bool() == Some(true) {
                    paint("installed", "32")
                } else {
                    dim(&format!("{} MB download", m["download_mb"]))
                };
                println!("{mark} {:<14} {:<26} {}", bold(m["id"].as_str().unwrap_or("")), m["name"].as_str().unwrap_or(""), inst);
                println!("  {:<14} {}", "", dim(m["tagline"].as_str().unwrap_or("")));
            }
            Ok(())
        }
        ModelCmd::Use { id } => {
            c.call("model.use", json!({"id": id}))?;
            let st: Status = serde_json::from_value(c.call("status", Value::Null)?)?;
            println!("using {} — {}", st.model.name, phase_label(st.model.phase));
            Ok(())
        }
        ModelCmd::Install { id } => {
            let id = id.unwrap_or_default();
            c.subscribe()?;
            c.call("model.install", json!({"id": id}))?;
            let tty = std::io::stderr().is_terminal();
            loop {
                if let Message::Event { event, data } = c.read_message()? {
                    if event != "install" {
                        continue;
                    }
                    let p: InstallProgress = serde_json::from_value(data)?;
                    if json_out {
                        println!("{}", serde_json::to_string(&p)?);
                    } else {
                        let pct = p.progress.map(|x| format!("{:>3.0}%", x * 100.0)).unwrap_or_else(|| "    ".into());
                        if tty {
                            eprint!("\r\x1b[2K  {pct}  {}", p.step);
                        } else {
                            eprintln!("  {pct}  {}", p.step);
                        }
                    }
                    if p.done {
                        if tty && !json_out {
                            eprintln!();
                        }
                        if let Some(e) = p.error {
                            bail!("{e}");
                        }
                        println!("{} installed", paint("✓", "32"));
                        return Ok(());
                    }
                }
            }
        }
        ModelCmd::Load => {
            c.call("model.load", Value::Null)?;
            c.subscribe()?;
            loop {
                if let Message::Event { event, data } = c.read_message()? {
                    if event == "status" {
                        let st: Status = serde_json::from_value(data)?;
                        match st.model.phase {
                            ModelPhase::Ready => {
                                println!("{} {} on {}", paint("✓", "32"), st.model.name, st.model.device);
                                return Ok(());
                            }
                            ModelPhase::Error | ModelPhase::NotInstalled => bail!("{}", st.model.error.unwrap_or_else(|| "model is not installed".into())),
                            _ => {}
                        }
                    }
                }
            }
        }
        ModelCmd::Unload => {
            c.call("model.unload", Value::Null)?;
            println!("model unloaded");
            Ok(())
        }
    }
}
