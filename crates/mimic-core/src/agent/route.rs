//! Deterministic routing of what you tell the agent privately.
//!
//! Clear phrasings go straight to an action or a context section, with no model call: fast,
//! predictable and independent of the model's quality. Anything else falls through to the
//! model (with tools), which may still edit the context.
//!
//! "note that the review moved to Friday"   -> Note
//! "this is Alex, my manager"               -> People: "Alex: my manager"
//! "we're talking about the release"        -> Situation
//! "keep it short" / "sound more excited"   -> Style
//! "don't agree to any dates"               -> Rules
//! "remember that the budget is 40k"        -> Facts
//! "tell them I'm running late"             -> Say "I'm running late"
//! "answer him" / "reply that we can't"     -> Reply (with a hint)
//! "take over" / "I'm back"                 -> Autopilot on / off
//! "forget about the budget"                -> Forget
//! "that was Ben" / "speaker 2 is Ben"  -> NameSpeaker
//! "new conversation"                       -> NewSession

use super::context::Section;

/// Which voice is meant.
#[derive(Debug, Clone, PartialEq)]
pub enum SpeakerRef {
    /// Whoever spoke last.
    Last,
    /// "Speaker 2" (the id).
    Id(u32),
}

/// "Ben", "Ben Carter": one to three capitalized words.
fn person_name(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches(['.', '!', ',']);
    let words: Vec<&str> = s.split_whitespace().collect();
    (!words.is_empty() && words.len() <= 3 && words.iter().all(|w| w.chars().next().is_some_and(|c| c.is_uppercase()))).then(|| words.join(" "))
}

