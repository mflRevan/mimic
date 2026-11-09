//! `mimic config`: read and change settings (through the daemon when it runs, else the file directly).

use anyhow::Result;
use clap::Subcommand;
use mimic_core::config::Config;
use mimic_core::ipc::Client;
use serde_json::{json, Value};

use crate::ui::{bold, confirm, print_json};

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Every setting with its current value.
    List,
    /// Print one setting (`audio.preroll_ms`) or a section (`audio`).
    Get { key: String },
    /// Change a setting: `mimic config set audio.preroll_ms 300`.
    Set { key: String, value: String },
    /// Restore all defaults (your dictionary is kept).
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// Where the config file is.
    Path,
    /// Open the config file in your editor.
    Edit,
}

pub fn run(c: ConfigCmd, json_out: bool) -> Result<()> {
    let client = Client::connect().ok();
    match c {
        ConfigCmd::List => {
            let cfg = match client {
                Some(mut c) => serde_json::from_value(c.call("config.get", json!({"path": ""}))?)?,
                None => Config::load()?,
            };
            if json_out {
                print_json(&cfg.to_value());
                return Ok(());
            }
            let mut section = String::new();
            for (k, v) in cfg.flatten() {
                let s = k.split('.').next().unwrap_or("").to_string();
                if s != section {
                    println!("{}", bold(&format!("[{s}]")));
                    section = s;
                }
                let val = match &v {
                    Value::String(s) if k.ends_with("api_key") && !s.is_empty() => "\"••••••\"".to_string(),
                    other => other.to_string(),
                };
                println!("  {:<34} {}", k, val);
            }
            Ok(())
        }
        ConfigCmd::Get { key } => {
            let v = match client {
                Some(mut c) => c.call("config.get", json!({"path": key}))?,
                None => Config::load()?.get_path(&key)?,
            };
            match &v {
                Value::String(s) if !json_out => println!("{s}"),
                _ => print_json(&v),
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let v = match client {
                Some(mut c) => c.call("config.set", json!({"path": key, "value": value}))?,
                None => {
                    let mut cfg = Config::load()?;
                    cfg.set_path(&key, Value::String(value))?;
                    cfg.save()?;
                    cfg.get_path(&key)?
                }
            };
            println!("{key} = {v}");
            Ok(())
        }
        ConfigCmd::Reset { yes } => {
            if !yes && !confirm("Reset all settings to defaults? Your dictionary is kept.")? {
                return Ok(());
            }
            match client {
                Some(mut c) => {
                    c.call("config.reset", Value::Null)?;
                }
                None => {
                    let old = Config::load().unwrap_or_default();
                    let mut cfg = Config::default();
                    cfg.general.onboarded = old.general.onboarded;
                    cfg.save()?;
                }
            }
            println!("Settings reset.");
            Ok(())
        }
        ConfigCmd::Path => {
            println!("{}", mimic_core::paths::config_file().display());
            Ok(())
        }
        ConfigCmd::Edit => {
            let p = mimic_core::paths::config_file();
            if !p.exists() {
                Config::load()?.save()?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "nano".into() });
            std::process::Command::new(editor).arg(&p).status()?;
            if let Some(mut c) = client {
                // Re-apply the edited file through the daemon (validates it too).
                let cfg = Config::load()?;
                c.call("config.merge", json!({"patch": cfg.to_value()}))?;
            }
            Ok(())
        }
    }
}
