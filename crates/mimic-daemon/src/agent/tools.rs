//! Tools the agent can use. Side-effect-only tools (notes, snapshots) run here; tools that
//! change what the agent does next (say, reply, autopilot, context edits) are returned as
//! [`Action`]s for the agent loop. Adding a tool: define it in `mimic_core::agent::prompt::command_tools`,
//! then handle its name in [`run`]. The deterministic router (`mimic_core::agent::route`) reaches
//! the same actions without a model call.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use mimic_core::agent::stream::ToolCall;
use mimic_core::agent::{Line, Section};

pub enum Action {
    Say(String),
    Reply(Option<String>),
    Autopilot(bool),
    ContextAdd(Section, String),
    ContextRemove(String),
    /// Name a voice: ("last" or "Speaker 2", name).
    NameSpeaker(String, String),
    /// Done; a short confirmation for the overlay/timeline.
    Done(String),
}

pub struct Snapshot<'a> {
    pub label: &'a str,
    pub lines: &'a [&'a Line],
    /// The context document at the time (Markdown).
    pub context: &'a str,
    /// (name, mono pcm, rate)
    pub audio: Vec<(String, Vec<f32>, u32)>,
}

pub fn notes_dir(cfg_dir: &str) -> PathBuf {
    if cfg_dir.trim().is_empty() {
        mimic_core::paths::data_dir().join("notes")
    } else {
        PathBuf::from(cfg_dir.trim())
    }
}

/// Interpret one tool call. `snapshot` is called lazily (it captures the screen and audio).
pub fn run(call: &ToolCall, notes: &str, snapshot: &mut dyn FnMut(&str) -> Result<PathBuf>) -> Result<Action> {
    let a = call.args();
    let s = |k: &str| a[k].as_str().unwrap_or("").trim().to_string();
    Ok(match call.name.as_str() {
        "note" => {
            let text = s("text");
            if text.is_empty() {
                return Ok(Action::Done("Nothing to note".into()));
            }
            let path = append_note(notes, &text)?;
            log::info!("note saved to {}", path.display());
            Action::Done(format!("Noted: {text}"))
        }
        "snapshot" => {
            let label = s("label");
            let dir = snapshot(if label.is_empty() { "snapshot" } else { &label })?;
            Action::Done(format!("Snapshot saved ({})", dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
        }
        "context_add" => {
            let section = Section::parse(&s("section")).unwrap_or(Section::Facts);
            Action::ContextAdd(section, s("text"))
        }
        "context_remove" => Action::ContextRemove(s("text")),
        "name_speaker" => Action::NameSpeaker(s("speaker"), s("name")),
        "say" => Action::Say(s("text")),
        "reply" => Action::Reply(Some(s("hint")).filter(|h| !h.is_empty())),
        "autopilot" => Action::Autopilot(a["on"].as_bool().unwrap_or_else(|| matches!(a["on"].as_str(), Some("true" | "on")))),
        other => anyhow::bail!("unknown tool `{other}`"),
    })
}

/// Append to today's notes file (`<notes>/YYYY-MM-DD.md`).
pub fn append_note(notes: &str, text: &str) -> Result<PathBuf> {
    let dir = notes_dir(notes);
    std::fs::create_dir_all(&dir)?;
    let now = chrono::Local::now();
    let path = dir.join(format!("{}.md", now.format("%Y-%m-%d")));
    let new = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    if new {
        writeln!(f, "# Notes, {}\n", now.format("%A %-d %B %Y"))?;
    }
    writeln!(f, "- **{}** {}", now.format("%H:%M"), text.replace('\n', " "))?;
    Ok(path)
}

/// Save a snapshot folder: `conversation.md` (with the context) and the recent audio as WAVs.
pub fn save_snapshot(notes: &str, s: &Snapshot) -> Result<PathBuf> {
    let now = chrono::Local::now();
    let slug: String = s
        .label
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let dir = notes_dir(notes).join("snapshots").join(format!("{}-{}", now.format("%Y-%m-%d-%H%M%S"), if slug.is_empty() { "snapshot".into() } else { slug }));
    std::fs::create_dir_all(&dir)?;
    let mut md = format!("# {}\n\n{}\n\n", s.label, now.format("%A %-d %B %Y, %H:%M:%S"));
    for l in s.lines {
        let at =
            chrono::DateTime::from_timestamp_millis(l.at as i64).map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
        let src = if l.source.is_empty() { String::new() } else { format!(" ({})", l.source) };
        md.push_str(&format!("- `{at}` **{:?}{src}**: {}\n", l.who, l.text));
    }
    if !s.context.trim().is_empty() {
        md.push_str("\n---\n\n");
        md.push_str(s.context);
    }
    std::fs::write(dir.join("conversation.md"), md)?;
    for (name, pcm, rate) in &s.audio {
        if !pcm.is_empty() {
            std::fs::write(dir.join(format!("{name}.wav")), crate::dsp::wav_bytes_at(pcm, *rate))?;
        }
    }
    let _ = append_note(notes, &format!("Snapshot: {} ({})", s.label, dir.display()));
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_and_actions() {
        let dir = std::env::temp_dir().join(format!("mimic-notes-{}", std::process::id()));
        let d = dir.display().to_string();
        let call = |name: &str, args: &str| ToolCall { id: "1".into(), name: name.into(), arguments: args.into() };
        let mut snap = |_: &str| -> Result<PathBuf> { Ok(dir.join("snap")) };
        assert!(matches!(run(&call("note", r#"{"text":"buy milk"}"#), &d, &mut snap).unwrap(), Action::Done(t) if t.contains("buy milk")));
        let today = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        assert!(std::fs::read_to_string(today).unwrap().contains("buy milk"));
        assert!(matches!(run(&call("say", r#"{"text":"hi"}"#), &d, &mut snap).unwrap(), Action::Say(t) if t == "hi"));
        assert!(matches!(run(&call("autopilot", r#"{"on":true}"#), &d, &mut snap).unwrap(), Action::Autopilot(true)));
        assert!(matches!(run(&call("reply", "{}"), &d, &mut snap).unwrap(), Action::Reply(None)));
        assert!(matches!(
            run(&call("context_add", r#"{"section":"people","text":"Alex: my manager"}"#), &d, &mut snap).unwrap(),
            Action::ContextAdd(Section::People, _)
        ));
        assert!(run(&call("nope", "{}"), &d, &mut snap).is_err());
        let lines = [Line {
            id: 1,
            at: 0,
            who: mimic_core::agent::Who::Them,
            source: "Discord".into(),
            speaker: None,
            name: "Speaker 1".into(),
            text: "hello".into(),
            interrupted: false,
        }];
        let refs: Vec<&Line> = lines.iter().collect();
        let p =
            save_snapshot(&d, &Snapshot { label: "Budget call!", lines: &refs, context: "# Context", audio: vec![("them".into(), vec![0.0; 1600], 16000)] })
                .unwrap();
        assert!(p.join("conversation.md").exists() && p.join("them.wav").exists());
        assert!(std::fs::read_to_string(p.join("conversation.md")).unwrap().contains("Them (Discord)"));
        assert!(p.file_name().unwrap().to_string_lossy().ends_with("budget-call"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
