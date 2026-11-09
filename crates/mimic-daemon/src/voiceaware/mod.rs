//! Small local models that make the agent conversation-aware, all on the CPU via ONNX Runtime:
//!
//! * [`Vad`]: Silero VAD (2 MB): is someone speaking, per 32 ms, robust to noise and music.
//! * [`TurnDetector`]: Smart Turn v3.2 (8 MB, Whisper-tiny encoder): has the speaker finished
//!   their turn, judged from the audio (intonation, pacing), not from a fixed pause.
//! * [`Embedder`]: ReDimNet2-B3 (19 MB): a 192-dim speaker embedding, to tell voices apart.
//!
//! Each loads on demand (downloaded once into `<data dir>/models/voiceaware`); a model that
//! is missing or fails leaves the agent working without it.

pub mod mel;

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
}

pub const SILERO: ModelFile = ModelFile { name: "silero-vad.onnx", url: "https://huggingface.co/onnx-community/silero-vad/resolve/main/onnx/model.onnx" };
pub const SMART_TURN: ModelFile =
    ModelFile { name: "smart-turn-v3.2-cpu.onnx", url: "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/main/smart-turn-v3.2-cpu.onnx" };
pub const REDIMNET: ModelFile =
    ModelFile { name: "redimnet2_b3_lm.onnx", url: "https://huggingface.co/diarizeapp/redimnet2-b3-onnx/resolve/main/redimnet2_b3_lm.onnx" };
pub const ALL: [&ModelFile; 3] = [&SILERO, &SMART_TURN, &REDIMNET];

pub fn dir() -> PathBuf {
    mimic_core::paths::models_dir().join("voiceaware")
}

pub fn path(m: &ModelFile) -> PathBuf {
    dir().join(m.name)
}

/// Download whatever is missing (small: ~30 MB together), plus ONNX Runtime if needed.
pub fn ensure(progress: crate::provision::Progress) -> Result<()> {
    crate::provision::ensure_onnxruntime(progress)?;
    std::fs::create_dir_all(dir())?;
    for (i, m) in ALL.iter().enumerate() {
        let dest = path(m);
        if !dest.exists() {
            let lo = i as f32 / ALL.len() as f32;
            crate::provision::download(m.url, &dest, "Downloading voice models", lo, lo + 1.0 / ALL.len() as f32, progress)?;
        }
    }
    Ok(())
}

fn session(m: &ModelFile, threads: usize) -> Result<Session> {
    crate::engine::parakeet::init_ort()?;
    let p = path(m);
    if !p.exists() {
        anyhow::bail!("{} is missing", p.display());
    }
    let r: ort::Result<Session> =
        (|| Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3)?.with_intra_threads(threads)?.commit_from_file(&p))();
    r.map_err(|e| anyhow!("loading {}: {e}", m.name))
}

// ------------------------------------------------------------------ Silero VAD

/// Streaming Silero VAD v5 at 16 kHz: 512-sample frames with 64 samples of context and a
/// recurrent state.
pub struct Vad {
    s: Session,
    state: Vec<f32>,
    context: Vec<f32>,
    pending: Vec<f32>,
}

pub const VAD_FRAME: usize = 512;
const VAD_CONTEXT: usize = 64;

impl Vad {
    pub fn load() -> Result<Self> {
        Ok(Self { s: session(&SILERO, 1)?, state: vec![0.0; 2 * 128], context: vec![0.0; VAD_CONTEXT], pending: Vec::new() })
    }