/// "Sarah from design" -> ("Sarah", "from design"); "Anna Lee, my manager" -> ("Anna Lee",
/// "my manager"). The name is the leading capitalized words (up to three).
fn name_and_description(s: &str) -> Option<(String, String)> {
    let s = s.trim().trim_end_matches(['.', '!']);
    let (head, tail) = match s.find(',') {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let words: Vec<&str> = head.split_whitespace().collect();
    let n = words.iter().take_while(|w| w.chars().next().is_some_and(|c| c.is_uppercase())).count().min(3);
    if n == 0 {
        return None;
    }
    let rest = words[n..].join(" ");
    let desc = [rest.as_str(), tail].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(", ");
    Some((words[..n].join(" "), describes(&desc)))
}

/// Keep a description only if it says who someone is, not what to do ("save him as speaker").
fn describes(desc: &str) -> String {
    let l = desc.to_lowercase();
    let instruction = ["speaker", "save", "store", "remember", "name", "tag", "label", "call him", "call her", "note"].iter().any(|w| l.contains(w));
    if instruction {
        String::new()
    } else {
        desc.to_string()
    }
}

/// After an explicit "name the last speaker ...": the name even if transcribed in lowercase
/// ("ben carter from design" -> "Ben Carter", "from design").
fn name_loose(s: &str) -> Option<(String, String)> {
    if let Some(r) = name_and_description(s) {
        return Some(r);
    }
    const STOP: [&str; 8] = ["from", "who", "my", "the", "our", "is", "and", "with"];
    let s = s.trim().trim_end_matches(['.', '!']);
    let (head, tail) = match s.find(',') {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let words: Vec<&str> = head.split_whitespace().collect();
    let n = words.iter().take_while(|w| !STOP.contains(&w.to_lowercase().as_str())).count().min(3);
    if n == 0 {
        return None;
    }
    let cap = |w: &&str| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    let rest = words[n..].join(" ");
    let desc = [rest.as_str(), tail].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(", ");
    Some((words[..n].iter().map(cap).collect::<Vec<_>>().join(" "), describes(&desc)))
}

/// "that was Ben (who just spoke)", "Ben just spoke", "the last speaker is Ben",
/// "speaker 2 is Ben (from design)".
fn name_speaker(l: &str, o: &str) -> Option<Route> {
    let tail_words = [" who just spoke", " who just talked", " speaking", " talking", " just now", " right now", " there"];
    let strip_tail = |s: &str| {
        let mut s = s.trim().trim_end_matches(['.', '!']).to_string();
        for t in tail_words {
            if s.to_lowercase().ends_with(t) {
                s.truncate(s.len() - t.len());
            }
        }
        s
    };
    // The canonical phrasings: "name the last speaker Ben", "update speaker name with Ben",
    // "name speaker 2 Ben".
    for p in [
        "name the last speaker ",
        "name last speaker ",
        "call the last speaker ",
        "update speaker name with ",
        "update speaker name to ",
        "update the speaker name with ",
        "update the speaker name to ",
        "speaker name ",
    ] {
        if l.starts_with(p) {
            return name_loose(&strip_tail(&o[p.len()..])).map(|(n, d)| Route::NameSpeaker(SpeakerRef::Last, n, d));
        }
    }
    if let Some(rest) = l.strip_prefix("name speaker ").or_else(|| l.strip_prefix("call speaker ")) {
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = num.parse::<u32>() {
            let at = l.len() - rest.len() + num.len();
            let name = o[at..].trim_start().trim_start_matches([':', '-']).trim_start();
            let name = name.strip_prefix("as ").or_else(|| name.strip_prefix("to ")).unwrap_or(name);
            return name_loose(name).map(|(n, d)| Route::NameSpeaker(SpeakerRef::Id(id), n, d));
        }
    }
    for p in [
        "that was ",
        "that's ",
        "thats ",
        "the last speaker is ",
        "the last speaker was ",
        "the last one was ",
        "the last one is ",
        "who just spoke is ",
        "who just spoke was ",
        "the speaker who just spoke was ",
        "the speaker who just spoke is ",
        "the person who just spoke was ",
        "the person who just spoke is ",
        "the one who just spoke was ",
        "the one who just spoke is ",
        "the guy who just spoke was ",
        "the guy who just spoke is ",
    ] {
        if l.starts_with(p) {
            let rest = strip_tail(&o[p.len()..]);
            // "that's Sam from accounting" may describe someone just mentioned (People); "that was
            // Sam (from accounting)" is who just spoke.
            if p.starts_with("that's") || p.starts_with("thats") {
                if rest.contains(',') || rest.split_whitespace().count() > 3 {
                    return None;
                }
                return person_name(&rest).map(|n| Route::NameSpeaker(SpeakerRef::Last, n, String::new()));
            }
            return name_and_description(&rest).map(|(n, d)| Route::NameSpeaker(SpeakerRef::Last, n, d));
        }
    }
    for suffix in [" just spoke", " just talked", " was speaking", " was talking", " is the one talking"] {
        if let Some(name) = l.strip_suffix(suffix) {
            return person_name(&o[..name.len()]).map(|n| Route::NameSpeaker(SpeakerRef::Last, n, String::new()));
        }
    }
    // "speaker 2 is Ben"
    if let Some(rest) = l.strip_prefix("speaker ") {
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let (Ok(id), Some(i)) = (num.parse::<u32>(), l.find(" is ")) {
            return name_and_description(&o[i + 4..]).map(|(n, d)| Route::NameSpeaker(SpeakerRef::Id(id), n, d));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Note(String),
    Snapshot(String),
    Say(String),
    Reply(Option<String>),
    Autopilot(bool),
    Stop,
    Context(Section, String),
    Forget(String),
    /// A voice on the call is this person (and, if given, who they are: "from design").
    NameSpeaker(SpeakerRef, String, String),
    NewSession,
    /// Needs the model (a question, or something the patterns don't cover).
    Model,
}

const LEAD: [&str; 9] = ["please", "ok", "okay", "hey", "so", "and", "also", "now", "alright"];

/// Lowercased, trimmed, leading fillers ("okay, ...") and trailing punctuation removed; and the
/// same span of the original text (to keep its casing in what gets stored).
fn clean(text: &str) -> (String, String) {
    let mut orig = text.trim().trim_end_matches(['.', '!']).trim().to_string();
    loop {
        let first = orig.split_whitespace().next().unwrap_or("");
        let word = first.trim_end_matches([',', ':', '.']).to_lowercase();
        if !first.is_empty() && LEAD.contains(&word.as_str()) && orig.len() > first.len() {
            orig = orig[first.len()..].trim_start().to_string();
        } else {
            break;
        }
    }
    (orig.to_lowercase(), orig)
}

/// The original-case remainder after a lowercase prefix of length `n`.
fn rest(orig: &str, n: usize) -> String {
    orig.get(n..).unwrap_or("").trim().trim_start_matches([',', ':']).trim().to_string()
}

fn strip_any<'a>(l: &str, prefixes: &[&'a str]) -> Option<&'a str> {
    prefixes.iter().copied().filter(|p| l.starts_with(p)).max_by_key(|p| p.len())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn is_question(l: &str) -> bool {
    let first = l.split_whitespace().next().unwrap_or("");
    l.trim_end().ends_with('?')
        || matches!(
            first,
            "what"
                | "what's"
                | "whats"
                | "who"
                | "who's"
                | "when"
                | "where"
                | "why"
                | "how"
                | "did"
                | "does"
                | "do"
                | "is"
                | "are"
                | "can"
                | "could"
                | "would"
                | "should"
                | "was"
                | "were"
                | "which"
        )
}

pub fn route(text: &str) -> Route {
    let (l, o) = clean(text);
    if l.is_empty() {
        return Route::Model;
    }
    let words: Vec<&str> = l.split_whitespace().collect();

    // Control.
    if matches!(l.as_str(), "stop" | "stop talking" | "shut up" | "be quiet" | "quiet" | "hush" | "enough") {
        return Route::Stop;
    }
    if matches!(
        l.as_str(),
        "take over" | "you take over" | "autopilot" | "autopilot on" | "turn on autopilot" | "go ahead and answer for me" | "handle it" | "you handle it"
    ) {
        return Route::Autopilot(true);
    }
    if matches!(
        l.as_str(),
        "i'm back" | "im back" | "i'll take it from here" | "ill take it from here" | "autopilot off" | "turn off autopilot" | "stop autopilot" | "i got it"
    ) {
        return Route::Autopilot(false);
    }
    if matches!(l.as_str(), "new conversation" | "new call" | "start over" | "reset the context" | "clear the context" | "fresh start") {
        return Route::NewSession;
    }

    // Actions.
    if let Some(p) = strip_any(&l, &["note that ", "note ", "make a note that ", "make a note ", "write down that ", "write down ", "jot down ", "add a note "])
    {
        let r = rest(&o, p.len());
        if !r.is_empty() {
            return Route::Note(capitalize(&r));
        }
    }
    if let Some(p) = strip_any(&l, &["take a snapshot", "save a snapshot", "snapshot", "save this conversation", "save the conversation", "bookmark this"]) {
        let label = rest(&o, p.len());
        let label = label.trim_start_matches("of ").trim_start_matches("called ").trim_start_matches("named ").to_string();
        return Route::Snapshot(if label.is_empty() || label == "this" { "Snapshot".into() } else { capitalize(&label) });
    }
    if let Some(p) = strip_any(
        &l,
        &[
            "tell them that ",
            "tell them ",
            "tell him that ",
            "tell him ",
            "tell her that ",
            "tell her ",
            "tell everyone that ",
            "tell everyone ",
            "say that ",
            "say ",
        ],
    ) {
        let r = rest(&o, p.len());
        if !r.is_empty() {
            return Route::Say(capitalize(&r));
        }
    }
    if let Some(p) = strip_any(&l, &["answer that ", "reply that ", "respond that ", "answer with ", "reply with ", "answer saying ", "reply saying "]) {
        return Route::Reply(Some(rest(&o, p.len())).filter(|r| !r.is_empty()));
    }
    if matches!(words.first().copied(), Some("answer" | "reply" | "respond")) && words.len() <= 3 {
        return Route::Reply(None);
    }
    if let Some(p) = strip_any(&l, &["forget about ", "forget that ", "forget ", "drop the rule about ", "never mind about "]) {
        let r = rest(&o, p.len());
        if !r.is_empty() {
            return Route::Forget(r);
        }
    }

    if is_question(&l) {
        return Route::Model;
    }

    if let Some(r) = name_speaker(&l, &o) {
        return r;
    }

    // Context: people.
    // "this is Alex, my manager" / "that's Sam from accounting" / "Alex is my manager"
    if let Some(p) = strip_any(
        &l,
        &[
            "this is ",
            "that is ",
            "that's ",
            "thats ",
            "he is ",
            "she is ",
            "i'm talking to ",
            "im talking to ",
            "i'm talking with ",
            "i'm speaking with ",
            "i'm speaking to ",
            "you're talking to ",
            "youre talking to ",
        ],
    ) {
        let r = rest(&o, p.len());
        let (name, what) = match r.split_once([',', ':']) {
            Some((n, w)) => (n.trim().to_string(), w.trim().to_string()),
            None => match r.find(" from ").or_else(|| r.find(" my ")).or_else(|| r.find(" who ")) {
                Some(i) => (r[..i].trim().to_string(), r[i..].trim().to_string()),
                None => (r.clone(), String::new()),
            },
        };
        let looks_like_name = !name.is_empty() && name.split_whitespace().count() <= 3 && name.chars().next().is_some_and(|c| c.is_uppercase());
        if looks_like_name {
            let item = if what.is_empty() { name } else { format!("{name}: {what}") };
            return Route::Context(Section::People, item);
        }
        if (p.starts_with("this is ") || p.starts_with("that is ")) && r.split_whitespace().count() >= 2 {
            // "this is a job interview" -> the situation ("this is fine" is just a remark).
            return Route::Context(Section::Situation, capitalize(&r));
        }
    }
    if let Some((name, what)) = o.split_once(" is my ").or_else(|| o.split_once(" is our ")) {
        if name.split_whitespace().count() <= 3 && name.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Route::Context(Section::People, format!("{}: my {}", name.trim(), what.trim()));
        }
    }

    // Context: situation.
    if let Some(p) = strip_any(
        &l,
        &[
            "we're talking about ",
            "were talking about ",
            "we are talking about ",
            "we're discussing ",
            "we are discussing ",
            "this call is about ",
            "this meeting is about ",
            "the call is about ",
            "the meeting is about ",
            "this conversation is about ",
            "the topic is ",
        ],
    ) {
        return Route::Context(Section::Situation, capitalize(&format!("About {}", rest(&o, p.len()))));
    }
    if let Some(p) = strip_any(&l, &["we're in a ", "we are in a ", "we're on a ", "we are on a ", "we're in an ", "we're on an "]) {
        let article = if p.ends_with("an ") { "An" } else { "A" };
        return Route::Context(Section::Situation, format!("{article} {}", rest(&o, p.len())));
    }

    // Context: facts.
    if let Some(p) = strip_any(
        &l,
        &["remember that ", "remember ", "keep in mind that ", "keep in mind ", "fyi ", "for your information ", "good to know: ", "background: "],
    ) {
        let r = rest(&o, p.len());
        if !r.is_empty() {
            return Route::Context(Section::Facts, capitalize(&r));
        }
    }

    // Context: rules (do / don't).
    if strip_any(&l, &["don't ", "dont ", "do not ", "never ", "always ", "make sure ", "avoid ", "try to ", "try not to ", "no ", "only "]).is_some() {
        return Route::Context(Section::Rules, capitalize(&o));
    }

    // Context: style.
    let style_lead = [
        "keep it ",
        "keep your answers ",
        "talk ",
        "speak ",
        "sound ",
        "be more ",
        "be less ",
        "be ",
        "act ",
        "use ",
        "answer in ",
        "reply in ",
        "respond in ",
        "more ",
        "less ",
    ];
    if strip_any(&l, &style_lead).is_some() && words.len() <= 12 {
        return Route::Context(Section::Style, capitalize(&o));
    }
    // About you: "I'm a/an ...", "I'm Alex, ...", "my name is ...", "I work/build/live ...".
    let about_me = strip_any(
        &l,
        &["i'm a ", "i'm an ", "im a ", "i am a ", "i am an ", "my name is ", "call me ", "i work ", "i live ", "i build ", "i make ", "i run ", "i study "],
    )
    .is_some();
    let named = strip_any(&l, &["i'm ", "i am "]).is_some_and(|p| o.get(p.len()..).and_then(|r| r.chars().next()).is_some_and(char::is_uppercase));
    if (about_me || named) && words.len() <= 16 {
        return Route::Context(Section::Me, capitalize(&o));
    }

    Route::Model
}

#[cfg(test)]
mod tests {
    use super::*;
    use Route::*;
    use Section::*;

    #[test]
    fn actions() {
        assert_eq!(route("Note that the review moved to Friday at 3."), Note("The review moved to Friday at 3".into()));
        assert_eq!(route("please take a snapshot"), Snapshot("Snapshot".into()));
        assert_eq!(route("take a snapshot of the pricing talk"), Snapshot("The pricing talk".into()));
        assert_eq!(route("Tell them I'm running ten minutes late."), Say("I'm running ten minutes late".into()));
        assert_eq!(route("say sounds good"), Say("Sounds good".into()));
        assert_eq!(route("answer him"), Reply(None));
        assert_eq!(route("reply that we can't do Friday"), Reply(Some("we can't do Friday".into())));
        assert_eq!(route("take over"), Autopilot(true));
        assert_eq!(route("Okay, I'm back."), Autopilot(false));
        assert_eq!(route("shut up"), Stop);
        assert_eq!(route("forget about the budget"), Forget("the budget".into()));
        assert_eq!(route("new conversation"), NewSession);
    }

    #[test]
    fn context() {
        assert_eq!(route("This is Alex, my manager."), Context(People, "Alex: my manager".into()));
        assert_eq!(route("that's Sam from accounting"), Context(People, "Sam: from accounting".into()));
        assert_eq!(route("Kim is my sister"), Context(People, "Kim: my sister".into()));
        assert_eq!(route("I'm talking to Jordan"), Context(People, "Jordan".into()));
        assert_eq!(route("we're talking about the Q3 release"), Context(Situation, "About the Q3 release".into()));
        assert_eq!(route("this is a job interview"), Context(Situation, "A job interview".into()));
        assert_eq!(route("remember that the budget is 40k"), Context(Facts, "The budget is 40k".into()));
        assert_eq!(route("Don't agree to any dates."), Context(Rules, "Don't agree to any dates".into()));
        assert_eq!(route("keep it short"), Context(Style, "Keep it short".into()));
        assert_eq!(route("sound a bit more excited"), Context(Style, "Sound a bit more excited".into()));
        assert_eq!(route("I'm a backend developer at Acme"), Context(Me, "I'm a backend developer at Acme".into()));
        assert_eq!(route("I'm Alex, I build audio software"), Context(Me, "I'm Alex, I build audio software".into()));
        assert_eq!(route("I build audio software"), Context(Me, "I build audio software".into()));
        // "I'm tired" isn't about who you are.
        assert_eq!(route("I'm tired of this call"), Model);
    }

    #[test]
    fn naming_voices() {
        use SpeakerRef::*;
        let n = |r, name: &str, d: &str| NameSpeaker(r, name.into(), d.into());
        assert_eq!(route("that was Ben who just spoke"), n(Last, "Ben", ""));
        assert_eq!(route("That was Ben."), n(Last, "Ben", ""));
        assert_eq!(route("Ben just spoke"), n(Last, "Ben", ""));
        assert_eq!(route("the last speaker is Anna Lee"), n(Last, "Anna Lee", ""));
        assert_eq!(route("speaker 2 is Ben"), n(Id(2), "Ben", ""));
        assert_eq!(route("speaker 2 is Sarah from design"), n(Id(2), "Sarah", "from design"));
        assert_eq!(route("that was Anna Lee, my manager"), n(Last, "Anna Lee", "my manager"));
        assert_eq!(route("that was Sam from accounting"), n(Last, "Sam", "from accounting"));
        assert_eq!(route("Name the last speaker Ben."), n(Last, "Ben", ""));
        assert_eq!(route("that was Sam, save him as speaker."), n(Last, "Sam", ""));
        assert_eq!(route("The speaker who just spoke was Sam."), n(Last, "Sam", ""));
        assert_eq!(route("update speaker name with Ben"), n(Last, "Ben", ""));
        assert_eq!(route("okay, name speaker 3 Ben Carter"), n(Id(3), "Ben Carter", ""));
        assert_eq!(route("name speaker 2 as Sarah, from design"), n(Id(2), "Sarah", "from design"));
        // Lowercase names from speech recognition still work in the canonical form.
        assert_eq!(route("name the last speaker ben"), n(Last, "Ben", ""));
        // Describing someone is still context.
        assert_eq!(route("that's Sam from accounting"), Context(People, "Sam: from accounting".into()));
        assert_eq!(route("that was fun"), Model);
    }

    #[test]
    fn falls_through_to_the_model() {
        assert_eq!(route("what did he just ask?"), Model);
        assert_eq!(route("how long have we been talking"), Model);
        assert_eq!(route("hmm let me think about the pricing thing he mentioned earlier and whether it matters"), Model);
        assert_eq!(route(""), Model);
        // A remark isn't context.
        assert_eq!(route("this is fine"), Model);
        assert_eq!(route("we're on a sales call"), Context(Situation, "A sales call".into()));
    }
}
