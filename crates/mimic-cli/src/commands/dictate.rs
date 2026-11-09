//! `mimic dictate`: start/stop dictation without the shortcut.

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::connect;

#[derive(Subcommand)]
pub enum DictateCmd {
    /// Start hands-free dictation.
    Start {
        /// Push-to-talk style instead of hands-free (stops with `mimic dictate stop`).
        #[arg(long)]
        hold: bool,
    },
    /// Finish the current dictation.
    Stop,
    /// Throw the current dictation away.
    Cancel,
}

pub fn run(d: DictateCmd) -> Result<()> {
    let mut c = connect()?;
    match d {
        DictateCmd::Start { hold } => c.call("dictation.start", json!({"hands_free": !hold}))?,
        DictateCmd::Stop => c.call("dictation.stop", Value::Null)?,
        DictateCmd::Cancel => c.call("dictation.cancel", Value::Null)?,
    };
    Ok(())
}
