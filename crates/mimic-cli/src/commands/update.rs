//! `mimic update`: check for a new version, install it (with download progress), or skip it.

use anyhow::{bail, Result};
use mimic_core::protocol::{Message, UpdateStatus};
use serde_json::Value;

use crate::client::{connect, start_daemon};
use crate::ui::{bold, confirm, dim, paint, print_json};

pub fn run(check_only: bool, install: bool, skip: bool, json_out: bool) -> Result<()> {
    let mut c = start_daemon()?;
    if skip {
        c.call("update.skip", Value::Null)?;
        println!("Mimic won't remind you about this version. `mimic update` still installs it.");
        return Ok(());
    }
    eprintln!("{}", dim("Checking for updates…"));
    let u: UpdateStatus = serde_json::from_value(c.call("update.check", Value::Null)?)?;
    if json_out {
        print_json(&serde_json::to_value(&u)?);
        return Ok(());
    }
    if let Some(e) = &u.error {
        bail!("couldn't check for updates: {e}");
    }
    let latest = u.latest.clone().unwrap_or_default();
    if !u.available {
        println!("Mimic {} is up to date.", u.current);
        return Ok(());
    }
    println!("{} {} → {}", bold("Update available:"), u.current, paint(&latest, "32"));
    if let Some(m) = &u.message {
        println!("  {m}");
    }
    if let Some(n) = &u.notes_url {
        println!("  {}", dim(&format!("What's new: {n}")));
    }
    if check_only || !(install || confirm("Install it now? Mimic restarts by itself.")?) {
        return Ok(());
    }
    // Watch progress on a second connection; the install call returns right away.
    let mut events = connect()?;
    events.subscribe()?;
    c.call("update.install", Value::Null)?;
    // Events are ordered: the daemon publishes "downloading" right after accepting the
    // install, so an error seen after that belongs to this download (not an older one).
    let mut started = false;
    let mut shown = -1i32;
    loop {
        match events.read_message() {
            Ok(Message::Event { event, data }) if event == "status" => {
                let st: UpdateStatus = serde_json::from_value(data["update"].clone()).unwrap_or_default();
                if let Some(p) = st.downloading {
                    started = true;
                    let pct = (p * 100.0) as i32;
                    if pct / 10 != shown / 10 {
                        shown = pct;
                        eprint!("\r  downloading {pct:>3}%");
                    }
                } else if let Some(e) = st.error.filter(|_| started) {
                    eprintln!();
                    bail!("{e}");
                }
            }
            Ok(Message::Event { event, .. }) if event == "quit" => break,
            Ok(_) => {}
            Err(_) => break, // the daemon quit to let the installer run
        }
    }
    eprintln!();
    println!("Installing Mimic {latest}. It starts again by itself when done.");
    Ok(())
}
