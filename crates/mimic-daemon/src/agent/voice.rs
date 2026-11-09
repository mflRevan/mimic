//! Your voice, for cloning: the last good phrases you said (native sample rate, with their
//! transcripts), kept in memory and replaced only by new speech, so the reference survives long
//! silences and follows your current mic and mood. Pinning freezes it and saves it to disk
//! (`<data dir>/voice/`) so it survives restarts; otherwise nothing is written.

use std::path::PathBuf;

use anyhow::{bail, Result};
use mimic_core::agent::voice::{self, ClipInfo};

struct Clip {
    pcm: Vec<f32>,
    rate: u32,
    text: String,
    info: ClipInfo,
}

#[derive(Default)]
pub struct VoiceBank {
    clips: Vec<Clip>,
    pinned: Option<(Vec<f32>, u32, String)>,
    /// The reference changed since the TTS last got it.
    pub dirty: bool,
}

fn dir() -> PathBuf {
    mimic_core::paths::data_dir().join("voice")
}

impl VoiceBank {
    /// Restores a pinned reference from disk when `pinned` is set in the config.
    pub fn new(pinned: bool) -> Self {
        let mut me = Self::default();
        if pinned {
            if let Ok(pcm) = crate::dsp::read_wav_native(&dir().join("reference.wav")) {
                let text = std::fs::read_to_string(dir().join("reference.txt")).unwrap_or_default();
                if !text.trim().is_empty() {
                    me.pinned = Some((pcm.0, pcm.1, text.trim().to_string()));
                    me.dirty = true;
                }
            }
        }
        me
    }

    /// Offer a phrase you said. Returns whether it was kept.
    pub fn offer(&mut self, pcm: Vec<f32>, rate: u32, text: &str) -> bool {
        let secs = pcm.len() as f32 / rate.max(1) as f32;
        let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
        let peak = pcm.iter().fold(0f32, |m, x| m.max(x.abs()));
        let info = ClipInfo { secs, words: text.split_whitespace().count(), rms, peak };
        if !voice::usable(&info) {
            return false;
        }
        self.clips.push(Clip { pcm, rate, text: text.trim().to_string(), info });
        // Drop clips beyond what could ever be selected, and clips from another sample rate.
        let rate_now = rate;
        self.clips.retain(|c| c.rate == rate_now);
        let mut total = 0.0;
        let keep = voice::keep_s(30.0);
        let mut cut = 0;
        for (i, c) in self.clips.iter().enumerate().rev() {
            total += c.info.secs;
            if total > keep {
                cut = i + 1;
                break;
            }
        }
        self.clips.drain(..cut);
        if self.pinned.is_none() {
            self.dirty = true;
        }
        true
    }

    /// The reference to clone from: (mono pcm, rate, transcript).
    pub fn reference(&self, target_s: f32) -> Option<(Vec<f32>, u32, String)> {
        if let Some(p) = &self.pinned {
            return Some(p.clone());
        }
        let infos: Vec<ClipInfo> = self.clips.iter().map(|c| c.info.clone()).collect();
        let pick = voice::select(&infos, target_s);
        if pick.is_empty() {
            return None;
        }
        let rate = self.clips[pick[0]].rate;
        let gap = vec![0.0f32; rate as usize / 4];
        let mut pcm = Vec::new();
        let mut text = Vec::new();
        for (k, &i) in pick.iter().enumerate() {
            if k > 0 {
                pcm.extend_from_slice(&gap);
            }
            pcm.extend_from_slice(&self.clips[i].pcm);
            text.push(self.clips[i].text.clone());
        }
        Some((pcm, rate, text.join(" ")))
    }

    pub fn seconds(&self, target_s: f32) -> f32 {
        self.reference(target_s).map(|(p, r, _)| p.len() as f32 / r as f32).unwrap_or(0.0)
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    /// Freeze the current reference and save it to disk.
    pub fn pin(&mut self, target_s: f32) -> Result<()> {
        let Some((pcm, rate, text)) = self.reference(target_s) else { bail!("no voice recorded yet: say a few sentences first") };
        std::fs::create_dir_all(dir())?;
        mimic_core::paths::write_atomic(&dir().join("reference.wav"), &crate::dsp::wav_bytes_at(&pcm, rate))?;
        mimic_core::paths::write_atomic(&dir().join("reference.txt"), text.as_bytes())?;
        self.pinned = Some((pcm, rate, text));
        Ok(())
    }

    /// Use a recording as the (pinned) reference.
    pub fn import(&mut self, path: &std::path::Path, text: &str) -> Result<()> {
        let (pcm, rate) = crate::dsp::read_wav_native(path)?;
        if text.trim().is_empty() {
            bail!("give the exact words said in the recording");
        }
        self.pinned = Some((pcm, rate, text.trim().to_string()));
        self.dirty = true;
        self.pin(0.0)
    }

    pub fn unpin(&mut self) {
        if self.pinned.take().is_some() {
            let _ = std::fs::remove_file(dir().join("reference.wav"));
            let _ = std::fs::remove_file(dir().join("reference.txt"));
            self.dirty = true;
        }
    }

    /// Forget everything (memory and the pinned copy on disk).
    pub fn clear(&mut self) {
        self.unpin();
        self.clips.clear();
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech(secs: f32, rate: u32) -> Vec<f32> {
        (0..(secs * rate as f32) as usize).map(|i| (i as f32 * 0.05).sin() * 0.2).collect()
    }

    #[test]
    fn collects_and_builds_reference() {
        let mut b = VoiceBank::default();
        assert!(b.reference(8.0).is_none());
        assert!(!b.offer(speech(0.5, 48_000), 48_000, "too short"));
        assert!(b.offer(speech(4.0, 48_000), 48_000, "one two three four"));
        assert!(b.offer(speech(5.0, 48_000), 48_000, "five six seven eight"));
        assert!(b.dirty);
        let (pcm, rate, text) = b.reference(8.0).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(text, "one two three four five six seven eight");
        assert!((pcm.len() as f32 / 48_000.0 - 9.25).abs() < 0.01);
        // Lots of speech: memory stays bounded.
        for _ in 0..100 {
            b.offer(speech(5.0, 48_000), 48_000, "and so on and so forth");
        }
        assert!(b.clips.len() <= 19);
    }
}
