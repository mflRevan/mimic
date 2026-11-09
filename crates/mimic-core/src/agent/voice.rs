//! Which of your recent phrases make a good voice-cloning reference.
//!
//! The daemon keeps the audio; this decides what is worth keeping and which clips form the
//! reference (newest first, until enough seconds), so the clone follows your current voice,
//! mic and mood, and stays put while you're silent.

#[derive(Debug, Clone, PartialEq)]
pub struct ClipInfo {
    pub secs: f32,
    pub words: usize,
    pub rms: f32,
    pub peak: f32,
}

/// Is a phrase usable as reference audio?
pub fn usable(c: &ClipInfo) -> bool {
    c.secs >= 1.2 && c.secs <= 20.0 && c.words >= 3 && c.peak < 0.995 && c.rms > 0.006
}

/// Pick clips (indices into `clips`, oldest first) totalling about `target_s`.
/// Clips are newest last. Never more than `target_s * 1.25` seconds; always at least one
/// clip if any is usable. (Longer references slow the voice model's start.)
pub fn select(clips: &[ClipInfo], target_s: f32) -> Vec<usize> {
    let mut picked = Vec::new();
    let mut total = 0.0;
    for (i, c) in clips.iter().enumerate().rev() {
        if !usable(c) {
            continue;
        }
        if !picked.is_empty() && total + c.secs > target_s * 1.25 {
            continue;
        }
        picked.push(i);
        total += c.secs;
        if total >= target_s {
            break;
        }
    }
    picked.reverse();
    picked
}

/// How many seconds of clips are worth keeping at all (older ones can be dropped).
pub fn keep_s(target_s: f32) -> f32 {
    (target_s * 3.0).max(30.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(secs: f32) -> ClipInfo {
        ClipInfo { secs, words: (secs * 2.5) as usize, rms: 0.05, peak: 0.6 }
    }

    #[test]
    fn newest_first_until_target() {
        let clips = vec![clip(5.0), clip(4.0), clip(3.0), clip(6.0)];
        assert_eq!(select(&clips, 8.0), vec![2, 3]);
        assert_eq!(select(&clips, 12.0), vec![1, 2, 3]);
    }

    #[test]
    fn skips_bad_clips() {
        let mut clipped = clip(5.0);
        clipped.peak = 1.0;
        let short = clip(0.5);
        let quiet = ClipInfo { rms: 0.001, ..clip(4.0) };
        let clips = vec![clip(3.0), clipped, short, quiet];
        assert_eq!(select(&clips, 8.0), vec![0]);
        // A single long clip is still taken when it's all there is.
        assert_eq!(select(&[clip(18.0)], 8.0), vec![0]);
        assert!(select(&[], 8.0).is_empty());
    }
}
