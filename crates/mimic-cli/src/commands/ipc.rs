//! `mimic ipc`: call any daemon method directly.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::client::connect;
use crate::ui::print_json;

pub fn run(method: String, params: Option<String>) -> Result<()> {
    let p: Value = match params {
        Some(s) => serde_json::from_str(&s).context("params must be JSON")?,
        None => Value::Null,
    };
    print_json(&connect()?.call(&method, p)?);
    Ok(())
}
