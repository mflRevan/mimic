//! The background app's lifecycle: start, stop, restart, open its window or the popup.

use anyhow::Result;
use mimic_core::protocol::Status;
use serde_json::{json, Value};

use crate::client::{start_daemon, stop_daemon};
use crate::ui::dim;

pub fn start() -> Result<()> {
    let mut c = start_daemon()?;
    let st: Status = serde_json::from_value(c.call("status", Value::Null)?)?;
    println!("Mimic {} is running {}", st.version, dim(&format!("(pid {})", st.pid)));
    Ok(())
}

pub fn stop() -> Result<()> {
    if stop_daemon()? {
        println!("Mimic stopped.");
    } else {
        println!("Mimic is not running.");
    }
    Ok(())
}

pub fn restart() -> Result<()> {
    stop_daemon()?;
    let mut c = start_daemon()?;
    let st: Status = serde_json::from_value(c.call("status", Value::Null)?)?;
    println!("Mimic {} restarted.", st.version);
    Ok(())
}

pub fn open(page: Option<String>) -> Result<()> {
    let mut c = start_daemon()?;
    c.call("ui.open", json!({"page": page.unwrap_or_default()}))?;
    Ok(())
}

pub fn popup() -> Result<()> {
    let mut c = start_daemon()?;
    c.call("popup.open", Value::Null)?;
    Ok(())
}
