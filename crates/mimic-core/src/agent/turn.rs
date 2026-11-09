//! Deterministic turn-taking: whether what the others just said calls for an answer, decided
//! before (and instead of) asking a model. Small models are unreliable at staying quiet after
//! "great" or at answering a direct question; these cases don't need a model at all.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// A question: answer (the model may not choose silence).
    Answer,
    /// A statement: let the model decide whether to say something.
    Maybe,
    /// Probably mid-sentence: wait longer, then let the model decide.
    Wait,
    /// An acknowledgement or filler: say nothing, no model call.
    Skip,
}

const ACKS: [&str; 34] = [
    "great",
    "perfect",
    "okay",
    "ok",
    "cool",
    "nice",
    "alright",
    "all right",
    "right",
    "sure",
    "yeah",
    "yes",
    "yep",
    "yup",
    "mhm",
    "mm-hmm",
    "mm",
    "uh-huh",
    "thanks",
    "thank you",
    "got it",
    "sounds good",
    "awesome",
    "good",
    "fine",
    "true",
    "exactly",
    "totally",
    "for sure",
    "i see",
    "makes sense",
    "fair",
    "fair enough",
    "hmm",
];

fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn decide(text: &str) -> Turn {
    let t = text.trim();
    if t.is_empty() {
        return Turn::Skip;
    }
    if t.ends_with('?') {
        return Turn::Answer;
    }
    // "Great." / "Okay, cool." / "Thanks, perfect!": only acknowledgement words.
    let n = norm(t);
    let mut rest = n.as_str();
    let mut any = false;
    'outer: while !rest.is_empty() {
        for a in ACKS.iter().filter(|a| a.contains(' ')).chain(ACKS.iter().filter(|a| !a.contains(' '))) {
            if rest == *a || rest.starts_with(&format!("{a} ")) {
                rest = rest[a.len()..].trim_start();
                any = true;
                continue 'outer;
            }
        }
        break;
    }
    if any && rest.is_empty() {
        return Turn::Skip;
    }
    if !super::stream::ends_thought(t) {
        return Turn::Wait;
    }
    Turn::Maybe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        assert_eq!(decide("How did the release go overall?"), Turn::Answer);
        assert_eq!(decide("Great."), Turn::Skip);
        assert_eq!(decide("Okay, cool, thanks!"), Turn::Skip);
        assert_eq!(decide("Sounds good."), Turn::Skip);
        assert_eq!(decide("Great, so let's move on to the budget."), Turn::Maybe);
        assert_eq!(decide("so I was also thinking that maybe we could"), Turn::Wait);
        assert_eq!(decide("I sent you the file."), Turn::Maybe);
        assert_eq!(decide(""), Turn::Skip);
    }
}
