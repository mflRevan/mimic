//! `mimic history`: recent dictations (in memory only).

use anyhow::Result;
use mimic_core::protocol::HistoryItem;
use serde_json::Value;

use crate::client::connect;
use crate::ui::{dim, print_json, truncate};

pub fn run(clear: bool, limit: usize, json_out: bool) -> Result<()> {
    let mut c = connect()?;
    if clear {
        c.call("history.clear", Value::Null)?;
        println!("History cleared.");
        return Ok(());
    }
    let v = c.call("history.list", Value::Null)?;
    if json_out {
        print_json(&v);
        return Ok(());
    }
    let items: Vec<HistoryItem> = serde_json::from_value(v)?;
    if items.is_empty() {
        println!("{}", dim("No dictations yet."));
    }
    for h in items.iter().take(limit) {
        println!("{} {}", dim(&format!("{:>5} ms  {:<12}", h.latency_ms, truncate(&h.app, 12))), h.text);
    }
    Ok(())
}
