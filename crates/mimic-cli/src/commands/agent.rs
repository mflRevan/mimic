//! `mimic agent`: the voice agent: on/off, autopilot, speaking, what it knows (context), the log, your voice reference.

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::cli::OnOff;
use crate::client::connect;
use crate::ui::print_json;

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Agent state: models, devices, voice reference, instructions.
    Status,
    /// Turn the agent on (loads its models: about 21 GB + 2.5 GB of GPU memory).
    On,
    /// Turn the agent off (frees the GPU).
    Off,
    /// Autopilot: the agent answers the conversation for you (toggles without an argument).
    Autopilot { state: Option<OnOff> },
    /// Say exactly this through your mic, in your voice.
    Say { text: Vec<String> },
    /// Answer the conversation now, in your voice.
    Reply {
        /// What the answer should get across.
        #[arg(long)]
        hint: Option<String>,
    },
    /// Stop speaking and thinking.
    Stop,
    /// Tell the agent something privately, as if you had said it ("note that ...", "this is Alex, my manager").
    Tell { text: Vec<String> },
    /// What the agent knows (context.md): show it, or `add <section> <text>`, `remove <text>`, `new`
    /// (a new conversation), `path`. Sections: me, style, people, situation, facts, rules.
    Context { args: Vec<String> },
    /// What was said recently (you, them, the agent, private notes).
    Log {
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Your voice reference for cloning.
    #[command(subcommand)]
    Voice(VoiceCmd),
    /// List apps and audio devices it can hear or talk into.
    Sources,
    /// Retry loading the agent's models after an error.
    Load,
}

#[derive(Subcommand)]
pub enum VoiceCmd {
    /// Keep the current reference (and save it, so it survives restarts).
    Pin,
    /// Follow your latest speech again.
    Unpin,
    /// Forget your voice (memory and disk).
    Clear,
    /// Use a recording as the reference.
    Import {
        file: std::path::PathBuf,
        /// Exactly what is said in the recording.
        #[arg(long)]
        text: String,
    },
    /// Save the current reference to a WAV file.
    Export { file: std::path::PathBuf },
}

