//! The agent: a light, private, local layer between your mic and your ears.
//!
//! ```text
//!  mic tap (native) ─▶ 16 kHz ─▶ phrases ─▶ STT ─▶ "Me" ─────────────┐   voice bank ◀─ your phrases (native rate)
//!                                            └─ private ─▶ router ─▶ action / context edit / model (tools)
//!  hear: app:Discord, devices ─▶ phrases ─▶ STT ─▶ "Them (Discord)" ─┤
//!                                                                    ▼
//!            context.md (me, style, people, situation, facts, rules, summary) + recent lines
//!                                                                    ▼
//!                                 reply (LLM) ─▶ sentences ─▶ speaker ─▶ Drama 3 ─▶ router (virtual mic + your output)
//! ```
//!
//! One thread owns all agent state (like the dictation controller); model requests, speech
//! and service start-up run on worker threads and report back through [`Msg`]. Services
//! (local LLM, voice model) run only while the agent is on, so turning it off frees the GPU.
//!
//! The work is split by concern: [`listen`] (streams, phrases, transcripts), [`think`]
//! (routing, context, replies, commands, speaking) and [`ipc`].

mod ipc;
pub mod listen;
pub mod llm;
pub mod segment;
pub mod server;
pub mod speaker;
mod think;
pub mod tools;
pub mod tts;
pub mod voice;
mod voices;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use mimic_core::agent::stream;
use mimic_core::agent::{AgentCfg, AgentStatus, ContextDoc, LlmProvider, ServiceStatus, StreamStatus, Timeline};
use mimic_core::config::Config;
use mimic_core::protocol::ModelPhase;
use mimic_core::text::Transcript;
use parking_lot::Mutex;

use crate::audio::{AudioEv, Tap};
use crate::engine::EngineCmd;
use crate::hotkeys::AgentKey;
use crate::ipc_server::{Hub, IpcCall};
use crate::overlay::Overlay;
use crate::router::Router;
use listen::{HearStream, History, MicFeed, Pending};
use segment::{Segmenter, Spans};
use speaker::{Speaker, SpeakerEv};

/// STT job ids from the agent live above this (the dictation controller counts from 1).
pub const JOB_BASE: u64 = 1 << 48;

pub enum Msg {
    /// An event from hearing stream `id`.
    Hear {
        id: u64,
        ev: AudioEv,
    },
    Stt {
        id: u64,
        result: Result<Transcript, String>,
    },
    Hotkey(AgentKey, bool),
    Ipc(IpcCall),
    Config(Box<Config>),
    /// A dictation session started/ended (its speech is for the text field, not the agent).
    Dictating(bool),
    Service(ServiceEv),
    Llm(LlmEv),
    Speaker(SpeakerEv),
    Shutdown,
}

pub enum ServiceEv {
    Llm(Result<server::Server, String>),
    Tts(Result<tts::Drama, String>),
    Voiceaware(Result<voices::Loaded, String>),
    Progress { tts: bool, detail: String, progress: Option<f32> },
}

pub enum LlmEv {
    /// A speakable sentence of a reply.
    Sentence {
        token: u64,
        text: String,
    },
    ReplyDone {
        token: u64,
        text: String,
        error: Option<String>,
    },
    CommandDone {
        utterance: String,
        acc: Result<stream::ChatAccumulator, String>,
    },
    /// A new summary of the conversation up to line `upto`.
    Summary {
        upto: u64,
        text: Result<String, String>,
    },
}

#[derive(Clone)]
pub struct AgentHandle {
    tx: Sender<Msg>,
    status: Arc<Mutex<AgentStatus>>,
}

impl AgentHandle {
    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
    }
    pub fn status(&self) -> AgentStatus {
        self.status.lock().clone()
    }
}

pub(crate) struct Agent {
    cfg: Config,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    engine: Sender<EngineCmd>,
    hub: Arc<Hub>,
    overlay: Overlay,
    router: Router,
    status: Arc<Mutex<AgentStatus>>,
    mic_tap: Tap,

    enabled: bool,
    autopilot: bool,
    talking: bool,

