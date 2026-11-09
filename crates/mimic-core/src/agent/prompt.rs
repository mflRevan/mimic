//! Prompts and tool definitions, kept short and regular so a small model can follow along.
//! Three kinds of requests, all built from the active mode and the context document (stable
//! parts first, so the server's prompt cache covers most of every request):
//!
//! * **reply**: the agent speaks in the conversation as you. The transcript goes in as one user
//!   message, each line labelled with who said it ("Ben:", "Speaker 2:", "Me:"); the answer
//!   is the exact words to say, or `[silent]`.
//! * **command**: you spoke to the agent privately and the deterministic router couldn't place
//!   it. The model acts through tools or answers you briefly.
//! * **summarize**: the model folds older conversation into the context's summary, so the
//!   prompt stays short however long the call runs.

use serde_json::{json, Value};

use super::context::{ContextDoc, Section};
use super::timeline::{Line, Who};

pub struct Context<'a> {
    /// The active mode's text: the setting the agent plays in.
    pub scenario: &'a str,
    pub doc: &'a ContextDoc,
    /// Recent conversation lines, oldest first.
    pub lines: &'a [&'a Line],
    /// Local time, e.g. "Saturday 8 November 2026, 22:50".
    pub now: &'a str,
}

/// How a line's speaker appears to the model.
pub fn speaker_label(l: &Line) -> &str {
    match l.who {
        Who::Me | Who::Agent => "Me",
        Who::Them if !l.name.is_empty() => &l.name,
        Who::Them => "Them",
        Who::Private => "Me (privately to you)",
        Who::Note => "You (privately)",
    }
}

pub fn transcript(lines: &[&Line], private: bool) -> String {
    let mut s = String::new();
    for l in lines {
        if !private && !l.who.is_spoken() {
            continue;
        }
        if l.text.trim().is_empty() {
            continue;
        }
        s.push_str(speaker_label(l));
        s.push_str(": ");
        s.push_str(l.text.trim());
        if l.interrupted {
            s.push_str(" [cut off]");
        }
        s.push('\n');
    }
    s
}

fn knowledge(doc: &ContextDoc) -> String {
    let k = doc.for_prompt(&[]);
    if k.is_empty() {
        String::new()
    } else {
        format!("\n\n# What you know\n{k}")
    }
}

/// How the voice (Drama 3) can be directed, from its control syntax (technical report, appendix C).
/// Tags are performed, never read out.
pub const EXPRESSION: &str = "Your words are spoken by an expressive voice. Usually just write the words. Now and then, \
when it really fits: a sound where it happens ([laughs], [sighs], [hesitates], [clears throat], or a [pause] between two thoughts), \
<emphasis>a stressed word</emphasis> or <whisper>something quiet</whisper>, or a mood at the very start of a line ([warmly], [dryly]).";

/// Messages for a spoken reply, as a chat: the model *is* me, so everything I said on the call
/// (myself or through the agent) is its own earlier turns (`assistant`), and the others' lines are
/// `user` turns prefixed with who said it ("Ben: ..."). Consecutive lines of one side are
/// merged so the roles alternate (some chat templates require it). What this turn needs (the
/// time, rules, whom to answer) goes into a bracketed note after the last line, so everything
/// before it stays a stable prefix the server can reuse from its cache.
/// `forced`: an answer is owed (a question, or the user asked). `hint`: what to get across.
pub fn reply_messages(c: &Context, forced: bool, hint: Option<&str>) -> Vec<Value> {
    let system = format!(
        "{scenario}\n\n\
         You are on a live voice call, speaking out loud in my voice: you are me. Your earlier \
         turns in this chat are what I said on the call; the others' lines come as \"Name: words\". \
         Go with the flow of the conversation, the way I would.\n\
         - Say only the words: usually one short sentence, at most two.\n\
         - React to what was just said. Don't repeat yourself or them.\n\
         - If nothing needs saying, answer exactly: [silent]\n\
         - {EXPRESSION}{knowledge}",
        scenario = c.scenario.trim(),
        knowledge = knowledge(c.doc),
    );
    let mut msgs = vec![json!({"role": "system", "content": system})];
    fn push(msgs: &mut Vec<Value>, role: &str, text: String) {
        match msgs.last_mut() {
            Some(m) if m["role"] == role => {
                let prev = m["content"].as_str().unwrap_or("").to_string();
                m["content"] = Value::String(format!(
                    "{prev}
{text}"
                ));
            }
            _ => msgs.push(json!({"role": role, "content": text})),
        }
    }
    let spoken: Vec<&Line> = c.lines.iter().copied().filter(|l| l.who.is_spoken() && !l.text.trim().is_empty()).collect();
    for (i, l) in spoken.iter().enumerate() {
        match l.who {
            Who::Me | Who::Agent => {
                if i == 0 {
                    // Chats start with the other side.
                    push(&mut msgs, "user", "(the call is on)".into());
                }
                // A line cut off by someone talking over it ends where it was cut.
                let t = if l.interrupted { format!("{}\u{2014}", l.text.trim()) } else { l.text.trim().to_string() };
                push(&mut msgs, "assistant", t);
            }
            _ => push(&mut msgs, "user", format!("{}: {}", speaker_label(l), l.text.trim())),
        }
    }
    let last = spoken.iter().rev().find(|l| l.who == Who::Them).map(|l| speaker_label(l).to_string());
    let rules = c.doc.items(Section::Rules);
    let rules =
        if rules.is_empty() { String::new() } else { format!(" Remember: {}.", rules.iter().map(|r| r.trim_end_matches('.')).collect::<Vec<_>>().join("; ")) };
    let hint = hint.map(|h| format!(" Get this across: {h}.")).unwrap_or_default();
    let turn = match (forced, last) {
        (true, Some(who)) => format!("Your turn: answer {who}."),
        (true, None) => "Your turn: say something.".to_string(),
        (false, _) => "Your turn: answer, or [silent].".to_string(),
    };
    let note = format!("[It is {now}.{rules}{hint} {turn}]", now = c.now);
    if spoken.is_empty() {
        push(&mut msgs, "user", format!("(the call is on)\n\n{note}"));
    } else {
        // After their last line, or on its own after mine (a reply I asked for).
        let text = if msgs.last().is_some_and(|m| m["role"] == "user") { format!("\n{note}") } else { note };
        push(&mut msgs, "user", text);
    }
    msgs
}

