//! Whisper's log-mel spectrogram (80 bins, 25 ms window, 10 ms hop at 16 kHz), as computed by
//! `transformers.WhisperFeatureExtractor`: the input format of the Smart Turn model.

use realfft::RealFftPlanner;

pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
pub const N_MELS: usize = 80;
const SR: f32 = 16_000.0;

fn hz_to_mel(f: f32) -> f32 {
    // Slaney: linear below 1 kHz, logarithmic above.
    if f < 1000.0 {
        3.0 * f / 200.0
    } else {
        15.0 + (f / 1000.0).ln() * 27.0 / 6.4f32.ln()
    }
}

fn mel_to_hz(m: f32) -> f32 {
    if m < 15.0 {
        200.0 * m / 3.0
    } else {
        1000.0 * ((m - 15.0) * 6.4f32.ln() / 27.0).exp()
    }
}

/// Slaney-normalized triangular filters, [N_MELS][N_FFT / 2 + 1].
fn filters() -> Vec<Vec<f32>> {
    let bins = N_FFT / 2 + 1;
    let fft_freqs: Vec<f32> = (0..bins).map(|i| i as f32 * (SR / 2.0) / (bins - 1) as f32).collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(SR / 2.0));
    let pts: Vec<f32> = (0..N_MELS + 2).map(|i| mel_to_hz(lo + (hi - lo) * i as f32 / (N_MELS + 1) as f32)).collect();
    (0..N_MELS)
        .map(|m| {
            let (l, c, r) = (pts[m], pts[m + 1], pts[m + 2]);
            let norm = 2.0 / (r - l);
            fft_freqs.iter().map(|&f| ((f - l) / (c - l)).min((r - f) / (r - c)).max(0.0) * norm).collect()
        })
        .collect()
}

pub struct LogMel {
    filters: Vec<Vec<f32>>,
    window: Vec<f32>,
    fft: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
}

impl Default for LogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl LogMel {
    pub fn new() -> Self {
        // Periodic Hann window.
        let window = (0..N_FFT).map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / N_FFT as f32).cos()).collect();
        Self { filters: filters(), window, fft: RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT) }
    }

    /// Features for exactly `seconds` of audio (the last `seconds`, zero-padded at the end if
    /// shorter), normalized to zero mean and unit variance first like Whisper's extractor.
    /// Returns [N_MELS][frames] flattened row-major, frames = seconds * 100.
    pub fn features(&self, audio: &[f32], seconds: usize) -> Vec<f32> {
        let len = seconds * SR as usize;
        let tail = &audio[audio.len().saturating_sub(len)..];
        let n = tail.len().max(1) as f32;
        let mean = tail.iter().sum::<f32>() / n;
        let var = tail.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n;
        let sd = (var + 1e-7).sqrt();
        let mut x: Vec<f32> = tail.iter().map(|v| (v - mean) / sd).collect();
        x.resize(len, 0.0);
        // Centered frames: reflect-pad by N_FFT / 2 on both sides.
        let pad = N_FFT / 2;
        let mut p = Vec::with_capacity(len + 2 * pad);
        p.extend((1..=pad).rev().map(|i| x[i.min(len - 1)]));
        p.extend_from_slice(&x);
        p.extend((0..pad).map(|i| x[len - 2 - i.min(len - 2)]));
        let frames = len / HOP; // Whisper drops the last of the 1 + len / HOP frames.
        let bins = N_FFT / 2 + 1;
        let mut input = self.fft.make_input_vec();
        let mut spec = self.fft.make_output_vec();
        let mut out = vec![0f32; N_MELS * frames];
        let mut power = vec![0f32; bins];
        for t in 0..frames {
            let s = t * HOP;
            for (k, v) in input.iter_mut().enumerate() {
                *v = p[s + k] * self.window[k];
            }
            self.fft.process(&mut input, &mut spec).expect("fft");
            for (pw, c) in power.iter_mut().zip(&spec) {
                *pw = c.re * c.re + c.im * c.im;
            }
            for (m, f) in self.filters.iter().enumerate() {
                let e: f32 = f.iter().zip(&power).map(|(a, b)| a * b).sum();
                out[m * frames + t] = e.max(1e-10).log10();
            }
        }
        let max = out.iter().cloned().fold(f32::MIN, f32::max);
        for v in out.iter_mut() {
            *v = (v.max(max - 8.0) + 4.0) / 4.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_range() {
        let m = LogMel::new();
        let tone: Vec<f32> = (0..32_000).map(|i| (i as f32 * 0.2).sin() * 0.3).collect();
        let f = m.features(&tone, 8);
        assert_eq!(f.len(), N_MELS * 800);
        assert!(f.iter().all(|v| v.is_finite()));
        // Whisper's scaling keeps values roughly in [-1, 2].
        assert!(f.iter().cloned().fold(f32::MIN, f32::max) <= 2.5);
    }

    /// Matches `transformers.WhisperFeatureExtractor` when reference files are given
    /// (MIMIC_MEL_REF=dir with ref_16k.wav (float) and ref_mel.npy).
    #[test]
    fn matches_whisper_reference() {
        let Ok(dir) = std::env::var("MIMIC_MEL_REF") else { return };
        let dir = std::path::PathBuf::from(dir);
        let (audio, rate) = crate::dsp::read_wav_native(&dir.join("ref_16k.wav")).unwrap();
        assert_eq!(rate, 16_000);
        let npy = std::fs::read(dir.join("ref_mel.npy")).unwrap();
        let header_len = u16::from_le_bytes([npy[8], npy[9]]) as usize;
        let data = &npy[10 + header_len..];
        let reference: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let ours = LogMel::new().features(&audio, 8);
        assert_eq!(ours.len(), reference.len());
        let max_err = ours.iter().zip(&reference).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(max_err < 2e-3, "max abs error {max_err}");
    }
}
