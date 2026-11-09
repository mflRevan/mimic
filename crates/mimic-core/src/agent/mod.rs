//! The agent layer: a private, local assistant that hears what you hear and can talk through
//! your microphone in your own (cloned) voice.
//!
//! This module holds everything that is pure logic (config, the context document and its
//! deterministic router, conversation timeline, prompts, streaming parsers, wake word, voice
//! reference selection) so it is unit-tested here; the daemon owns devices, servers and threads.

pub mod context;
pub mod prompt;
pub mod route;
pub mod speakers;
pub mod stream;
pub mod timeline;
pub mod turn;
pub mod voice;
pub mod wake;

use serde::{Deserialize, Serialize};

use crate::protocol::ModelPhase;

pub use context::{ContextDoc, Section};
pub use timeline::{Line, Timeline, Who};

// ------------------------------------------------------------------ config

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentCfg {
    /// Master switch: hearing, voice reference buffering, the wake word and the agent shortcuts.
    pub enabled: bool,
    /// The setting the agent plays in: which of [`Modes`] is active. What it knows about this
    /// conversation lives in the context document (`context.md`).
    pub mode: Mode,
    /// The text of each mode (editable; persists across conversations).
    pub modes: Modes,
    /// Old single persona text; folded into the modes when an older config is loaded.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub persona: String,
    /// Saying this first ("Mimic, note that ...") talks to the agent privately. Empty = off.
    pub wakeword: String,
    /// In autopilot, how long the others must be quiet before the agent answers.
    pub reply_after_ms: u32,
    /// What to do when someone talks over the agent.
    #[serde(deserialize_with = "barge_in_compat")]
    pub barge_in: BargeIn,
    /// Use the end-of-turn model to answer as soon as someone has finished (not after a fixed pause).
    pub turn_detection: bool,
    /// Tell the voices on a call apart (speaker embeddings), so lines are labelled by person.
    pub speaker_id: bool,
    /// In autopilot your real microphone is not passed through: everything you say goes to the agent.
    pub mute_mic_in_autopilot: bool,
    /// Lines of conversation the agent sees verbatim; older ones live in the summary.
    pub context_lines: u32,
    /// Longest spoken answer, in sentences (small models ramble; speech should be short).
    pub max_sentences: u32,
    /// Fold the conversation into the summary after this many new lines (0 = never).
    pub summarize_every: u32,
    /// After this many minutes without conversation, forget it: the transcript and the
    /// conversation part of the context (people, situation, facts, rules, summary). What you told
    /// it about yourself stays. 0 = never.
    pub forget_after_min: u32,
    /// Minutes of raw audio kept in memory per stream (for snapshots). 0 = none.
    pub history_min: u32,
    /// Where notes and snapshots go. Empty = `<data dir>/notes`.
    pub notes_dir: String,
    pub llm: LlmCfg,
    pub tts: TtsCfg,
    pub hear: HearCfg,
    pub route: RouteCfg,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: Mode::StandIn,
            modes: Modes::default(),
            persona: String::new(),
            wakeword: "mimic".into(),
            reply_after_ms: 600,
            barge_in: BargeIn::Finish,
            turn_detection: true,
            speaker_id: true,
            mute_mic_in_autopilot: true,
            context_lines: 16,
            max_sentences: 2,
            summarize_every: 10,
            forget_after_min: 20,
            history_min: 5,
            notes_dir: String::new(),
            llm: LlmCfg::default(),
            tts: TtsCfg::default(),
            hear: HearCfg::default(),
            route: RouteCfg::default(),
        }
    }
}

/// The setting the agent plays in. Each has an editable text in [`Modes`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Standing in for you: sound like you, keep things moving, commit to nothing.
    #[default]
    StandIn,
    /// You among friends: playful, quick, warm.
    Fun,
    /// The agent as itself: your sidekick with its own personality, in your voice.
    Sidekick,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Modes {
    pub stand_in: String,
    pub fun: String,
    pub sidekick: String,
}

pub const STAND_IN: &str = "You stand in for me on a call while I'm busy, speaking as me. Sound like me: relaxed, natural, short. \
Keep the conversation moving without committing to anything: no promises, dates, prices or decisions, and no facts you \
aren't sure of. If you'd have to decide or know something, say you'll check and get back to them.";
pub const FUN: &str = "You're me hanging out with friends on a call, in a good mood. Be playful and quick: react, joke, tease a \
little, laugh along. Short, natural lines, never mean, never a monologue.";
pub const SIDEKICK: &str = "You are Mimic, my AI sidekick, speaking with my voice but as yourself. You're curious, witty and \
helpful, with your own opinions. If someone asks, you're open that you're an AI helping me out. Keep it short and conversational.";

impl Default for Modes {
    fn default() -> Self {
        Self { stand_in: STAND_IN.into(), fun: FUN.into(), sidekick: SIDEKICK.into() }
    }
}

impl Modes {
    pub fn text(&self, m: Mode) -> &str {
        match m {
            Mode::StandIn => &self.stand_in,
            Mode::Fun => &self.fun,
            Mode::Sidekick => &self.sidekick,
        }
    }
}

