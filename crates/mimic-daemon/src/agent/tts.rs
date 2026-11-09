//! Voice: Fish Audio Drama 3 (int4, via drama-rs) as a managed local server. Zero-shot cloning
//! from a short reference of your own recent speech; streamed audio (first sound in ~90 ms on
//! an RTX 5090, ~8x real time). Dropping a request's connection stops its generation, which is
//! how the agent gets interrupted mid-sentence.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use mimic_core::agent::TtsCfg;
use serde_json::json;

use super::server::{self, Server};

pub const MODEL_DIR: &str = "drama-3-q4";
/// Drama renders 44.1 kHz mono.
pub const RATE: u32 = 44_100;
pub const VOICE_NAME: &str = "me";
pub const NAME: &str = "Drama 3";

/// Where the experts live, for the status line ("int4, 24/40 layers in VRAM").
pub fn placement(cfg: &TtsCfg) -> String {
    let kind = if cfg.experts.trim().eq_ignore_ascii_case("fp8") { "FP8" } else { "int4" };
    match cfg.vram_layers {
        0 | 40.. => format!("{kind}, all in VRAM"),
        n => format!("{kind}, {n}/40 layers in VRAM"),
    }
}

pub fn exe() -> PathBuf {
    mimic_core::paths::runtime_dir().join("drama").join(if cfg!(windows) { "drama.exe" } else { "drama" })
}

pub fn model_dir(cfg: &TtsCfg) -> PathBuf {
    if cfg.model.trim().is_empty() {
        mimic_core::paths::models_dir().join(MODEL_DIR)
    } else {
        PathBuf::from(cfg.model.trim())
    }
}

pub fn installed(cfg: &TtsCfg) -> bool {
    exe().exists() && model_dir(cfg).join("drama.json").exists()
}

fn parse_layer(line: &str) -> Option<f32> {
    // "loading weights: layer 12/40"
    let i = line.rfind("layer ")?;
    let (a, b) = line[i + 6..].split_once('/')?;
    let n: f32 = a.trim().parse().ok()?;
    let d: f32 = b.trim().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some((n / d).clamp(0.0, 1.0))
}

pub struct Drama {
    server: Server,
    http: ureq::Agent,
    temperature: f32,
}

impl Drama {
    pub fn start(cfg: &TtsCfg, progress: &dyn Fn(&str, Option<f32>)) -> Result<Self> {
        let model = model_dir(cfg);
        if !model.join("drama.json").exists() {
            bail!("the voice model is not downloaded ({})", model.display());
        }
        let port = server::free_port()?;
        let fp8 = cfg.experts.trim().eq_ignore_ascii_case("fp8");
        let mut args: Vec<String> = vec![
            "serve".into(),
            "--model".into(),
            model.display().to_string(),
            "--experts".into(),
            if cfg.experts.trim().is_empty() { "q4".into() } else { cfg.experts.trim().to_string() },
            "--addr".into(),
            format!("127.0.0.1:{port}"),
        ];
        // Experts of layers past `vram_layers` are read from system RAM (int4: Mimic's drama-rs
        // patch via DRAMA_Q4_VRAM_LAYERS; FP8: drama-rs's own placement, needs the original model).
        let mut env = Vec::new();
        if cfg.vram_layers > 0 && cfg.vram_layers < 40 {
            if fp8 {
                args.push("--fp8-vram-layers".into());
                args.push(cfg.vram_layers.to_string());
            } else {
                env.push(("DRAMA_Q4_VRAM_LAYERS".to_string(), cfg.vram_layers.to_string()));
            }
        }
        if fp8 {
            args.push("--fp8-model".into());
            args.push(mimic_core::paths::models_dir().join("drama-3").display().to_string());
        }
        progress("Loading the voice model", Some(0.0));
        let server = Server::start(
            server::Spec {
                name: "drama",
                exe: &exe(),
                args,
                env,
                port,
                health: "/health",
                timeout: Duration::from_secs(300),
                low_priority: true,
                progress: Some(parse_layer),
            },
            progress,
        )?;
        let http: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).http_status_as_error(false).build().into();
        let me = Self { server, http, temperature: cfg.temperature };
        // The first request captures CUDA graphs (~10 s); do it now, not on the first reply.
        progress("Warming up the voice", Some(0.99));
        let _ = me.speak("Okay.", None, &mut |_| true);
        Ok(me)
    }

    /// Register (or replace) the clone reference: mono PCM at `rate` and exactly what it says.
    pub fn set_voice(&self, pcm: &[f32], rate: u32, text: &str) -> Result<()> {
        let wav = crate::dsp::wav_bytes_at(pcm, rate);
        let r = self
            .http
            .post(&format!("{}/v1/voices", self.server.base))
            .query("name", VOICE_NAME)
            .query("text", text)
            .header("Content-Type", "audio/wav")
            .send(&wav[..])?;
        let status = r.status();
        if status != 200 {
            let mut body = r.into_body();
            bail!("voice reference rejected ({status}): {}", body.read_to_string().unwrap_or_default().trim());
        }
        Ok(())
    }

    /// Speak `text`, streaming mono f32 at [`RATE`] into `sink` as it is generated. `sink`
    /// returns false to stop (the connection is dropped and the server stops generating).
    /// Returns false if stopped early.
    pub fn speak(&self, text: &str, voice: Option<&str>, sink: &mut dyn FnMut(&[f32]) -> bool) -> Result<bool> {
        // `split: false`: perform the text as one utterance. Split, every sentence starts from the
        // reference alone and tone and pacing jump between sentences. (Splitting exists to stop
        // runaway silence without a voice reference; there always is one here.)
        let mut body = json!({"text": text, "stream": true, "split": voice.is_none(), "temperature": self.temperature});
        if let Some(v) = voice {
            body["voice"] = json!(v);
        }
        let r =
            self.http.post(&format!("{}/v1/speak", self.server.base)).header("Content-Type", "application/json").send(serde_json::to_vec(&body)?.as_slice())?;
        let status = r.status();
        let mut reader = r.into_body().into_reader();
        if status != 200 {
            let mut s = String::new();
            let _ = reader.take(2000).read_to_string(&mut s);
            bail!("speech failed ({status}): {}", s.trim());
        }
        let mut header = 44usize;
        let mut carry: Option<u8> = None;
        let mut buf = vec![0u8; 8192];
        let mut pcm = Vec::with_capacity(4096);
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Ok(true);
            }
            let mut bytes = &buf[..n];
            if header > 0 {
                let skip = header.min(bytes.len());
                header -= skip;
                bytes = &bytes[skip..];
            }
            pcm.clear();
            let mut i = 0;
            if let Some(lo) = carry.take() {
                if let Some(&hi) = bytes.first() {
                    pcm.push(i16::from_le_bytes([lo, hi]) as f32 / 32768.0);
                    i = 1;
                } else {
                    carry = Some(lo);
                }
            }
            while i + 1 < bytes.len() {
                pcm.push(i16::from_le_bytes([bytes[i], bytes[i + 1]]) as f32 / 32768.0);
                i += 2;
            }
            if i < bytes.len() {
                carry = Some(bytes[i]);
            }
            if !pcm.is_empty() && !sink(&pcm) {
                return Ok(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn layer_progress() {
        assert_eq!(super::parse_layer("loading weights: layer 10/40"), Some(0.25));
        assert_eq!(super::parse_layer("ready in 21.0s"), None);
    }
}