    mic: Option<MicFeed>,
    mic_seg: Segmenter,
    mic_hist: History,
    hears: Vec<HearStream>,
    next_hear: u64,

    talk: Spans,
    dictating: Spans,
    speak_mic: Spans,

    pending: HashMap<u64, Pending>,
    next_job: u64,
    /// Per stream (`None` = your mic): its recent speech, decoded in front of each new phrase
    /// so the speech model hears it in context (language, voices) instead of in isolation.
    stt_ctx: HashMap<Option<u64>, listen::SttContext>,
    talk_texts: Vec<String>,
    talk_released: bool,

    timeline: Timeline,
    /// What the agent knows; mirrored to `context.md`.
    ctx: ContextDoc,
    ctx_mtime: Option<SystemTime>,
    ctx_checked: Instant,
    /// Lines up to this id are folded into the summary.
    summarized_upto: u64,
    summarizing: bool,

    llm_server: Option<server::Server>,
    llm: Option<llm::Client>,
    llm_status: ServiceStatus,
    llm_loading: bool,
    tts_status: ServiceStatus,
    tts_loading: bool,
    tts_used: Instant,
    shared: Arc<speaker::Shared>,
    speaker: Speaker,

    cancel: llm::Cancel,
    thinking: bool,
    reply_due: Option<Instant>,
    /// The pending reply answers a question (silence isn't an option).
    reply_forced: bool,
    /// The reply being generated must say something (echoed sentences are skipped, not fatal).
    reply_must: bool,
    /// The agent line being spoken.
    line: Option<u64>,
    speaking: bool,
    last_pub: Instant,
    configured: bool,
    /// When the current reply was requested (latency logging).
    reply_t0: Option<Instant>,
    /// The time string given to the current reply (to catch it being parroted).
    reply_now: String,
    /// Sentences of the answer being generated (spoken together when it is complete).
    reply_buf: Vec<String>,
    /// Last thing said or told (the conversation is forgotten after `forget_after_min` of quiet).
    last_activity: Instant,
    /// Voices heard on calls (persistent: `speakers.json`).
    speakers: mimic_core::agent::speakers::SpeakerBook,
    turn: Option<crate::voiceaware::TurnDetector>,
    embed: Option<crate::voiceaware::Embedder>,
    voiceaware_loading: bool,
    /// What the agent is saying right now (to know what was said if it's cut off).
    utter: Option<Utterance>,
    /// Someone talks over the agent: stop when playback reaches `stop_at` (fraction).
    barge: Option<Barge>,
}

pub(crate) struct Utterance {
    pub line: u64,
    /// The spoken words (without voice-direction tags).
    pub words: String,
    pub gen: u64,
}

pub(crate) struct Barge {
    pub since: Instant,
    pub stop_at: f64,
}

