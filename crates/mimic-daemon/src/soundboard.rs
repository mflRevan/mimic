//! Soundboard: nine slots of sound effects on hotkeys (Alt+1..9 by default), played into the
//! virtual mic (so the call hears them) and on your own output. Pressing a slot again stops it.
//! Files are decoded once (WAV, MP3, OGG/Vorbis, FLAC) and kept in memory as mono at the
//! router's effect rate; a changed file is decoded again on its next use.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use mimic_core::config::Soundboard as SoundboardCfg;

use crate::dsp::RateConv;
use crate::router::{Router, FX_RATE};

/// Longest effect accepted (keeps memory bounded: 60 s mono f32 at 48 kHz is ~11 MB).
const MAX_SECONDS: usize = 60;

pub struct Soundboard {
    router: Router,
    cache: HashMap<PathBuf, (SystemTime, Arc<Vec<f32>>)>,
}

impl Soundboard {
    pub fn new(router: Router) -> Self {
        Self { router, cache: HashMap::new() }
    }

    /// Play slot `idx` (0-based), or stop it if it is playing. Returns a short message for the overlay.
    pub fn toggle(&mut self, cfg: &SoundboardCfg, idx: usize) -> Result<Option<String>> {
        if !cfg.enabled {
            return Ok(None);
        }
        let slot = cfg.slots.get(idx).ok_or_else(|| anyhow!("there is no sound slot {}", idx + 1))?;
        if slot.file.trim().is_empty() {
            bail!("sound slot {} is empty: pick a file in Mimic", idx + 1);
        }
        let clip = self.clip(Path::new(slot.file.trim()))?;
        let gain = (cfg.volume * slot.volume).clamp(0.0, 2.0);
        let started = self.router.toggle_fx(idx, clip, gain, cfg.monitor);
        Ok(started.then(|| if slot.label.trim().is_empty() { format!("Sound {}", idx + 1) } else { slot.label.trim().to_string() }))
    }

    pub fn stop(&self) {
        self.router.stop_fx();
    }

    /// Slots playing right now (1-based).
    pub fn playing(&self) -> Vec<usize> {
        self.router.fx_playing().into_iter().map(|i| i + 1).collect()
    }

    /// Decode (or reuse) a file.
    fn clip(&mut self, path: &Path) -> Result<Arc<Vec<f32>>> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).with_context(|| format!("{} not found", path.display()))?;
        if let Some((t, c)) = self.cache.get(path) {
            if *t == modified {
                return Ok(c.clone());
            }
        }
        let (pcm, rate) = decode(path)?;
        let mut out = Vec::with_capacity(pcm.len() * FX_RATE as usize / rate.max(1) as usize + 16);
        RateConv::new(rate, FX_RATE).process(&pcm, &mut out);
        out.truncate(FX_RATE as usize * MAX_SECONDS);
        let clip = Arc::new(out);
        self.cache.insert(path.to_path_buf(), (modified, clip.clone()));
        Ok(clip)
    }
}

pub fn sounds_dir() -> PathBuf {
    mimic_core::paths::data_dir().join("sounds")
}

/// Save an imported sound (base64) as `<data dir>/sounds/<slot>-<name>`, checking it decodes.
pub fn import(slot: usize, name: &str, data_b64: &str) -> Result<PathBuf> {
    use base64::Engine as _;
    if !(1..=mimic_core::config::SOUND_SLOTS).contains(&slot) {
        bail!("slots are 1 to {}", mimic_core::config::SOUND_SLOTS);
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64.trim()).context("the file didn't arrive intact")?;
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' }).collect();
    let dir = sounds_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{slot}-{}", safe.trim()));
    std::fs::write(&path, &bytes)?;
    if let Err(e) = decode(&path) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// Delete a replaced sound if Mimic made the copy (never touches your own files).
pub fn forget(old: &str, new: &Path) {
    let old = Path::new(old);
    if !old.as_os_str().is_empty() && old != new && old.starts_with(sounds_dir()) {
        let _ = std::fs::remove_file(old);
    }
}

/// Decode an audio file to mono f32 at its own rate.
pub fn decode(path: &Path) -> Result<(Vec<f32>, u32)> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("{}: unsupported audio ({e})", path.display()))?;
    let mut format = probed.format;
    let track = format.default_track().ok_or_else(|| anyhow!("{}: no audio track", path.display()))?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(48_000);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let mut mono = Vec::new();
    let mut buf: Option<SampleBuffer<f32>> = None;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if b.capacity() < decoded.capacity() * ch {
            *b = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        b.copy_interleaved_ref(decoded);
        mono.extend(b.samples().chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        if mono.len() > rate as usize * MAX_SECONDS {
            break;
        }
    }
    if mono.is_empty() {
        bail!("{}: no audio decoded", path.display());
    }
    Ok((mono, rate))
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_wav() {
        let p = std::env::temp_dir().join(format!("mimic-sfx-{}.wav", std::process::id()));
        let pcm: Vec<f32> = (0..22_050).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        std::fs::write(&p, crate::dsp::wav_bytes_at(&pcm, 22_050)).unwrap();
        let (out, rate) = super::decode(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(rate, 22_050);
        assert!((out.len() as i64 - 22_050).abs() < 64);
        assert!((out[100] - pcm[100]).abs() < 0.01);
    }
}