/// Messages for a private command the router passed to the model. `voices`: the people heard
/// on the call ("Speaker 2", "Ben"), so it can name them.
pub fn command_messages(c: &Context, utterance: &str, autopilot: bool, voices: &[String]) -> Vec<Value> {
    let convo = transcript(c.lines, true);
    let mode = if autopilot { "on (you answer the call for me)" } else { "off" };
    let voices = if voices.is_empty() { String::new() } else { format!("\nVoices on the call: {}.", voices.join(", ")) };
    let system = format!(
        "You are Mimic, my private assistant. You run on my PC and hear my calls; I'm talking to you \
         privately, nobody else hears this.\n\
         - If I ask for something a tool does, call the tool.\n\
         - If I tell you something about me, the people, the situation, how to talk or what to do or avoid, \
         store it with context_add.\n\
         - If I tell you the name of a voice on the call (\"name the last speaker Ben\", \"that was Ben\", \
         \"speaker 2 is Ben\"), call name_speaker. Not for people who are only mentioned.\n\
         - Otherwise answer me in one short sentence.\n\
         Autopilot is {mode}. Now: {now}.{voices}{knowledge}\n\n\
         # Recent conversation\n{convo}",
        knowledge = knowledge(c.doc),
        now = c.now,
        convo = if convo.is_empty() { "(nothing yet)\n".into() } else { convo },
    );
    vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": utterance})]
}

/// Messages asking for an updated summary that folds in `older` lines (which will drop out of
/// the verbatim transcript). The answer replaces the context's summary.
pub fn summarize_messages(doc: &ContextDoc, older: &[&Line]) -> Vec<Value> {
    let previous = doc.items(Section::Summary).first().cloned().unwrap_or_default();
    let system = "You keep the running summary of a voice call I'm on (my lines are marked \"Me\"). \
                  Write the updated summary from my point of view, in the first person (\"I asked ...\", never \
                  \"Me ...\"), others by name: at most 6 short sentences, plain prose, in the language of the call. \
                  Keep what matters later: who said what, decisions, open questions, promises, names, numbers. \
                  Drop small talk. Reply with the summary only.";
    let user =
        format!("Summary so far:\n{}\n\nNew conversation to fold in:\n{}", if previous.is_empty() { "(none)" } else { &previous }, transcript(older, false));
    vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": user})]
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {"type": "object", "properties": props, "required": required},
        }
    })
}