pub fn spawn(cfg: &Config, mic_tap: Tap, engine: Sender<EngineCmd>, hub: Arc<Hub>, overlay: Overlay, router: Router) -> AgentHandle {
    let (tx, rx) = crossbeam_channel::unbounded();
    let status = Arc::new(Mutex::new(AgentStatus::default()));
    let h = AgentHandle { tx: tx.clone(), status: status.clone() };
    let shared = Arc::new(speaker::Shared {
        tts: Mutex::new(None),
        voice: Mutex::new(voice::VoiceBank::new(cfg.agent.tts.pinned)),
        voice_s: Mutex::new(cfg.agent.tts.voice_s),
    });
    let speaker = Speaker::spawn(shared.clone(), router.clone(), tx.clone());
    let cfg = cfg.clone();
    std::thread::Builder::new()
        .name("mimic-agent".into())
        .spawn(move || {
            let (ctx, ctx_mtime) = think::load_context(cfg.agent.forget_after_min);
            let mut me = Agent {
                mic_seg: Segmenter::new(700, 28),
                mic_hist: History::new(cfg.agent.history_min as usize * 60),
                cfg: Config::default(),
                tx,
                rx,
                engine,
                hub,
                overlay,
                router,
                status,
                mic_tap,
                enabled: false,
                autopilot: false,
                talking: false,
                mic: None,
                hears: Vec::new(),
                next_hear: 1,
                talk: Spans::default(),
                dictating: Spans::default(),
                speak_mic: Spans::default(),
                pending: HashMap::new(),
                next_job: JOB_BASE,
                stt_ctx: HashMap::new(),
                talk_texts: Vec::new(),
                talk_released: false,
                timeline: Timeline::new(400),
                ctx,
                ctx_mtime,
                ctx_checked: Instant::now(),
                summarized_upto: 0,
                summarizing: false,
                llm_server: None,
                llm: None,
                llm_status: ServiceStatus::default(),
                llm_loading: false,
                tts_status: ServiceStatus::default(),
                tts_loading: false,
                tts_used: Instant::now(),
                shared,
                speaker,
                cancel: llm::Cancel::default(),
                thinking: false,
                reply_due: None,
                reply_forced: false,
                reply_must: false,
                line: None,
                speaking: false,
                last_pub: Instant::now(),
                configured: false,
                reply_t0: None,
                reply_now: String::new(),
                reply_buf: Vec::new(),
                last_activity: Instant::now(),
                speakers: voices::load_speakers(),
                turn: None,
                embed: None,
                voiceaware_loading: false,
                utter: None,
                barge: None,
            };
            me.apply_config(cfg);
            me.run();
        })
        .expect("spawn agent thread");
    h
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub(crate) fn local_time() -> String {
    chrono::Local::now().format("%A %-d %B %Y, %H:%M").to_string()
}

impl Agent {
    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Msg::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Ok(m) => self.handle(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            while let Ok(m) = self.rx.try_recv() {
                if matches!(m, Msg::Shutdown) {
                    self.stop_all();
                    return;
                }
                self.handle(m);
            }
            self.tick();
        }
        self.stop_all();
    }

    fn stop_all(&mut self) {
        self.interrupt(false);
        *self.shared.tts.lock() = None;
        self.llm_server = None;
        self.hears.clear();
    }

    fn a(&self) -> &AgentCfg {
        &self.cfg.agent
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Hear { id, ev } => self.on_hear(id, ev),
            Msg::Stt { id, result } => self.on_stt(id, result),
            Msg::Hotkey(k, down) => self.on_hotkey(k, down),
            Msg::Ipc(call) => self.on_ipc(call),
            Msg::Config(c) => self.apply_config(*c),
            Msg::Dictating(on) => {
                let pos = self.mic_seg.pos();
                self.dictating.set(on, pos);
            }
            Msg::Service(e) => self.on_service(e),
            Msg::Llm(e) => self.on_llm(e),
            Msg::Speaker(e) => self.on_speaker(e),
            Msg::Shutdown => {}
        }
    }

    // ------------------------------------------------------------- config & lifecycle

    fn apply_config(&mut self, next: Config) {
        let prev = std::mem::replace(&mut self.cfg, next);
        let (p, n) = (&prev.agent, &self.cfg.agent.clone());
        let first = !self.configured;
        self.configured = true;
        // The virtual mic and your output run whenever configured (agent on or off: the mic
        // passthrough and the soundboard use them too).
        if first || p.route != n.route || prev.audio.device != self.cfg.audio.device {
            if n.route.output.trim().is_empty() && !n.route.monitor {
                self.router.stop();
            } else {
                let mut route = n.route.clone();
                let mic = if self.cfg.audio.device.trim().is_empty() { crate::audio::default_input_name() } else { self.cfg.audio.device.clone() };
                if route.passthrough && mimic_core::agent::passthrough_loops(&route.output, &mic) {
                    // Your mic already feeds that channel: passing it through again would echo.
                    log::warn!("virtual mic `{}` is also your microphone: not passing the mic through (it would echo)", route.output);
                    self.overlay.notice("Your mic already goes into that output, so Mimic won't pass it through again (it would echo)", true);
                    route.passthrough = false;
                }
                self.router.configure(&route);
            }
        }
        for h in &mut self.hears {
            h.seg.set_pause(n.hear.pause_ms);
        }
        *self.shared.voice_s.lock() = n.tts.voice_s;
        if p.tts.voice_s != n.tts.voice_s {
            self.shared.voice.lock().dirty = true;
        }
        if p.tts.pinned != n.tts.pinned && !first {
            let mut v = self.shared.voice.lock();
            if n.tts.pinned {
                if let Err(e) = v.pin(n.tts.voice_s) {
                    self.overlay.notice(&format!("{e:#}"), true);
                }
            } else {
                v.unpin();
            }
        }
        // Services: restart what changed.
        if p.llm != n.llm {
            self.llm = None;
            if p.llm.provider != n.llm.provider || p.llm.local_model != n.llm.local_model || p.llm.local_ctx != n.llm.local_ctx {
                self.llm_server = None;
                self.llm_status = ServiceStatus::default();
            }
        }
        if p.tts.model != n.tts.model || p.tts.experts != n.tts.experts || p.tts.temperature != n.tts.temperature || p.tts.vram_layers != n.tts.vram_layers {
            *self.shared.tts.lock() = None;
            self.tts_status = ServiceStatus::default();
        }
        if n.enabled != self.enabled {
            self.set_enabled(n.enabled);
        } else if self.enabled {
            if p.hear != n.hear || first {
                self.open_hears();
            }
            self.ensure_services();
        }
        self.publish(true);
    }

    fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if on {
            log::info!("agent on");
            self.mic = Some(MicFeed::new(&self.mic_tap));
            self.open_hears();
            self.start_voiceaware();
            self.ensure_services();
        } else {
            log::info!("agent off");
            self.interrupt(false);
            self.autopilot = false;
            self.talking = false;
            self.mic = None;
            self.hears.clear();
            // Free the GPU.
            *self.shared.tts.lock() = None;
            self.llm_server = None;
            self.llm = None;
            self.llm_status = ServiceStatus::default();
            self.tts_status = ServiceStatus::default();
            self.router.shared.mute_mic.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Start whatever is missing (in the background).
    fn ensure_services(&mut self) {
        if !self.enabled {
            return;
        }
        let a = self.a().clone();
        if self.llm.is_none() {
            match a.llm.provider {
                LlmProvider::Custom => match llm::Client::new(&a.llm, None) {
                    Ok(c) => {
                        self.llm_status = ServiceStatus { phase: ModelPhase::Ready, name: c.name.clone(), ..Default::default() };
                        self.llm = Some(c);
                    }
                    Err(e) => self.llm_status = ServiceStatus { phase: ModelPhase::Error, error: Some(format!("{e:#}")), ..Default::default() },
                },
                LlmProvider::Local => {
                    if let Some(s) = &self.llm_server {
                        if let Ok(c) = llm::Client::new(&a.llm, Some(&s.base)) {
                            self.llm_status = ServiceStatus { phase: ModelPhase::Ready, name: c.name.clone(), ..Default::default() };
                            self.llm = Some(c);
                        }
                    } else if !self.llm_loading && self.llm_status.phase != ModelPhase::Error {
                        if !llm::local_installed(&a.llm) {
                            self.llm_status = ServiceStatus {
                                phase: ModelPhase::NotInstalled,
                                name: llm::LOCAL_NAME.into(),
                                error: Some(format!(
                                    "needs {} and {} (scripts\\setup-agent.ps1)",
                                    llm::llama_exe().display(),
                                    llm::local_model(&a.llm).display()
                                )),
                                ..Default::default()
                            };
                        } else {
                            self.llm_loading = true;
                            self.llm_status =
                                ServiceStatus { phase: ModelPhase::Loading, name: llm::LOCAL_NAME.into(), detail: "Starting".into(), ..Default::default() };
                            let tx = self.tx.clone();
                            let cfg = a.llm.clone();
                            std::thread::spawn(move || {
                                let t2 = tx.clone();
                                let p = move |d: &str, f: Option<f32>| {
                                    let _ = t2.send(Msg::Service(ServiceEv::Progress { tts: false, detail: d.into(), progress: f }));
                                };
                                let r = llm::start_local(&cfg, &p).map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Msg::Service(ServiceEv::Llm(r)));
                            });
                        }
                    }
                }
            }
        }
        // Not after an error (retry with agent.load) or an idle unload (it reloads on demand).
        let idle_off = self.tts_status.phase == ModelPhase::Unloaded && self.tts_status.detail == "idle";
        if self.shared.tts.lock().is_none() && !self.tts_loading && self.tts_status.phase != ModelPhase::Error && !idle_off {
            if !tts::installed(&a.tts) {
                self.tts_status = ServiceStatus {
                    phase: ModelPhase::NotInstalled,
                    name: tts::NAME.into(),
                    error: Some(format!("needs {} and {} (scripts\\setup-agent.ps1)", tts::exe().display(), tts::model_dir(&a.tts).display())),
                    ..Default::default()
                };
            } else {
                self.start_tts();
            }
        }
    }

    fn start_tts(&mut self) {
        self.tts_loading = true;
        self.tts_status = ServiceStatus { phase: ModelPhase::Loading, name: tts::NAME.into(), detail: "Starting".into(), ..Default::default() };
        let tx = self.tx.clone();
        let cfg = self.a().tts.clone();
        std::thread::spawn(move || {
            let t2 = tx.clone();
            let p = move |d: &str, f: Option<f32>| {
                let _ = t2.send(Msg::Service(ServiceEv::Progress { tts: true, detail: d.into(), progress: f }));
            };
            let r = tts::Drama::start(&cfg, &p).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Service(ServiceEv::Tts(r)));
        });
    }

    fn on_service(&mut self, e: ServiceEv) {
        match e {
            ServiceEv::Progress { tts, detail, progress } => {
                let s = if tts { &mut self.tts_status } else { &mut self.llm_status };
                if s.phase == ModelPhase::Loading {
                    s.detail = match progress {
                        Some(p) => format!("{detail} {:.0}%", p * 100.0),
                        None => detail,
                    };
                }
            }
            ServiceEv::Llm(r) => {
                self.llm_loading = false;
                match r {
                    Ok(s) if self.enabled && self.a().llm.provider == LlmProvider::Local => {
                        self.llm_server = Some(s);
                        self.llm = None;
                        self.ensure_services();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("local model: {e}");
                        self.llm_status = ServiceStatus { phase: ModelPhase::Error, name: llm::LOCAL_NAME.into(), error: Some(e), ..Default::default() };
                    }
                }
            }
            ServiceEv::Voiceaware(r) => self.on_voiceaware(r),
            ServiceEv::Tts(r) => {
                self.tts_loading = false;
                match r {
                    Ok(d) if self.enabled => {
                        *self.shared.tts.lock() = Some(Arc::new(d));
                        // A fresh server knows no voice yet.
                        self.shared.voice.lock().dirty = true;
                        self.tts_used = Instant::now();
                        self.tts_status = ServiceStatus {
                            phase: ModelPhase::Ready,
                            name: tts::NAME.into(),
                            detail: tts::placement(&self.cfg.agent.tts),
                            ..Default::default()
                        };
                        log::info!("voice model ready");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("voice model: {e}");
                        self.tts_status = ServiceStatus { phase: ModelPhase::Error, name: tts::NAME.into(), error: Some(e), ..Default::default() };
                    }
                }
            }
        }
        self.publish(true);
    }

    // ------------------------------------------------------------- tick

    fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        self.pump_mic();
        let now = Instant::now();

        // Speaking state (current speech, or audio still draining).
        let speaking = self.speaker.active() || self.router.pending_s() > 0.03;
        if speaking != self.speaking {
            self.speaking = speaking;
            if !speaking {
                self.line = None;
            }
            self.publish(true);
        }
        let mp = self.mic_seg.pos();
        self.speak_mic.set(self.speaking, mp);
        for h in &mut self.hears {
            let hp = h.seg.pos();
            h.speak.set(self.speaking, hp);
        }
        let mute = self.speaking || self.talking || (self.autopilot && self.a().mute_mic_in_autopilot);
        self.router.shared.mute_mic.store(mute, std::sync::atomic::Ordering::Relaxed);

        // Barge-in: someone started talking over the agent.
        self.check_barge_in();

        // Autopilot turn-taking.
        if let Some(due) = self.reply_due {
            let others_talking = self.hears.iter().any(|h| h.seg.in_phrase());
            if others_talking || self.mic_seg.in_phrase() && !self.a().mute_mic_in_autopilot {
                // Someone is talking: wait for the next pause.
                self.reply_due = None;
            } else if now >= due && !self.pending.values().any(|p| p.hear.is_some()) {
                self.reply_due = None;
                let forced = std::mem::take(&mut self.reply_forced);
                self.start_reply(forced, None);
            }
        }

        self.maybe_summarize();

        // A long quiet ends the conversation: forget it (not what you said about yourself).
        let forget = self.a().forget_after_min;
        if forget > 0 && !self.speaking && !self.thinking && self.last_activity.elapsed() > Duration::from_secs(forget as u64 * 60) {
            let has_session = !self.timeline.is_empty() || mimic_core::agent::Section::ALL.iter().any(|s| s.is_session() && !self.ctx.items(*s).is_empty());
            if has_session {
                log::info!("quiet for {forget} min: forgetting the conversation");
                self.forget_conversation();
            }
            self.last_activity = Instant::now();
        }
        if self.ctx_checked.elapsed() > Duration::from_secs(2) {
            self.ctx_checked = Instant::now();
            self.reload_context_if_edited();
        }

        // Idle unload of the voice model.
        let um = self.a().tts.unload_after_min;
        if um > 0 && !self.speaking && self.tts_used.elapsed() > Duration::from_secs(um as u64 * 60) && self.shared.tts.lock().is_some() {
            log::info!("voice model idle for {um} min: unloading");
            *self.shared.tts.lock() = None;
            self.tts_status = ServiceStatus { phase: ModelPhase::Unloaded, name: tts::NAME.into(), detail: "idle".into(), ..Default::default() };
            self.publish(true);
        }

        // Bound span bookkeeping.
        let keep = |p: usize| p.saturating_sub(crate::dsp::SR * 120);
        self.talk.prune(keep(mp));
        self.dictating.prune(keep(mp));
        self.speak_mic.prune(keep(mp));
        for h in &mut self.hears {
            let k = keep(h.seg.pos());
            h.speak.prune(k);
        }

        if self.last_pub.elapsed() > Duration::from_millis(500) {
            self.publish(false);
        }
    }

    fn monitor_device(&self) -> Option<String> {
        let r = &self.a().route;
        r.monitor.then(|| if r.monitor_device.trim().is_empty() { crate::audio::default_output_name() } else { r.monitor_device.trim().to_string() })
    }

    fn snapshot_status(&self) -> AgentStatus {
        let v = self.shared.voice.lock();
        AgentStatus {
            enabled: self.enabled,
            autopilot: self.autopilot,
            talking: self.talking,
            thinking: self.thinking,
            speaking: self.speaking,
            llm: self.llm_status.clone(),
            tts: self.tts_status.clone(),
            voice_s: v.seconds(self.a().tts.voice_s),
            voice_pinned: v.is_pinned(),
            hear: self.hears.iter().map(|h| h.status.clone()).collect(),
            route: self.router.status(),
            context: self.ctx.clone(),
            // Whole seconds of a fixed instant, so the status doesn't change on every call.
            last_active_s: std::time::SystemTime::now()
                .checked_sub(self.last_activity.elapsed())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    fn publish(&mut self, force: bool) {
        let st = self.snapshot_status();
        let changed = *self.status.lock() != st;
        if changed || force {
            *self.status.lock() = st.clone();
            if changed {
                self.hub.broadcast("agent", serde_json::to_value(&st).unwrap_or_default());
            }
        }
        self.last_pub = Instant::now();
    }
}

/// Status line for a stream that hasn't opened yet.
pub(crate) fn pending_status(label: &str) -> StreamStatus {
    StreamStatus { label: label.to_string(), ..Default::default() }
}
