//! Who is speaking: a persistent book of voices (speaker embeddings), so lines from a call are
//! labelled "Speaker 2" — or "Ben" once you've said who that is — and stay labelled across
//! conversations.
//!
//! Each voice keeps up to [`MEMBERS`] of the (L2-normalized) embeddings it was confidently
//! heard with, and a new clip is scored against a voice by its *average* similarity to those
//! (average linkage). That is much more robust than one running centroid, which drifts towards
//! whoever it last absorbed and then swallows the next similar voice. A clip goes to the best
//! voice when it is clearly close enough (and refines it); it starts a new voice when it is
//! clearly far from all of them and long enough to trust; in between it is assigned without
//! refining anything. Thresholds were tuned with ReDimNet2-B3 on 16 kHz speech (same-speaker
//! clips average 0.70-0.75, different speakers at most ~0.5; synthetic voices from one model,
//! more alike than real people are): online over shuffled clips, 0.62/0.55 kept 94% purity and
//! merged the fewest distinct voices. Calls have 2-4 people on different mics, which are easier
//! to tell apart, so matching is a little more lenient (0.58/0.52): fewer voices split in two.

use serde::{Deserialize, Serialize};

/// Confident match: assign and refine the voice.
pub const MATCH: f32 = 0.58;
/// Below this against every voice: someone new (if the clip is long enough).
pub const NEW: f32 = 0.52;
/// The best voice must also beat the runner-up by this much to be confident.
pub const MARGIN: f32 = 0.04;
/// Shortest clip that may start a new voice or refine one (seconds).
pub const MIN_TRUST_S: f32 = 1.5;
/// Voices remembered (the least recently heard unnamed ones are dropped first).
pub const CAPACITY: usize = 64;
/// Embeddings kept per voice (the oldest are replaced).
pub const MEMBERS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Voice {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    /// The mean of the members (kept for display and for books saved before members existed).
    pub centroid: Vec<f32>,
    /// Recent confident embeddings of this voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Vec<f32>>,
    /// Seconds of speech heard as this voice.
    pub seconds: f32,
    /// Unix milliseconds.
    pub last_heard: u64,
}

impl Voice {
    pub fn label(&self) -> String {
        if self.name.trim().is_empty() {
            format!("Speaker {}", self.id)
        } else {
            self.name.trim().to_string()
        }
    }

    /// Average similarity of `e` to this voice's members.
    pub fn score(&self, e: &[f32]) -> f32 {
        if self.members.is_empty() {
            return cos(e, &self.centroid);
        }
        self.members.iter().map(|m| cos(e, m)).sum::<f32>() / self.members.len() as f32
    }

    fn add_member(&mut self, e: Vec<f32>) {
        if self.members.is_empty() && !self.centroid.is_empty() {
            self.members.push(self.centroid.clone());
        }
        self.members.push(e);
        if self.members.len() > MEMBERS {
            let extra = self.members.len() - MEMBERS;
            self.members.drain(..extra);
        }
        self.recenter();
    }

