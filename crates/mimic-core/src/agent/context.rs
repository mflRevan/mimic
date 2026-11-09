//! The agent's context as an explicit, editable document.
//!
//! Instead of an ever-growing chat log, what the agent knows lives in one structured file
//! (`context.md`) with named sections: who you are, how you talk, who you're talking to,
//! the situation, facts, rules and a running summary. You edit it (by voice, the app, the CLI
//! or the file itself), the deterministic router edits it, and the model edits it through
//! tools and while consolidating the conversation, in the spirit of context language models,
//! which manage their own context as a file. Sections are ordered from stable to volatile, so
//! the rendered prompt keeps a long cacheable prefix.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    /// Who you are (the agent speaks as you).
    Me,
    /// How you talk.
    Style,
    /// Who's who: "Alex: my manager".
    People,
    /// What this conversation is about.
    Situation,
    /// Things to know.
    Facts,
    /// Do / don't.
    Rules,
    /// The agent's running summary of the conversation (it maintains this itself).
    Summary,
}

impl Section {
    pub const ALL: [Section; 7] = [Section::Me, Section::Style, Section::People, Section::Situation, Section::Facts, Section::Rules, Section::Summary];

    pub fn key(self) -> &'static str {
        match self {
            Section::Me => "me",
            Section::Style => "style",
            Section::People => "people",
            Section::Situation => "situation",
            Section::Facts => "facts",
            Section::Rules => "rules",
            Section::Summary => "summary",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Section::Me => "Me",
            Section::Style => "How I talk",
            Section::People => "People",
            Section::Situation => "Situation",
            Section::Facts => "Facts",
            Section::Rules => "Rules",
            Section::Summary => "Summary so far",
        }
    }

    /// Lenient: keys, titles and a few synonyms ("instructions" -> rules, "who" -> people).
    pub fn parse(s: &str) -> Option<Section> {
        let l = s.trim().trim_start_matches('#').trim().to_lowercase();
        Some(match l.as_str() {
            "me" | "about me" | "persona" | "identity" => Section::Me,
            "style" | "how i talk" | "tone" | "voice" => Section::Style,
            "people" | "who" | "contacts" | "participants" => Section::People,
            "situation" | "context" | "topic" | "interaction" => Section::Situation,
            "facts" | "fact" | "knowledge" | "memory" => Section::Facts,
            "rules" | "rule" | "instructions" | "instruction" | "dos" | "do and don't" => Section::Rules,
            "summary" | "summary so far" | "notes" | "state" => Section::Summary,
            _ => return None,
        })
    }

    /// Sections describing this conversation (cleared when a new one starts); the rest is about you.
    pub fn is_session(self) -> bool {
        matches!(self, Section::People | Section::Situation | Section::Facts | Section::Rules | Section::Summary)
    }

    /// The summary is one paragraph the model rewrites; the others are lists of items.
    pub fn is_prose(self) -> bool {
        matches!(self, Section::Me | Section::Summary)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextDoc {
    /// Items per section, in [`Section::ALL`] order.
    sections: Vec<(Section, Vec<String>)>,
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect::<String>().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "Alex: my manager" / "Alex - my manager" / "Alex is my manager" -> Some("alex").
fn person_key(item: &str) -> Option<String> {
    let head = item.split([':', '—', '–']).next()?;
    let head = head.split(" - ").next()?.split(" is ").next()?;
    let k = norm(head);
    (!k.is_empty() && k.split_whitespace().count() <= 3 && head.len() < item.len()).then_some(k)
}

impl ContextDoc {
    pub fn new() -> Self {
        Self { sections: Section::ALL.iter().map(|s| (*s, Vec::new())).collect() }
    }

    fn slot(&mut self, s: Section) -> &mut Vec<String> {
        if !self.sections.iter().any(|(k, _)| *k == s) {
            self.sections.push((s, Vec::new()));
            self.sections.sort_by_key(|(k, _)| *k);
        }
        &mut self.sections.iter_mut().find(|(k, _)| *k == s).unwrap().1
    }

    pub fn items(&self, s: Section) -> &[String] {
        self.sections.iter().find(|(k, _)| *k == s).map(|(_, v)| v.as_slice()).unwrap_or(&[])
    }

    /// Add an item. Duplicates are skipped; a person already listed is updated instead.
    /// Prose sections are replaced. Returns false if nothing changed.
    pub fn add(&mut self, s: Section, text: &str) -> bool {
        let t = text.trim().trim_start_matches(['-', '*', '•']).trim().to_string();
        if t.is_empty() {
            return false;
        }
        if s.is_prose() {
            return self.set(s, &t);
        }
        let v = self.slot(s);
        let n = norm(&t);
        if v.iter().any(|x| norm(x) == n) {
            return false;
        }
        if s == Section::People {
            if let Some(k) = person_key(&t) {
                if let Some(x) = v.iter_mut().find(|x| person_key(x).as_deref() == Some(k.as_str())) {
                    *x = t;
                    return true;
                }
            }
        }
        v.push(t);
        // Bound every list: the oldest items go first.
        if v.len() > 24 {
            v.remove(0);
        }
        true
    }

    /// Replace a section's content (prose sections: the whole text; lists: one item per line).
    pub fn set(&mut self, s: Section, text: &str) -> bool {
        let new: Vec<String> = if s.is_prose() {
            let t = text.trim();
            if t.is_empty() {
                vec![]
            } else {
                vec![t.to_string()]
            }
        } else {
            text.lines().map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string()).filter(|l| !l.is_empty()).collect()
        };
        let v = self.slot(s);
        if *v == new {
            return false;
        }
        *v = new;
        true
    }

    /// Remove items mentioning `needle`: every content word of it appears in the item
    /// ("the revenue" matches "The Q3 revenue was 1.2M"). An empty needle clears the section.
    pub fn remove(&mut self, s: Section, needle: &str) -> usize {
        const STOP: [&str; 12] = ["the", "a", "an", "about", "that", "this", "of", "my", "our", "to", "it", "thing"];
        let words: Vec<String> = norm(needle).split_whitespace().filter(|w| !STOP.contains(w)).map(str::to_string).collect();
        let v = self.slot(s);
        let before = v.len();
        if norm(needle).is_empty() {
            v.clear();
        } else if !words.is_empty() {
            v.retain(|x| {
                let item = norm(x);
                let item_words: Vec<&str> = item.split_whitespace().collect();
                !words.iter().all(|w| item_words.iter().any(|iw| iw.starts_with(w.as_str())))
            });
        }
        before - v.len()
    }

    /// Remove `needle` from whichever sections mention it.
    pub fn forget(&mut self, needle: &str) -> usize {
        Section::ALL.iter().filter(|s| **s != Section::Me).map(|s| self.remove(*s, needle)).sum()
    }

    /// Start a new conversation: keep who you are and how you talk.
    pub fn new_session(&mut self) {
        for s in Section::ALL {
            if s.is_session() {
                self.slot(s).clear();
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|(_, v)| v.is_empty())
    }

    /// The document as Markdown (also its file format). Empty sections are kept so the file
    /// shows where things go.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("# Context\n\n");
        for s in Section::ALL {
            let _ = writeln!(out, "## {}\n", s.title());
            for item in self.items(s) {
                if s.is_prose() {
                    let _ = writeln!(out, "{item}");
                } else {
                    let _ = writeln!(out, "- {item}");
                }
            }
            out.push('\n');
        }
        out
    }

    /// Parse the Markdown file format (unknown headings are ignored; text outside lists in a
    /// list section becomes items).
    pub fn from_markdown(md: &str) -> Self {
        let mut doc = Self::new();
        let mut cur: Option<Section> = None;
        let mut prose = String::new();
        let flush = |doc: &mut Self, cur: Option<Section>, prose: &mut String| {
            if let Some(s) = cur.filter(|s| s.is_prose()) {
                doc.set(s, prose.trim());
            }
            prose.clear();
        };
        for line in md.lines() {
            if let Some(h) = line.strip_prefix("## ") {
                flush(&mut doc, cur, &mut prose);
                cur = Section::parse(h);
                continue;
            }
            if line.starts_with("# ") {
                continue;
            }
            match cur {
                Some(s) if s.is_prose() => {
                    prose.push_str(line);
                    prose.push('\n');
                }
                Some(s) => {
                    doc.add(s, line);
                }
                None => {}
            }
        }
        flush(&mut doc, cur, &mut prose);
        doc
    }

    /// For the model: only non-empty sections, as compact Markdown. `skip` leaves sections out
    /// (e.g. the summary when it is rendered separately).
    pub fn for_prompt(&self, skip: &[Section]) -> String {
        let mut out = String::new();
        for s in Section::ALL {
            let items = self.items(s);
            if items.is_empty() || skip.contains(&s) {
                continue;
            }
            let _ = writeln!(out, "## {}", s.title());
            for item in items {
                if s.is_prose() {
                    let _ = writeln!(out, "{item}");
                } else {
                    let _ = writeln!(out, "- {item}");
                }
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes_and_updates_people() {
        let mut d = ContextDoc::new();
        assert!(d.add(Section::Rules, "Keep it short"));
        assert!(!d.add(Section::Rules, "keep it short."));
        assert!(d.add(Section::People, "Alex: my manager"));
        assert!(d.add(Section::People, "Alex: my manager, likes numbers"));
        assert!(d.add(Section::People, "Sam - a friend from uni"));
        assert_eq!(d.items(Section::People), ["Alex: my manager, likes numbers", "Sam - a friend from uni"]);
        assert!(d.add(Section::Summary, "We agreed on Friday."));
        assert!(d.add(Section::Summary, "We agreed on Friday at 3."));
        assert_eq!(d.items(Section::Summary), ["We agreed on Friday at 3."]);
    }

    #[test]
    fn remove_forget_and_sessions() {
        let mut d = ContextDoc::new();
        d.add(Section::Me, "I'm Alex, a developer.");
        d.add(Section::Style, "casual");
        d.add(Section::Facts, "The budget is 40k");
        d.add(Section::Rules, "Don't mention the budget");
        assert_eq!(d.forget("budget"), 2);
        d.add(Section::Facts, "The Q3 revenue was 1.2 million");
        assert_eq!(d.forget("the revenue"), 1);
        d.add(Section::Facts, "Kim likes jazz");
        assert_eq!(d.forget("the revenue"), 0);
        d.add(Section::Situation, "Weekly sync");
        d.new_session();
        assert!(d.items(Section::Situation).is_empty());
        assert_eq!(d.items(Section::Style), ["casual"]);
        assert_eq!(d.items(Section::Me), ["I'm Alex, a developer."]);
        assert_eq!(d.remove(Section::Style, ""), 1);
    }

    #[test]
    fn markdown_roundtrip_and_prompt() {
        let mut d = ContextDoc::new();
        d.add(Section::Me, "I'm Alex.\nI build audio tools.");
        d.add(Section::People, "Alex: my manager");
        d.add(Section::Summary, "Talked about the release.");
        let md = d.to_markdown();
        assert!(md.contains("## How I talk\n\n"));
        let back = ContextDoc::from_markdown(&md);
        assert_eq!(back, d);
        // Hand-written files are forgiving.
        let h = ContextDoc::from_markdown("## instructions\n* be brief\nno jargon\n## Unknown\n- x\n## people\n- Kim: designer\n");
        assert_eq!(h.items(Section::Rules), ["be brief", "no jargon"]);
        assert_eq!(h.items(Section::People), ["Kim: designer"]);
        let p = d.for_prompt(&[Section::Summary]);
        assert!(p.starts_with("## Me\nI'm Alex.\nI build audio tools."));
        assert!(p.contains("## People\n- Alex: my manager"));
        assert!(!p.contains("Summary") && !p.contains("How I talk"));
    }
}
