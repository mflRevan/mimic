//! Wake word on transcripts: "Mimic, note that the meeting moved" -> "note that the meeting moved".
//!
//! Matching is on the speech-to-text output, so it is tolerant: case, punctuation, a leading
//! "hey"/"ok", near-misses one edit away ("mimik", "mimics") and the word split in two ("mi mic").

/// If `text` starts with `wakeword`, the rest of it (may be empty: "Mimic?" alone).
pub fn strip(text: &str, wakeword: &str) -> Option<String> {
    let wake = norm(wakeword);
    if wake.is_empty() {
        return None;
    }
    let wake_joined: String = wake.join("");
    // Token spans over the original text so the remainder keeps its casing/punctuation.
    let toks = tokens(text);
    let mut i = 0;
    while i < toks.len() && i < 2 && FILLERS.contains(&toks[i].1.as_str()) {
        i += 1;
    }
    // Try the wake word as 1..=3 consecutive tokens joined.
    for n in 1..=3usize {
        if i + n > toks.len() {
            break;
        }
        let joined: String = toks[i..i + n].iter().map(|t| t.1.as_str()).collect();
        // Several tokens only count as the wake word split apart ("mi mic"), never a word before it.
        let split_ok = n == 1 || (toks[i].1.len() >= 2 && wake_joined.starts_with(&toks[i].1));
        if split_ok && close(&joined, &wake_joined) {
            let end = toks[i + n - 1].0 .1;
            let rest = text[end..].trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'');
            return Some(rest.trim().to_string());
        }
    }
    None
}

const FILLERS: [&str; 6] = ["hey", "ok", "okay", "yo", "hi", "um"];

fn norm(s: &str) -> Vec<String> {
    tokens(s).into_iter().map(|t| t.1).collect()
}

/// (byte span, lowercased alphanumeric word)
fn tokens(s: &str) -> Vec<((usize, usize), String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || c == '\'' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(st) = start.take() {
            out.push(((st, i), s[st..i].to_lowercase().replace('\'', "")));
        }
    }
    if let Some(st) = start {
        out.push(((st, s.len()), s[st..].to_lowercase().replace('\'', "")));
    }
    out
}

fn close(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let tol = if b.chars().count() >= 4 { 1 } else { 0 };
    // A plural/possessive of the wake word still counts ("mimics", "mimic's").
    if a.strip_suffix('s') == Some(b) {
        return true;
    }
    edit_distance(a, b) <= tol
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_variants() {
        assert_eq!(strip("Mimic, note that the meeting moved.", "mimic").as_deref(), Some("note that the meeting moved."));
        assert_eq!(strip("hey mimik take a snapshot", "mimic").as_deref(), Some("take a snapshot"));
        assert_eq!(strip("Mi mic: stop", "mimic").as_deref(), Some("stop"));
        assert_eq!(strip("Mimic's going to answer", "mimic").as_deref(), Some("going to answer"));
        assert_eq!(strip("Mimic?", "mimic").as_deref(), Some(""));
        assert_eq!(strip("Hey Jarvis, lights", "Jarvis").as_deref(), Some("lights"));
    }

    #[test]
    fn ignores_non_matches() {
        assert_eq!(strip("I mimic him all the time", "mimic"), None);
        assert_eq!(strip("Magic, right?", "mimic"), None);
        assert_eq!(strip("anything", ""), None);
        assert_eq!(strip("", "mimic"), None);
    }
}
