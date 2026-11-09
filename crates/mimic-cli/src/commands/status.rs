//! `mimic status`: what Mimic is doing (optionally watching for changes), plus the status lines shared with other commands.

use anyhow::Result;
use mimic_core::protocol::{Message, ModelPhase, Status, UpdateStatus};
use serde_json::Value;

use crate::client::connect;
use crate::ui::{bold, dim, paint, print_json};

/// The background app runs a different version than this command (an update
/// replaced the files but the old process kept running, or a dev build is mixed in).
pub fn version_skew(st: &Status) -> Option<String> {
    (st.version != mimic_core::VERSION).then(|| format!("the background app is v{} but this command is v{} → mimic restart", st.version, mimic_core::VERSION))
}

pub fn update_line(u: &UpdateStatus) -> Option<String> {
    let v = u.latest.as_deref()?;
    if u.required {
        Some(paint(&format!("v{v} available; this version is no longer supported → mimic update"), "31"))
    } else if u.available && !u.skipped {
        Some(paint(&format!("v{v} available → mimic update"), "33"))
    } else {
        None
    }
}

pub fn phase_label(p: ModelPhase) -> String {
    match p {
        ModelPhase::Ready => paint("ready", "32"),
        ModelPhase::Loading => paint("loading", "36"),
        ModelPhase::Installing => paint("installing", "36"),
        ModelPhase::Unloaded => paint("sleeping (loads on next dictation)", "33"),
        ModelPhase::NotInstalled => paint("not installed (run `mimic model install`)", "31"),
        ModelPhase::Error => paint("error", "31"),
    }
}

fn print_status(st: &Status) {
    let kind = match st.install.kind.as_str() {
        "dev" => " · dev build",
        "portable" => " · portable",
        _ => "",
    };
    println!("{}  {}", bold("Mimic"), dim(&format!("v{}{kind} · pid {} · up {}", st.version, st.pid, human_secs(st.uptime_s))));
    let state = match st.phase {
        mimic_core::protocol::DictationPhase::Idle => "idle".to_string(),
        mimic_core::protocol::DictationPhase::Listening => paint("listening", "31"),
        mimic_core::protocol::DictationPhase::HandsFree => paint("listening (hands-free)", "33"),
        mimic_core::protocol::DictationPhase::Finishing => paint("typing", "36"),
    };
    println!("  state      {state}");
    let mut m = format!("{} — {}", st.model.name, phase_label(st.model.phase));
    if let Some(p) = st.model.progress {
        m.push_str(&format!(" {:.0}%", p * 100.0));
    }
    if !st.model.detail.is_empty() && st.model.phase != ModelPhase::Ready {
        m.push_str(&dim(&format!(" ({})", st.model.detail)));
    }
    println!("  model      {m}");
    if !st.model.device.is_empty() {
        let mem = st.model.memory_mb.map(|m| format!(" · {m} MB")).unwrap_or_default();
        println!("  runs on    {}{}", st.model.device, mem);
    }
    if let Some(s) = st.model.unload_in_s {
        println!("  unloads in {}", human_secs(s));
    }
    if let Some(e) = &st.model.error {
        println!("  {}  {}", paint("problem", "31"), e.lines().next().unwrap_or(""));
    }
    let mic = if st.mic.open { format!("{} {}", st.mic.device, dim(&format!("({} Hz)", st.mic.sample_rate))) } else { dim("closed (opens when you dictate)") };
    println!("  microphone {mic}");
    if let Some(e) = &st.mic.error {
        println!("  {}  {e}", paint("mic issue", "31"));
    }
    println!(
        "  shortcuts  hold {} · hands-free {} · add word {}{}",
        bold(&st.hotkeys.push_to_talk),
        st.hotkeys.hands_free,
        st.hotkeys.add_rule,
        if st.hotkeys.paused { paint("  (paused)", "33") } else { String::new() }
    );
    let lat = st.stats.median_latency_ms.map(|m| format!(" · median {m} ms after release")).unwrap_or_default();
    println!("  session    {} dictations · {} words{}", st.stats.dictations, st.stats.words, lat);
    if let Some(u) = update_line(&st.update) {
        println!("  update     {u}");
    }
    if let Some(w) = version_skew(st) {
        println!("  {}    {w}", paint("note", "33"));
    }
}

fn human_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn run(json_out: bool, watch: bool) -> Result<()> {
    let mut c = connect()?;
    let v = c.call("status", Value::Null)?;
    if json_out && !watch {
        print_json(&v);
        return Ok(());
    }
    let st: Status = serde_json::from_value(v)?;
    print_status(&st);
    if !watch {
        return Ok(());
    }
    c.subscribe()?;
    loop {
        if let Message::Event { event, data } = c.read_message()? {
            match event.as_str() {
                "status" if json_out => println!("{data}"),
                "status" => {
                    let st: Status = serde_json::from_value(data)?;
                    println!();
                    print_status(&st);
                }
                "partial" => println!("{} {}", dim("…"), data["text"].as_str().unwrap_or("")),
                "dictation" => println!("{} {}", paint("✓", "32"), data["text"].as_str().unwrap_or("")),
                "notice" => println!("{} {}", paint("•", "36"), data["text"].as_str().unwrap_or("")),
                _ => {}
            }
        }
    }
}