/// Someone talks over the agent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BargeIn {
    /// Finish the current sentence, then stop (a quick "mhm" doesn't stop it).
    #[default]
    Finish,
    /// Stop at once.
    Stop,
    /// Keep talking.
    Off,
}

/// `barge_in` used to be a bool (true = stop).
fn barge_in_compat<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BargeIn, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Any {
        B(bool),
        M(BargeIn),
    }
    Ok(match Any::deserialize(d)? {
        Any::B(true) => BargeIn::Finish,
        Any::B(false) => BargeIn::Off,
        Any::M(m) => m,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LlmProvider {
    /// A managed `llama-server` with a small local model (Gemma 4 E2B by default).
    Local,
    /// Any OpenAI-compatible `POST {base}/chat/completions` endpoint.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LlmCfg {
    pub provider: LlmProvider,
    /// Custom endpoint, e.g. `https://api.openai.com/v1` or `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    pub api_key: String,
    /// Model name sent to the custom endpoint.
    pub model: String,
    /// Local model: GGUF path. Empty = Gemma 4 E2B under `<data dir>/models`.
    pub local_model: String,
    /// Local model: context size in tokens.
    pub local_ctx: u32,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_s: u32,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self {
            provider: LlmProvider::Local,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            local_model: String::new(),
            local_ctx: 8192,
            // Gemma 4's recommended sampling (model card): temperature 1.0, top_p 0.95, top_k 64.
            temperature: 1.0,
            max_tokens: 200,
            timeout_s: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TtsCfg {
    /// Directory of the converted Drama 3 int4 model. Empty = `<data dir>/models/drama-3-q4`.
    pub model: String,
    /// Routed experts: `q4` (int4, fast) or `fp8` (the original experts, needs
    /// `<data dir>/models/drama-3`; higher quality, much slower when they don't all fit in VRAM).
    pub experts: String,
    /// How many of the 40 layers keep their experts in VRAM; the rest are read from system RAM
    /// over PCIe (less VRAM, slower). 0 = all in VRAM (`q4`) / as many as fit (`fp8`).
    pub vram_layers: u32,
    /// Seconds of your recent speech used as the voice reference.
    pub voice_s: f32,
    /// Keep the current voice reference (stop updating it from new speech).
    pub pinned: bool,
    /// Sampling temperature (Drama 3 default 1.0; lower is steadier).
    pub temperature: f32,
    /// Unload the voice model after this many idle minutes (0 = keep loaded while the agent is on).
    pub unload_after_min: u32,
}

impl Default for TtsCfg {
    fn default() -> Self {
        // 16 of 40 expert layers on the GPU (~11 GB), the rest read from RAM: first sound in ~0.5 s.
        Self { model: String::new(), experts: "q4".into(), vram_layers: 16, voice_s: 12.0, pinned: false, temperature: 1.0, unload_after_min: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HearCfg {
    /// Transcribe what you hear so the agent can follow the conversation.
    pub enabled: bool,
    /// What to listen to, each transcribed and labelled separately:
    /// `app:<exe name>` (one app's audio, e.g. `app:Discord`), `device:<output name>`
    /// (everything an output plays), or `default` (the default output).
    pub sources: Vec<String>,
    /// A pause this long ends what someone said.
    pub pause_ms: u32,
}

impl Default for HearCfg {
    fn default() -> Self {
        // With turn detection a finished turn closes after ~0.2 s; this is the fallback pause.
        Self { enabled: true, sources: vec!["app:Discord".into()], pause_ms: 900 }
    }
}

/// A parsed hearing source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HearSource {
    /// One application's audio (process loopback), by executable name.
    App(String),
    /// Everything an output device plays; empty = the default output.
    Device(String),
    /// A WAV file played in real time (testing).
    File(String),
}

impl HearSource {
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        let (kind, rest) = t.split_once(':').map(|(a, b)| (a.to_ascii_lowercase(), b.trim())).unwrap_or((t.to_ascii_lowercase(), ""));
        match kind.as_str() {
            "app" if !rest.is_empty() => Some(Self::App(rest.trim_end_matches(".exe").trim_end_matches(".EXE").to_string())),
            "device" => Some(Self::Device(rest.to_string())),
            "default" if rest.is_empty() => Some(Self::Device(String::new())),
            "file" if !rest.is_empty() => Some(Self::File(t[5..].trim().to_string())),
            // A bare device name.
            _ => Some(Self::Device(t.to_string())),
        }
    }

    /// Short label shown with what was heard ("Discord", "Speakers").
    pub fn label(&self) -> String {
        match self {
            Self::App(a) => a.clone(),
            Self::Device(d) if d.is_empty() => "Output".into(),
            Self::Device(d) => d.split(" (").next().unwrap_or(d).to_string(),
            Self::File(p) => std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RouteCfg {
    /// Output device feeding a virtual microphone ("CABLE Input", "Speakers (Steam Streaming
    /// Microphone)"); apps select its other end as their mic. Empty = no virtual mic.
    pub output: String,
    /// Pass your real microphone through to `output` while the agent isn't talking.
    pub passthrough: bool,
    pub mic_gain: f32,
    pub voice_gain: f32,
    /// Also play the agent's voice (and sound effects) where you hear it.
    pub monitor: bool,
    /// Where you hear it. Empty = the Windows default output.
    pub monitor_device: String,
}

/// Passing the mic through would feed it back into itself: the virtual mic output is the same
/// endpoint the mic is read from (e.g. SteelSeries Sonar's "Microphone" channel, which has both
/// a playback and a recording side with the same name). The result is an echo.
pub fn passthrough_loops(output: &str, mic: &str) -> bool {
    let norm = |s: &str| s.trim().to_lowercase();
    let (o, m) = (norm(output), norm(mic));
    !o.is_empty() && !m.is_empty() && o == m
}

impl Default for RouteCfg {
    fn default() -> Self {
        Self { output: String::new(), passthrough: true, mic_gain: 1.0, voice_gain: 1.0, monitor: true, monitor_device: String::new() }
    }
}

// ------------------------------------------------------------------ status

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ServiceStatus {
    pub phase: ModelPhase,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub error: Option<String>,
    /// What it is ("Gemma 4 E2B (local)", "gpt-4.1-mini @ api.openai.com", "Drama 3").
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StreamStatus {
    #[serde(default)]
    pub label: String,
    pub device: String,
    pub open: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentStatus {
    pub enabled: bool,
    /// The agent answers the conversation on your behalf.
    pub autopilot: bool,
    /// You are talking privately to the agent (shortcut held).
    pub talking: bool,
    pub thinking: bool,
    pub speaking: bool,
    pub llm: ServiceStatus,
    pub tts: ServiceStatus,
    /// Seconds of your voice currently available as the clone reference.
    pub voice_s: f32,
    pub voice_pinned: bool,
    /// One per hearing source.
    pub hear: Vec<StreamStatus>,
    pub route: StreamStatus,
    /// What the agent knows (the context document).
    pub context: ContextDoc,
    /// When something last happened in the conversation (Unix seconds).
    #[serde(default)]
    pub last_active_s: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hear_sources() {
        assert_eq!(HearSource::parse("app:Discord.exe"), Some(HearSource::App("Discord".into())));
        assert_eq!(HearSource::parse("default"), Some(HearSource::Device(String::new())));
        assert_eq!(HearSource::parse("device:CABLE Output"), Some(HearSource::Device("CABLE Output".into())));
        assert_eq!(HearSource::parse("file:C:/a/them.wav"), Some(HearSource::File("C:/a/them.wav".into())));
        assert_eq!(HearSource::parse("SteelSeries Sonar - Chat (SteelSeries)"), Some(HearSource::Device("SteelSeries Sonar - Chat (SteelSeries)".into())));
        assert_eq!(HearSource::parse(" "), None);
        assert_eq!(HearSource::App("Discord".into()).label(), "Discord");
        assert_eq!(HearSource::Device("Speakers (Realtek)".into()).label(), "Speakers");
    }

    #[test]
    fn passthrough_loop_detection() {
        let sonar = "SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)";
        assert!(passthrough_loops(sonar, sonar));
        assert!(!passthrough_loops("Speakers (Steam Streaming Microphone)", "Microphone (fifine Microphone)"));
        assert!(!passthrough_loops("", sonar));
    }

    #[test]
    fn old_barge_in_and_modes() {
        let c: crate::config::Config = toml::from_str("[agent]\nbarge_in = true\n").unwrap();
        assert_eq!(c.agent.barge_in, BargeIn::Finish);
        let c: crate::config::Config = toml::from_str("[agent]\nbarge_in = false\n").unwrap();
        assert_eq!(c.agent.barge_in, BargeIn::Off);
        let c: crate::config::Config = toml::from_str("[agent]\nbarge_in = \"stop\"\nmode = \"fun\"\n").unwrap();
        assert_eq!(c.agent.barge_in, BargeIn::Stop);
        assert_eq!(c.agent.modes.text(c.agent.mode), FUN);
    }

    #[test]
    fn agent_config_roundtrips_and_partial_loads() {
        let c = crate::config::Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: crate::config::Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
        let c: crate::config::Config = toml::from_str("[agent]\nenabled = true\n[agent.llm]\nprovider = \"custom\"\n").unwrap();
        assert!(c.agent.enabled);
        assert_eq!(c.agent.llm.provider, LlmProvider::Custom);
        assert_eq!(c.agent.tts.experts, "q4");
        let mut c = crate::config::Config::default();
        c.set_path("agent.route.output", serde_json::Value::String("CABLE Input".into())).unwrap();
        assert_eq!(c.agent.route.output, "CABLE Input");
        c.set_path("agent.tts.voice_s", serde_json::Value::String("8".into())).unwrap();
        assert_eq!(c.agent.tts.voice_s, 8.0);
    }
}