/// Tools for command requests. The daemon implements each by name (`agent::tools`).
pub fn command_tools() -> Vec<Value> {
    let sections: Vec<&str> = Section::ALL.iter().filter(|s| **s != Section::Summary).map(|s| s.key()).collect();
    vec![
        tool(
            "note",
            "Save a note for me (what to remember later, a todo, an idea).",
            json!({"text": {"type": "string", "description": "The note, clear and self-contained."}}),
            &["text"],
        ),
        tool("snapshot", "Save the recent conversation and audio.", json!({"label": {"type": "string", "description": "A short title."}}), &[]),
        tool(
            "context_add",
            "Remember something for this conversation.",
            json!({
                "section": {"type": "string", "enum": sections, "description": "me: about me; style: how to talk; people: who someone is (\"Name: who they are\"); situation: what this call is; facts: things to know; rules: what to do or avoid"},
                "text": {"type": "string"}
            }),
            &["section", "text"],
        ),
        tool(
            "context_remove",
            "Forget something remembered before.",
            json!({"text": {"type": "string", "description": "Words of the item to forget."}}),
            &["text"],
        ),
        tool(
            "name_speaker",
            "Update a speaker tag: give a voice on the call its person's name, so its lines read \"Ben: ...\" \
             instead of \"Speaker 2: ...\" from now on. Use it only when I identify a voice, e.g. \"name the last \
             speaker Ben\" -> {speaker: \"last\", name: \"Ben\"}; \"speaker 2 is Ben\" -> {speaker: \"Speaker 2\", name: \"Ben\"}. \
             Not when someone is only talked about.",
            json!({
                "speaker": {"type": "string", "description": "\"last\" for whoever spoke last, or the voice's current tag, e.g. \"Speaker 2\""},
                "name": {"type": "string", "description": "Just the name, e.g. \"Ben\" (no title or description)"}
            }),
            &["speaker", "name"],
        ),
        tool("say", "Say exactly this out loud on the call, in my voice.", json!({"text": {"type": "string"}}), &["text"]),
        tool(
            "reply",
            "Answer the call now, in my voice (you choose the words).",
            json!({"hint": {"type": "string", "description": "Optional: what the answer should get across."}}),
            &[],
        ),
        tool("autopilot", "Turn autopilot on or off (answering the call for me).", json!({"on": {"type": "boolean"}}), &["on"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(who: Who, name: &str, text: &str) -> Line {
        Line { id: 0, at: 0, who, source: "Discord".into(), speaker: None, name: name.into(), text: text.into(), interrupted: false }
    }

    #[test]
    fn reply_prompt_shape() {
        let a = line(Who::Them, "Ben", "Are you coming tonight?");
        let b = line(Who::Private, "", "say yes but late");
        let mut c = line(Who::Agent, "", "Yeah, a bit");
        c.interrupted = true;
        let d = line(Who::Them, "Speaker 2", "Wait, who's driving?");
        let lines = vec![&a, &b, &c, &d];
        let mut doc = ContextDoc::new();
        doc.add(Section::People, "Ben: my brother");
        doc.add(Section::Rules, "Don't promise to drive");
        let ctx = Context { scenario: "You stand in for me.", doc: &doc, lines: &lines, now: "Sat 22:50" };
        let m = reply_messages(&ctx, true, None);
        let sys = m[0]["content"].as_str().unwrap();
        assert!(sys.contains("you are me"));
        assert!(sys.starts_with("You stand in for me."));
        assert!(sys.contains("[silent]") && sys.contains("<whisper>"));
        assert!(sys.contains("# What you know\n## People\n- Ben: my brother"));
        // The system prompt stays stable (cacheable) and clean.
        assert!(!sys.contains("22:50") && !sys.contains('\\'));
        // My lines are the model's own turns; theirs are named user turns; roles alternate.
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(m[1]["content"], "Ben: Are you coming tonight?");
        assert_eq!(m[2]["content"], "Yeah, a bit\u{2014}");
        assert_eq!(m[3]["content"], "Speaker 2: Wait, who's driving?\n\n[It is Sat 22:50. Remember: Don't promise to drive. Your turn: answer Speaker 2.]");
        let open = reply_messages(&ctx, false, Some("we can't"));
        assert!(open[3]["content"].as_str().unwrap().ends_with("Get this across: we can't. Your turn: answer, or [silent].]"));
        // I spoke first and last: the chat still starts and ends with a user turn.
        let me = line(Who::Me, "", "Hi all");
        let lines2 = vec![&me];
        let m2 = reply_messages(&Context { lines: &lines2, ..ctx }, true, None);
        let roles: Vec<&str> = m2.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert!(m2[3]["content"].as_str().unwrap().starts_with("[It is"));
    }

    #[test]
    fn command_and_summary_prompts() {
        let a = line(Who::Them, "Speaker 1", "What's the budget?");
        let b = line(Who::Private, "", "be vague");
        let lines = vec![&a, &b];
        let doc = ContextDoc::new();
        let ctx = Context { scenario: "", doc: &doc, lines: &lines, now: "now" };
        let m = command_messages(&ctx, "that was Ben", true, &["Speaker 1".into()]);
        let sys = m[0]["content"].as_str().unwrap();
        assert!(sys.contains("Speaker 1: What's the budget?"));
        assert!(sys.contains("Me (privately to you): be vague"));
        assert!(sys.contains("Voices on the call: Speaker 1."));
        let names: Vec<String> = command_tools().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"name_speaker".to_string()) && names.contains(&"context_add".to_string()));
        let s = summarize_messages(&doc, &lines);
        assert!(s[1]["content"].as_str().unwrap().contains("(none)"));
        assert!(!s[1]["content"].as_str().unwrap().contains("be vague"));
    }
}
