//! What was said, by whom, in order. The agent's short-term memory of the conversation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Who {
    /// You, speaking into the conversation.
    Me,
    /// The others (what you hear).
    Them,
    /// The agent, speaking in your voice.
    Agent,
    /// You, privately to the agent (never heard by the others).
    Private,
    /// The agent's private answer to you, or a tool result.
    Note,
}

impl Who {
    /// Part of the spoken conversation (vs. the private channel).
    pub fn is_spoken(self) -> bool {
        matches!(self, Who::Me | Who::Them | Who::Agent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub id: u64,
    /// Unix milliseconds.
    pub at: u64,
    pub who: Who,
    /// Where it was heard ("Discord"); empty for your mic and the agent.
    #[serde(default)]
    pub source: String,
    /// Which voice said it (the speaker book's id), when known.
    #[serde(default)]
    pub speaker: Option<u32>,
    /// Who said it, for display and the model: "Ben", "Speaker 2". Empty = unknown.
    #[serde(default)]
    pub name: String,
    pub text: String,
    /// Speech that was cut off (barge-in) before it was finished.
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    lines: std::collections::VecDeque<Line>,
    cap: usize,
    next: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new(400)
    }
}

impl Timeline {
    pub fn new(cap: usize) -> Self {
        Self { lines: Default::default(), cap: cap.max(8), next: 1 }
    }

    /// Add a line. Consecutive lines by the same speaker within `merge_ms` are joined, so a
    /// sentence split by a breath stays one turn.
    pub fn push(&mut self, at: u64, who: Who, source: &str, text: &str, merge_ms: u64) -> &Line {
        self.push_voice(at, who, source, None, "", text, merge_ms)
    }

    /// [`Timeline::push`] for a line from a known voice.
    #[allow(clippy::too_many_arguments)]
    pub fn push_voice(&mut self, at: u64, who: Who, source: &str, speaker: Option<u32>, name: &str, text: &str, merge_ms: u64) -> &Line {
        let text = text.trim();
        let merge = self.lines.back().is_some_and(|l| {
            l.who == who && l.source == source && l.speaker == speaker && who.is_spoken() && !l.interrupted && at.saturating_sub(l.at) <= merge_ms
        });
        if merge {
            let l = self.lines.back_mut().unwrap();
            if !l.text.is_empty() {
                l.text.push(' ');
            }
            l.text.push_str(text);
            l.at = at;
        } else {
            let id = self.next;
            self.next += 1;
            self.lines.push_back(Line { id, at, who, source: source.to_string(), speaker, name: name.to_string(), text: text.to_string(), interrupted: false });
            while self.lines.len() > self.cap {
                self.lines.pop_front();
            }
        }
        self.lines.back().unwrap()
    }

    pub fn mark_interrupted(&mut self, id: u64, said: &str) {
        if let Some(l) = self.lines.iter_mut().find(|l| l.id == id) {
            l.interrupted = true;
            if !said.trim().is_empty() {
                l.text = said.trim().to_string();
            }
        }
    }

    /// Replace a line's text.
    pub fn set_text(&mut self, id: u64, text: &str) -> Option<&Line> {
        let l = self.lines.iter_mut().find(|l| l.id == id)?;
        l.text = text.trim().to_string();
        Some(l)
    }

    /// A voice got a name: relabel its lines. Returns the changed lines.
    pub fn rename_speaker(&mut self, speaker: u32, name: &str) -> Vec<Line> {
        let mut changed = Vec::new();
        for l in self.lines.iter_mut().filter(|l| l.speaker == Some(speaker)) {
            l.name = name.to_string();
            changed.push(l.clone());
        }
        changed
    }

    /// Append text to a line.
    pub fn append(&mut self, id: u64, text: &str) -> Option<&Line> {
        let l = self.lines.iter_mut().find(|l| l.id == id)?;
        if !l.text.is_empty() {
            l.text.push(' ');
        }
        l.text.push_str(text.trim());
        Some(l)
    }

    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &Line> {
        self.lines.iter()
    }

    /// The last `n` lines matching `keep`, oldest first.
    pub fn recent(&self, n: usize, keep: impl Fn(&Line) -> bool) -> Vec<&Line> {
        let mut v: Vec<&Line> = self.lines.iter().rev().filter(|l| keep(l)).take(n).collect();
        v.reverse();
        v
    }

    pub fn last(&self) -> Option<&Line> {
        self.lines.back()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_same_speaker_and_caps() {
        let mut t = Timeline::new(8);
        t.push(1000, Who::Them, "", "So I was thinking", 1500);
        t.push(2000, Who::Them, "", "we could go Friday.", 1500);
        t.push(2500, Who::Me, "", "Sure.", 1500);
        t.push(9000, Who::Me, "", "Wait.", 1500);
        let v: Vec<_> = t.lines().map(|l| (l.who, l.text.clone())).collect();
        assert_eq!(v, vec![(Who::Them, "So I was thinking we could go Friday.".into()), (Who::Me, "Sure.".into()), (Who::Me, "Wait.".into())]);
        // Private lines never merge.
        t.push(9100, Who::Private, "", "be nice", 1500);
        t.push(9200, Who::Private, "", "and short", 1500);
        assert_eq!(t.len(), 5);
        for i in 0..20 {
            t.push(20_000 + i * 10_000, Who::Them, "", "x", 0);
        }
        assert_eq!(t.len(), 8);
        let r = t.recent(2, |l| l.who == Who::Them);
        assert_eq!(r.len(), 2);
        assert!(r[0].at < r[1].at);
    }

    #[test]
    fn interrupted_lines_keep_what_was_said() {
        let mut t = Timeline::new(8);
        let a = t.push_voice(0, Who::Them, "Discord", Some(1), "Speaker 1", "hi", 1000).id;
        let b = t.push_voice(10, Who::Them, "Discord", Some(2), "Speaker 2", "hey", 1000).id;
        assert_ne!(a, b, "different voices never merge");
        assert_eq!(t.rename_speaker(1, "Ben").len(), 1);
        assert_eq!(t.lines().next().unwrap().name, "Ben");
        t.clear();
        let id = t.push(0, Who::Agent, "", "One. Two. Three.", 0).id;
        t.mark_interrupted(id, "One.");
        assert_eq!(t.append(id, "Two.").unwrap().text, "One. Two.");
        t.mark_interrupted(id, "One.");
        assert_eq!(t.last().unwrap().text, "One.");
        assert!(t.last().unwrap().interrupted);
        // A new agent line after an interruption is a new turn.
        t.push(10, Who::Agent, "", "Sorry, go ahead.", 1000);
        assert_eq!(t.len(), 2);
    }
}
