//! Streaming helpers: OpenAI-style server-sent events, tool-call accumulation, and cutting a
//! token stream into speakable sentences as early as possible.

use serde_json::Value;

// ------------------------------------------------------------------ SSE

/// Splits a byte stream into SSE `data:` payloads.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum SseEvent {
    Data(String),
    Done,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<SseEvent>) {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(d) = line.strip_prefix("data:") {
                let d = d.trim_start();
                if d == "[DONE]" {
                    out.push(SseEvent::Done);
                } else if !d.is_empty() {
                    out.push(SseEvent::Data(d.to_string()));
                }
            }
        }
    }
}

// ------------------------------------------------------------------ chat deltas

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON arguments as streamed (parse when complete).
    pub arguments: String,
}

impl ToolCall {
    pub fn args(&self) -> Value {
        serde_json::from_str(&self.arguments).unwrap_or(Value::Null)
    }
}

/// Accumulates `choices[0].delta` chunks of a streamed chat completion.
#[derive(Debug, Default)]
pub struct ChatAccumulator {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

impl ChatAccumulator {
    /// Apply one chunk; returns the new text content in it (for streaming to TTS).
    pub fn apply(&mut self, chunk: &Value) -> String {
        let choice = &chunk["choices"][0];
        if let Some(r) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(r.to_string());
        }
        // Non-streamed responses use `message`, streamed ones `delta`.
        let delta = if choice["delta"].is_object() { &choice["delta"] } else { &choice["message"] };
        let mut new = String::new();
        if let Some(t) = delta["content"].as_str() {
            self.content.push_str(t);
            new.push_str(t);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (n, c) in calls.iter().enumerate() {
                let idx = c["index"].as_u64().map(|i| i as usize).unwrap_or(n);
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(ToolCall::default());
                }
                let tc = &mut self.tool_calls[idx];
                if let Some(id) = c["id"].as_str() {
                    tc.id = id.to_string();
                }
                if let Some(name) = c["function"]["name"].as_str() {
                    tc.name.push_str(name);
                }
                match &c["function"]["arguments"] {
                    Value::String(a) => tc.arguments.push_str(a),
                    Value::Object(_) => tc.arguments = c["function"]["arguments"].to_string(),
                    _ => {}
                }
            }
        }
        new
    }
}

// ------------------------------------------------------------------ sentences

/// The model's way of saying "nothing to say right now".
pub const SILENT: &str = "[silent]";

/// Cuts streamed text into chunks that can be spoken on their own, as early as possible:
/// at sentence ends, or at a comma/dash once a chunk gets long.
#[derive(Default)]
pub struct SentenceSplitter {
    buf: String,
    /// At least this many characters before a sentence end counts (avoids "Hi." "Ok." chunks
    /// that sound choppy). The first chunk may be short so speech starts fast.
    pub min_chars: usize,
    emitted: usize,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self { buf: String::new(), min_chars: 24, emitted: 0 }
    }

    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        loop {
            let min = if self.emitted == 0 { 2 } else { self.min_chars };
            let Some(cut) = find_cut(&self.buf, min) else { break };
            let piece: String = self.buf[..cut].to_string();
            self.buf.drain(..cut);
            if let Some(s) = clean_for_speech(&piece) {
                self.emitted += 1;
                out.push(s);
            }
        }
        out
    }

    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        let s = clean_for_speech(&rest)?;
        self.emitted += 1;
        Some(s)
    }

    /// Text received so far that hasn't been emitted.
    pub fn pending(&self) -> &str {
        &self.buf
    }
}

fn find_cut(s: &str, min: usize) -> Option<usize> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    for (k, &(i, c)) in chars.iter().enumerate() {
        let next = chars.get(k + 1).map(|x| x.1);
        let end = i + c.len_utf8();
        if c == '\n' && end > 1 {
            return Some(end);
        }
        // A sentence end needs to be followed by whitespace (so "3.5" and "e.g." mid-word don't cut).
        if matches!(c, '.' | '!' | '?' | '…') && next.is_some_and(char::is_whitespace) && end >= min {
            let prev_word: String = s[..i].chars().rev().take_while(|c| c.is_alphanumeric()).collect();
            let abbrev = matches!(
                prev_word.chars().rev().collect::<String>().to_ascii_lowercase().as_str(),
                "mr" | "mrs" | "ms" | "dr" | "st" | "vs" | "etc" | "e.g" | "i.e"
            );
            if !abbrev {
                return Some(end);
            }
        }
        if matches!(c, ',' | ';' | ':' | '—') && next.is_some_and(char::is_whitespace) && end >= 110 {
            return Some(end);
        }
    }
    None
}

/// Drama 3's scoped tags (`<whisper>...</whisper>`); other angle-bracket tags are dropped.
pub const SCOPED_TAGS: [&str; 3] = ["whisper", "slow", "emphasis"];