    /// Feed 16 kHz audio; returns the speech probability of each completed 32 ms frame.
    pub fn feed(&mut self, audio: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.pending.extend_from_slice(audio);
        while self.pending.len() >= VAD_FRAME {
            let frame: Vec<f32> = self.pending.drain(..VAD_FRAME).collect();
            let mut x = Vec::with_capacity(VAD_CONTEXT + VAD_FRAME);
            x.extend_from_slice(&self.context);
            x.extend_from_slice(&frame);
            self.context.copy_from_slice(&frame[VAD_FRAME - VAD_CONTEXT..]);
            let input = Tensor::from_array(([1usize, x.len()], x))?;
            let state = Tensor::from_array(([2usize, 1, 128], self.state.clone()))?;
            let sr = Tensor::from_array(((), vec![16_000i64]))?;
            let o = self.s.run(ort::inputs!["input" => input, "state" => state, "sr" => sr])?;
            let (_, p) = o["output"].try_extract_tensor::<f32>()?;
            let (_, st) = o["stateN"].try_extract_tensor::<f32>()?;
            out.push(p[0]);
            self.state.copy_from_slice(&st[..256]);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ Smart Turn

/// Has the speaker finished their turn? Looks at the last 8 s of their audio.
pub struct TurnDetector {
    s: Session,
    mel: mel::LogMel,
}

impl TurnDetector {
    pub fn load() -> Result<Self> {
        Ok(Self { s: session(&SMART_TURN, 2)?, mel: mel::LogMel::new() })
    }

    /// Probability (0..1) that the turn is complete, for 16 kHz audio ending now.
    pub fn complete(&mut self, audio: &[f32]) -> Result<f32> {
        let feats = self.mel.features(audio, 8);
        let x = Tensor::from_array(([1usize, mel::N_MELS, feats.len() / mel::N_MELS], feats))?;
        let o = self.s.run(ort::inputs!["input_features" => x])?;
        let (_, p) = o["logits"].try_extract_tensor::<f32>().context("smart turn output")?;
        Ok(p[0])
    }
}

// ------------------------------------------------------------------ speaker embeddings

/// ReDimNet2-B3 speaker embeddings (192 dims, L2-normalized), from 16 kHz audio.
pub struct Embedder {
    s: Session,
}

impl Embedder {
    pub fn load() -> Result<Self> {
        Ok(Self { s: session(&REDIMNET, 2)? })
    }

    pub fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        // Long phrases: the last 10 s are plenty and keep the cost bounded (~40 ms per 4 s).
        let a = &audio[audio.len().saturating_sub(160_000)..];
        let x = Tensor::from_array(([1usize, a.len()], a.to_vec()))?;
        let o = self.s.run(ort::inputs!["waveform" => x])?;
        let (_, e) = o["embedding"].try_extract_tensor::<f32>()?;
        let n = e.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        Ok(e.iter().map(|v| v / n).collect())
    }
}

/// Developer check: `mimic-daemon --voiceaware-check <dir>` runs the three models on every WAV
/// in `dir` (end-of-turn probability, speech share, and voice assignment with the speaker book).
pub fn check(dir: &std::path::Path) -> Result<()> {
    let mut turn = TurnDetector::load()?;
    let mut emb = Embedder::load()?;
    let mut book = mimic_core::agent::speakers::SpeakerBook::default();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "wav")).collect();
    files.sort();
    for f in files {
        let audio = crate::dsp::read_wav(&f)?;
        let t0 = std::time::Instant::now();
        let p = turn.complete(&audio)?;
        let t_turn = t0.elapsed().as_millis();
        let mut vad = Vad::load()?;
        let mut probs = Vec::new();
        vad.feed(&audio, &mut probs)?;
        let speech = probs.iter().filter(|p| **p >= 0.5).count() as f32 / probs.len().max(1) as f32;
        let t1 = std::time::Instant::now();
        let e = emb.embed(&audio)?;
        let t_emb = t1.elapsed().as_millis();
        let who = book.assign(&e, audio.len() as f32 / 16_000.0, 0);
        println!("{:<16} turn {p:.3} ({t_turn} ms)  speech {:>3.0}%  voice {who:?} ({t_emb} ms)", f.file_name().unwrap().to_string_lossy(), speech * 100.0);
    }
    Ok(())
}