pub fn run(a: AgentCmd, json_out: bool) -> Result<()> {
    let mut c = connect()?;
    let words = |v: Vec<String>| v.join(" ").trim().to_string();
    let abs = |p: std::path::PathBuf| std::path::absolute(&p).unwrap_or(p).display().to_string();
    let out = match a {
        AgentCmd::Status => {
            let v = c.call("agent.status", Value::Null)?;
            if json_out {
                print_json(&v);
                return Ok(());
            }
            let svc = |s: &Value| {
                let mut t = format!("{} {}", s["phase"].as_str().unwrap_or("-"), s["name"].as_str().unwrap_or(""));
                if let Some(d) = s["detail"].as_str().filter(|d| !d.is_empty()) {
                    t += &format!(" ({d})");
                }
                if let Some(e) = s["error"].as_str() {
                    t += &format!("\n             {e}");
                }
                t
            };
            let dev = |s: &Value| match (s["open"].as_bool(), s["device"].as_str().unwrap_or("")) {
                (Some(true), d) => d.to_string(),
                _ => s["error"].as_str().map(|e| format!("not open: {e}")).unwrap_or_else(|| "off".into()),
            };
            let yes = |k: &str| if v[k].as_bool().unwrap_or(false) { "yes" } else { "no" };
            println!("agent        {}", if v["enabled"].as_bool().unwrap_or(false) { "on" } else { "off" });
            println!("autopilot    {}", yes("autopilot"));
            println!("brain        {}", svc(&v["llm"]));
            println!("voice        {}", svc(&v["tts"]));
            let pinned = if v["voice_pinned"].as_bool().unwrap_or(false) { " (pinned)" } else { "" };
            println!("your voice   {:.1} s{pinned}", v["voice_s"].as_f64().unwrap_or(0.0));
            for h in v["hear"].as_array().cloned().unwrap_or_default() {
                println!("hears        {}: {}", h["label"].as_str().unwrap_or(""), dev(&h));
            }
            println!("virtual mic  {}", dev(&v["route"]));
            println!("now          thinking: {}, speaking: {}, talking to it: {}", yes("thinking"), yes("speaking"), yes("talking"));
            let known: usize = v["context"]["sections"].as_array().map(|s| s.iter().map(|x| x[1].as_array().map_or(0, |a| a.len())).sum()).unwrap_or(0);
            println!("knows        {known} things (mimic agent context)");
            return Ok(());
        }
        AgentCmd::On => c.call("config.set", json!({"path": "agent.enabled", "value": true}))?,
        AgentCmd::Off => c.call("config.set", json!({"path": "agent.enabled", "value": false}))?,
        AgentCmd::Autopilot { state } => match state {
            Some(s) => c.call("agent.autopilot", json!({"on": matches!(s, OnOff::On)}))?,
            None => c.call("agent.autopilot", json!({}))?,
        },
        AgentCmd::Say { text } => c.call("agent.say", json!({"text": words(text)}))?,
        AgentCmd::Reply { hint } => c.call("agent.reply", json!({"hint": hint.unwrap_or_default()}))?,
        AgentCmd::Stop => c.call("agent.stop", Value::Null)?,
        AgentCmd::Tell { text } => c.call("agent.tell", json!({"text": words(text)}))?,
        AgentCmd::Context { args } => {
            let rest = |n: usize| args.get(n..).map(|a| a.join(" ")).unwrap_or_default();
            match args.first().map(String::as_str) {
                None | Some("show") => {
                    let v = c.call("agent.context", Value::Null)?;
                    if json_out {
                        print_json(&v);
                    } else {
                        print!("{}", v["markdown"].as_str().unwrap_or(""));
                    }
                    return Ok(());
                }
                Some("path") => {
                    let v = c.call("agent.context", Value::Null)?;
                    println!("{}", v["path"].as_str().unwrap_or(""));
                    return Ok(());
                }
                Some("add") => c.call("agent.context.add", json!({"section": args.get(1).cloned().unwrap_or_default(), "text": rest(2)}))?,
                Some("remove" | "forget") => c.call("agent.context.remove", json!({"text": rest(1)}))?,
                Some("new") => c.call("agent.context.new", Value::Null)?,
                Some(other) => bail!("unknown `{other}`: use show, add <section> <text>, remove <text>, new or path"),
            }
        }
        AgentCmd::Log { limit } => {
            let v = c.call("agent.timeline", json!({"limit": limit}))?;
            if json_out {
                print_json(&v);
                return Ok(());
            }
            for l in v.as_array().cloned().unwrap_or_default() {
                let who = match l["who"].as_str().unwrap_or("") {
                    "me" => "you".to_string(),
                    "them" => {
                        let name = l["name"].as_str().filter(|s| !s.is_empty()).unwrap_or("them");
                        match l["source"].as_str().filter(|s| !s.is_empty()) {
                            Some(s) => format!("{name} ({s})"),
                            None => name.to_string(),
                        }
                    }
                    "agent" => "agent".into(),
                    "private" => "private".into(),
                    _ => "note".into(),
                };
                let who = format!("{who:<20}");
                let cut = if l["interrupted"].as_bool().unwrap_or(false) { " [cut off]" } else { "" };
                println!("{who}  {}{cut}", l["text"].as_str().unwrap_or(""));
            }
            return Ok(());
        }
        AgentCmd::Voice(v) => match v {
            VoiceCmd::Pin => c.call("config.set", json!({"path": "agent.tts.pinned", "value": true}))?,
            VoiceCmd::Unpin => c.call("config.set", json!({"path": "agent.tts.pinned", "value": false}))?,
            VoiceCmd::Clear => c.call("agent.voice.clear", Value::Null)?,
            VoiceCmd::Import { file, text } => {
                c.call("agent.voice.import", json!({"path": abs(file), "text": text}))?;
                c.call("config.set", json!({"path": "agent.tts.pinned", "value": true}))?
            }
            VoiceCmd::Export { file } => c.call("agent.voice.export", json!({"path": abs(file)}))?,
        },
        AgentCmd::Sources => c.call("agent.sources", Value::Null)?,
        AgentCmd::Load => c.call("agent.load", Value::Null)?,
    };
    if json_out || (!out.is_null() && out != json!({})) {
        print_json(&out);
    }
    Ok(())
}