/// Keep known scoped tags, drop any other `<...>` markup (and unbalanced leftovers).
fn keep_scoped_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        match after.find('>') {
            Some(j) => {
                let tag = after[1..j].trim().trim_start_matches('/').to_ascii_lowercase();
                if SCOPED_TAGS.contains(&tag.as_str()) {
                    out.push_str(&after[..=j]);
                } else {
                    out.push(' ');
                }
                rest = &after[j + 1..];
            }
            // A lone "<": keep the text after it.
            None => {
                out.push_str(&after[1..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip what a TTS should not read out (markdown, emoji, reasoning markers), keeping Drama 3's
/// own controls: `[point tags]`, `[style prefixes]` and the scoped tags. `None` if nothing
/// speakable is left.
pub fn clean_for_speech(s: &str) -> Option<String> {
    // Reasoning markers a model may emit even with thinking off.
    let s = s.replace("</think>", " ").replace("<think>", " ").replace("</think", " ").replace("<think", " ");
    let s = keep_scoped_tags(&s);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '*' | '#' | '_' | '`' | '~' => {}
            c if is_emoji(c) => {}
            '\n' | '\r' | '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    let mut t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // A leading speaker label the model sometimes writes ("Me: ...", "You: ...").
    for label in ["me:", "you:", "assistant:", "mimic:"] {
        if t.to_ascii_lowercase().starts_with(label) {
            t = t[label.len()..].trim_start().to_string();
        }
    }
    let t = t.replace(SILENT, "").replace("[SILENT]", "");
    let mut t = t.trim().trim_matches('"').trim();
    // A pause before the first or after the last word is only dead air (small models open
    // every line with one).
    loop {
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("[pause]") {
            t = t[7..].trim_start();
        } else if lower.ends_with("[pause]") {
            t = t[..t.len() - 7].trim_end();
        } else {
            break;
        }
    }
    if t.chars().any(|c| c.is_alphanumeric()) {
        Some(t.to_string())
    } else {
        None
    }
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x1F000..=0x1F2FF | 0xFE0F | 0x200D)
}

/// A sentence that only parrots something the model was given (their last line, the clock):
/// small models do this instead of saying `[silent]`.
pub fn is_echo(sentence: &str, their_last: &str, now: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let s = norm(sentence);
    !s.is_empty() && (s == norm(their_last) || (!now.is_empty() && s.contains(&norm(now))))
}

/// Their words end a thought (the transcriber punctuates sentence ends); without it they're
/// probably mid-sentence and an answer would interrupt.
pub fn ends_thought(text: &str) -> bool {
    text.trim_end().ends_with(['.', '?', '!', '…'])
}

/// The reply means "stay quiet" (possibly with whitespace/punctuation around it).
pub fn is_silent(s: &str) -> bool {
    let t = s.trim().trim_matches(|c: char| !c.is_alphanumeric() && c != '[' && c != ']').to_ascii_lowercase();
    t.is_empty() || t == SILENT || t == "silent" || t.starts_with(SILENT)
}

// ------------------------------------------------------------------ what was actually said

/// The spoken words of `text` without voice-direction tags (what a listener heard).
pub fn spoken_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth_sq = 0usize;
    let mut in_angle = false;
    for c in text.chars() {
        match c {
            '[' => depth_sq += 1,
            ']' if depth_sq > 0 => depth_sq -= 1,
            '<' => in_angle = true,
            '>' if in_angle => in_angle = false,
            c if depth_sq == 0 && !in_angle => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where each sentence of `words` ends, as a fraction (0..1] of its length (speech time is
/// roughly proportional to the words).
pub fn sentence_ends(words: &str) -> Vec<f64> {
    let total = words.chars().count().max(1) as f64;
    let chars: Vec<char> = words.chars().collect();
    let mut out = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        let next_space = chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if matches!(c, '.' | '!' | '?' | '…') && next_space {
            out.push((i + 1) as f64 / total);
        }
    }
    if out.last().is_none_or(|l| *l < 1.0) {
        out.push(1.0);
    }
    out
}

/// What had been said when playback stopped at `fraction` of the utterance: whole words only.
pub fn spoken_prefix(words: &str, fraction: f64) -> String {
    let chars: Vec<char> = words.chars().collect();
    let cut = ((chars.len() as f64) * fraction.clamp(0.0, 1.0)).round() as usize;
    if cut >= chars.len() {
        return words.to_string();
    }
    // Back to the last word boundary (a word half-said wasn't really said).
    let mut i = cut;
    while i > 0 && !chars[i].is_whitespace() {
        i -= 1;
    }
    chars[..i].iter().collect::<String>().trim_end_matches([',', ';', ':', ' ']).to_string()
}

/// The end of the sentence playing at `fraction` (finish it, then stop).
pub fn end_of_sentence(words: &str, fraction: f64) -> f64 {
    sentence_ends(words).into_iter().find(|e| *e >= fraction - 1e-9).unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spoken_parts() {
        let t = "[warmly] Sure, I can <emphasis>check</emphasis> that. Give me a second [laughs] okay?";
        let w = spoken_words(t);
        assert_eq!(w, "Sure, I can check that. Give me a second okay?");
        let ends = sentence_ends(&w);
        assert_eq!(ends.len(), 2);
        assert!((ends[0] - 23.0 / 46.0).abs() < 1e-9);
        assert_eq!(spoken_prefix(&w, 0.0), "");
        assert_eq!(spoken_prefix(&w, 0.3), "Sure, I can");
        assert_eq!(spoken_prefix(&w, 1.0), w);
        assert_eq!(spoken_prefix(&w, end_of_sentence(&w, 0.2)), "Sure, I can check that.");
        assert_eq!(end_of_sentence(&w, 0.9), 1.0);
        assert_eq!(sentence_ends("no end"), vec![1.0]);
    }

    #[test]
    fn sse_lines_across_chunks() {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        p.feed(b"data: {\"a\":1}\n\nda", &mut out);
        p.feed(b"ta: {\"a\":2}\r\n\r\n: keepalive\ndata: [DONE]\n", &mut out);
        assert_eq!(out, vec![SseEvent::Data("{\"a\":1}".into()), SseEvent::Data("{\"a\":2}".into()), SseEvent::Done]);
    }

    #[test]
    fn accumulates_content_and_tool_calls() {
        let mut a = ChatAccumulator::default();
        assert_eq!(a.apply(&json!({"choices":[{"delta":{"content":"Hel"}}]})), "Hel");
        a.apply(&json!({"choices":[{"delta":{"content":"lo"}}]}));
        a.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"note","arguments":"{\"te"}}]}}]}));
        a.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"xt\":\"hi\"}"}}]},"finish_reason":"tool_calls"}]}));
        assert_eq!(a.content, "Hello");
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0].name, "note");
        assert_eq!(a.tool_calls[0].args()["text"], "hi");
        assert_eq!(a.finish_reason.as_deref(), Some("tool_calls"));
        // Non-streamed shape.
        let mut b = ChatAccumulator::default();
        b.apply(&json!({"choices":[{"message":{"content":"","tool_calls":[{"id":"x","function":{"name":"snapshot","arguments":"{}"}}]}}]}));
        assert_eq!(b.tool_calls[0].name, "snapshot");
    }

    #[test]
    fn splits_sentences_early_and_cleanly() {
        let mut s = SentenceSplitter::new();
        let mut got = Vec::new();
        for tok in ["Sure", ".", " That works", " for me, Dr", ". Smith wants", " 3.5 hours", "! And then", " we go"] {
            got.extend(s.push(tok));
        }
        got.extend(s.finish());
        assert_eq!(got, vec!["Sure.", "That works for me, Dr. Smith wants 3.5 hours!", "And then we go"]);
    }

    #[test]
    fn long_clauses_cut_at_commas() {
        let mut s = SentenceSplitter::new();
        let long = "Well I was going to say that the thing about the release is that it really depends on the timing of the review, and then";
        let got = s.push(long);
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("review,"));
        assert_eq!(s.pending(), " and then");
    }

    #[test]
    fn cleaning_and_silence() {
        assert_eq!(clean_for_speech("**Me:** Sure thing 😊").as_deref(), Some("Sure thing"));
        assert_eq!(clean_for_speech("[laughs] okay").as_deref(), Some("[laughs] okay"));
        assert_eq!(clean_for_speech(" ... "), None);
        assert_eq!(clean_for_speech("is Wednesday. </think>I'll check").as_deref(), Some("is Wednesday. I'll check"));
        assert_eq!(clean_for_speech("is Wednesday. </thinkI'll check").as_deref(), Some("is Wednesday. I'll check"));
        assert_eq!(clean_for_speech("5 < 6 so").as_deref(), Some("5 6 so"));
        assert_eq!(clean_for_speech("[pause] Not yet, [pause] I gotta check. [PAUSE]").as_deref(), Some("Not yet, [pause] I gotta check."));
        assert_eq!(clean_for_speech("[pause]"), None);
        assert_eq!(clean_for_speech("Promise <whisper>you won't tell</whisper>, <b>ok</b>").as_deref(), Some("Promise <whisper>you won't tell</whisper>, ok"));
        assert_eq!(
            clean_for_speech("[with bright excitement] We won! <emphasis>Again</emphasis>.").as_deref(),
            Some("[with bright excitement] We won! <emphasis>Again</emphasis>.")
        );
        assert!(is_silent("[silent]"));
        assert!(is_silent("  [SILENT]. "));
        assert!(is_silent(""));
        assert!(!is_silent("Sure."));
        assert!(is_echo("Perfect.", "perfect", "now"));
        assert!(is_echo("Wednesday 4 November 2026, 21:00", "Great.", "Wednesday 4 November 2026, 21:00"));
        assert!(!is_echo("Perfect, see you then.", "Perfect.", "x"));
        assert!(ends_thought("Are you free?") && !ends_thought("so I was thinking that we could"));
    }
}