    fn recenter(&mut self) {
        let Some(first) = self.members.first() else { return };
        let mut sum = vec![0.0; first.len()];
        for m in &self.members {
            for (s, x) in sum.iter_mut().zip(m) {
                *s += x;
            }
        }
        self.centroid = normalize(&sum);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Assigned {
    /// Confident: the voice was refined with this clip.
    Known(u32),
    /// Probably this voice (not confident enough to refine it).
    Likely(u32),
    /// A new voice was started.
    New(u32),
    /// Too short and too unlike anyone to tell.
    Unknown,
}

impl Assigned {
    pub fn id(self) -> Option<u32> {
        match self {
            Assigned::Known(i) | Assigned::Likely(i) | Assigned::New(i) => Some(i),
            Assigned::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpeakerBook {
    pub voices: Vec<Voice>,
    next: u32,
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / n).collect()
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}

impl SpeakerBook {
    /// Assign an embedding of `secs` seconds of speech heard at `now`.
    pub fn assign(&mut self, embedding: &[f32], secs: f32, now: u64) -> Assigned {
        if embedding.is_empty() || embedding.iter().any(|x| !x.is_finite()) {
            return Assigned::Unknown;
        }
        let e = normalize(embedding);
        let mut scored: Vec<(usize, f32)> = self.voices.iter().enumerate().map(|(i, v)| (i, v.score(&e))).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let best = scored.first().copied();
        let runner_up = scored.get(1).map(|s| s.1).unwrap_or(-1.0);
        match best {
            Some((i, s)) if s >= MATCH && s - runner_up >= MARGIN => {
                let v = &mut self.voices[i];
                if secs >= MIN_TRUST_S {
                    v.add_member(e);
                }
                v.seconds += secs;
                v.last_heard = now;
                Assigned::Known(v.id)
            }
            Some((i, s)) if s >= NEW || secs < MIN_TRUST_S && s >= NEW - 0.1 => {
                self.voices[i].last_heard = now;
                self.voices[i].seconds += secs;
                Assigned::Likely(self.voices[i].id)
            }
            _ if secs >= MIN_TRUST_S => {
                self.next = self.next.max(self.voices.iter().map(|v| v.id).max().unwrap_or(0)) + 1;
                let id = self.next;
                self.voices.push(Voice { id, name: String::new(), centroid: e.clone(), members: vec![e], seconds: secs, last_heard: now });
                self.evict();
                Assigned::New(id)
            }
            _ => Assigned::Unknown,
        }
    }

    fn evict(&mut self) {
        while self.voices.len() > CAPACITY {
            // Drop the least recently heard unnamed voice (named ones only if all are named).
            let pick = self
                .voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.name.is_empty())
                .min_by_key(|(_, v)| v.last_heard)
                .or_else(|| self.voices.iter().enumerate().min_by_key(|(_, v)| v.last_heard))
                .map(|(i, _)| i);
            match pick {
                Some(i) => {
                    self.voices.remove(i);
                }
                None => break,
            }
        }
    }

    pub fn get(&self, id: u32) -> Option<&Voice> {
        self.voices.iter().find(|v| v.id == id)
    }

    pub fn label(&self, id: u32) -> String {
        self.get(id).map(Voice::label).unwrap_or_else(|| format!("Speaker {id}"))
    }

    /// Name a voice. Another voice already carrying that name gives it up (and if both are the
    /// same person, merge them).
    pub fn rename(&mut self, id: u32, name: &str) -> bool {
        let name = name.trim().to_string();
        if self.get(id).is_none() {
            return false;
        }
        if !name.is_empty() {
            let dup: Vec<u32> = self.voices.iter().filter(|v| v.id != id && v.name.eq_ignore_ascii_case(&name)).map(|v| v.id).collect();
            for d in dup {
                self.merge(d, id);
            }
        }
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
            v.name = name;
        }
        true
    }

    /// Fold voice `from` into `into` (the same person heard as two voices).
    pub fn merge(&mut self, from: u32, into: u32) -> bool {
        let (Some(a), Some(b)) = (self.get(from).cloned(), self.get(into).cloned()) else { return false };
        if from == into {
            return false;
        }
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == into) {
            let theirs = if a.members.is_empty() { vec![a.centroid.clone()] } else { a.members.clone() };
            let mine = if v.members.is_empty() { vec![b.centroid.clone()] } else { std::mem::take(&mut v.members) };
            // Interleave so both stay represented when trimmed to the newest MEMBERS.
            let mut all = Vec::new();
            let (mut x, mut y) = (mine.into_iter(), theirs.into_iter());
            loop {
                let (p, q) = (x.next(), y.next());
                if p.is_none() && q.is_none() {
                    break;
                }
                all.extend(p);
                all.extend(q);
            }
            let extra = all.len().saturating_sub(MEMBERS);
            v.members = all.split_off(extra);
            v.recenter();
            v.seconds += a.seconds;
            v.last_heard = v.last_heard.max(a.last_heard);
            if v.name.is_empty() {
                v.name = a.name.clone();
            }
        }
        self.voices.retain(|v| v.id != from);
        true
    }

    pub fn forget(&mut self, id: u32) -> bool {
        let n = self.voices.len();
        self.voices.retain(|v| v.id != id);
        n != self.voices.len()
    }

    /// The voice named `name` (case-insensitive).
    pub fn by_name(&self, name: &str) -> Option<u32> {
        self.voices.iter().find(|v| v.name.eq_ignore_ascii_case(name.trim())).map(|v| v.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector near axis `k` (8 dims), tilted by `t` towards axis `k + 1`.
    fn emb(k: usize, t: f32) -> Vec<f32> {
        let mut v = vec![0.0; 8];
        v[k] = 1.0;
        v[(k + 1) % 8] = t;
        v
    }

    #[test]
    fn assigns_creates_and_refines() {
        let mut b = SpeakerBook::default();
        assert_eq!(b.assign(&emb(0, 0.0), 0.8, 1), Assigned::Unknown); // too short to start anyone
        assert_eq!(b.assign(&emb(0, 0.0), 2.0, 2), Assigned::New(1));
        assert_eq!(b.assign(&emb(0, 0.2), 2.0, 3), Assigned::Known(1));
        assert_eq!(b.assign(&emb(3, 0.0), 2.5, 4), Assigned::New(2));
        assert_eq!(b.assign(&emb(3, 0.1), 0.8, 5), Assigned::Known(2)); // short but clearly them
        assert_eq!(b.voices.len(), 2);
        assert_eq!(b.label(2), "Speaker 2");
    }

    #[test]
    fn ambiguous_is_likely_not_refined() {
        let mut b = SpeakerBook::default();
        b.assign(&emb(0, 0.0), 2.0, 1);
        let before = b.voices[0].centroid.clone();
        // cos ~0.56 against voice 1: between NEW and MATCH.
        let r = b.assign(&emb(0, 1.48), 2.0, 2);
        assert_eq!(r, Assigned::Likely(1));
        assert_eq!(b.voices[0].centroid, before);
    }

    #[test]
    fn average_linkage_resists_drift() {
        // Voice 1 heard spread around axis 0; a different voice sits on axis 1.
        let mut b = SpeakerBook::default();
        b.assign(&emb(0, 0.0), 2.0, 1);
        b.assign(&emb(0, 0.3), 2.0, 2);
        b.assign(&emb(0, -0.3), 2.0, 3);
        assert_eq!(b.voices[0].members.len(), 3);
        assert_eq!(b.assign(&emb(1, 0.0), 2.0, 4), Assigned::New(2));
        // Books saved before members existed still score by their centroid.
        let mut old = b.voices[0].clone();
        old.members.clear();
        assert!(old.score(&normalize(&emb(0, 0.0))) > 0.9);
        // Merging keeps members of both.
        assert!(b.merge(2, 1));
        assert_eq!(b.voices[0].members.len(), 4);
    }

    #[test]
    fn naming_merging_and_persistence() {
        let mut b = SpeakerBook::default();
        b.assign(&emb(0, 0.0), 2.0, 1);
        b.assign(&emb(4, 0.0), 2.0, 2);
        assert!(b.rename(1, "Ben"));
        assert_eq!(b.label(1), "Ben");
        // Naming another voice "Ben" too: they are the same person.
        assert!(b.rename(2, "ben"));
        assert_eq!(b.voices.len(), 1);
        assert_eq!(b.by_name("BEN"), Some(2));
        let json = serde_json::to_string(&b).unwrap();
        let back: SpeakerBook = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
        // New ids never reuse an old one.
        let mut b = back;
        assert!(matches!(b.assign(&emb(6, 0.0), 2.0, 3), Assigned::New(3)));
        assert!(b.forget(3));
        assert!(!b.forget(3));
    }

    #[test]
    fn capacity_drops_old_unnamed_voices() {
        let mut b = SpeakerBook::default();
        for i in 0..(CAPACITY + 5) {
            let mut v = vec![0.0; CAPACITY + 8];
            v[i] = 1.0;
            b.assign(&v, 2.0, i as u64);
            if i == 0 {
                b.rename(1, "Kept");
            }
        }
        assert_eq!(b.voices.len(), CAPACITY);
        assert!(b.by_name("Kept").is_some());
    }
}
