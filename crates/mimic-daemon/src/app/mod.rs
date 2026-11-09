//! The controller: one thread that owns all dictation state. Every other
//! thread (hooks, audio, engine, injector, IPC, tray) talks to it through one
//! channel, so there are no locks on the hot path and nothing here blocks.
//!
//! Life of a dictation:
//! 1. push-to-talk down -> session starts with the pre-roll ring buffer,
//!    model load is triggered if needed (audio keeps buffering meanwhile)
//! 2. audio is segmented into phrases by pauses; the open phrase gets live
//!    previews (shown in the overlay); a closed phrase gets a final decode,
//!    jointly with the previous phrase's tail when continuing (seam repair)
//! 3. finals are post-processed and typed strictly in order
//! 4. release -> post-roll keeps listening; a phrase that already ended in
//!    silence is finalized immediately (often from a preview that already
//!    covered it, so the text appears with ~zero extra decode time)
//!
//! This file holds the state (`App`, `Session`, ...), construction and the run
//! loop; `impl App` continues in the submodules, one per concern: `mic`,
//! `segment`, `dictation` (sessions and hotkeys), `results` (engine results and
//! typing), `popup`, `timers`, `install` (models), `config`, `tray`, `updates`,
//! `status` and `ipc`.

mod config;
mod dictation;
mod install;
mod ipc;
mod mic;
mod popup;
mod results;
mod segment;
mod status;
mod timers;
mod tray;
mod updates;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use mimic_core::config::{Config, Residency};
use mimic_core::protocol::*;
use mimic_core::rules::{Matcher, RuleSet};
use mimic_core::text::{Context, Transcript, Word};
use serde_json::json;

use crate::agent::{AgentHandle, Msg as AgentMsg};
use crate::audio::{AudioCmd, AudioEv};
use crate::dsp::{Ring, Vad, SR};
use crate::engine::{EngineCmd, EngineEv, EngineSpec};
use crate::hotkeys::{self, HotkeyEv};
use crate::inject::{Edit, InjectEv};
use crate::ipc_server::{Hub, IpcCall, Replier};
use crate::overlay::Overlay;
use crate::platform::{self, FocusCtx};
use crate::tray::TrayCmd;
use crate::updater::{UpdateEv, Updater};

pub enum Ev {
    Hotkey(HotkeyEv),
    Audio(AudioEv),
    Engine(EngineEv),
    Injected(InjectEv),
    Ipc(IpcCall),
    Install(InstallProgress),
    Tray(TrayCmd),
    Update(UpdateEv),
}

const S: fn(f32) -> usize = |secs| (secs * SR as f32) as usize;
const TAP_MAX: Duration = Duration::from_millis(260);
const RELEASE_PAUSE_MS: u32 = 220;
const SEAM_TAIL_S: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Hold,
    HandsFree,
}

#[derive(Debug, Clone, Default)]
struct Open {
    start: usize,
    speech_start: Option<usize>,
    speech_end: usize,
    voiced_blocks: usize,
    silence: usize,
}

struct Phrase {
    end: usize,
    speech_start: usize,
    speech_end: usize,
    /// Absolute sample where the decoded audio (excluding seam context) begins.
    decode_start: usize,
    boundary_s: Option<f32>,
    result: Option<Result<Transcript, String>>,
}

struct Session {
    id: u64,
    mode: Mode,
    started: Instant,
    /// Session audio; `audio[0]` is absolute sample `base`.
    audio: Vec<f32>,
    base: usize,
    vad: Vad,
    block_pos: usize,
    open: Open,
    phrases: Vec<Phrase>,
    emitted: usize,
    preview_inflight: bool,
    preview_from: usize,
    preview_end: usize,
    last_preview: Option<((usize, usize), Transcript)>,
    preview_text: String,
    capturing: bool,
    released: Option<Instant>,
    stop_at: Option<Instant>,
    tap_since: Option<Instant>,
    ignore_next_up: bool,
    add_rule_after: bool,
    cancelled: bool,
    pending_edits: usize,
    last_done: Option<Instant>,
    text: String,
    raw: String,
    app: String,
    error: Option<String>,
}

impl Session {
    fn pos(&self) -> usize {
        self.base + self.audio.len()
    }
    fn slice(&self, a: usize, b: usize) -> Vec<f32> {
        let a = a.max(self.base).min(self.pos());
        let b = b.max(a).min(self.pos());
        self.audio[a - self.base..b - self.base].to_vec()
    }
}

/// What was typed last, for continuation and in-place rule fixes.
struct LastInsert {
    ctx: FocusCtx,
    epoch: u64,
    at: Instant,
    auto_tail: String,
    ended_sentence: bool,
    tail_audio: Vec<f32>,
    words: Vec<Word>,
    env: Vec<f32>,
    text: String,
    flags: Context,
}

pub struct App {
    cfg: Config,
    rules: RuleSet,
    matcher: Matcher,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    audio: Sender<AudioCmd>,
    engine: Sender<EngineCmd>,
    inject: Sender<Edit>,
    hub: Arc<Hub>,
    overlay: Overlay,
    tray: crate::tray::TrayHandle,
    agent: AgentHandle,
    soundboard: crate::soundboard::Soundboard,
    started: Instant,
    model: ModelStatus,
    mic: MicStatus,
    mic_open: bool,
    ring: Ring,
    vad_mem: Vad,
    session: Option<Session>,
    next_id: u64,
    last: Option<LastInsert>,
    history: VecDeque<HistoryItem>,
    stats: Stats,
    latencies: VecDeque<u32>,
    last_used: Instant,
    load_at: Option<Instant>,
    unload_at: Option<Instant>,
    installing: Option<String>,
    popup_reply: Vec<Replier>,
    popup_return: u64,
    apply_after_popup: bool,
    apply_at: Option<Instant>,
    record: Option<(Replier, Instant)>,
    file_jobs: HashMap<u64, Replier>,
    edit_tags: HashMap<u64, u64>,
    level_acc: (f32, f32, usize),
    last_level: Instant,
    mic_close_at: Option<Instant>,
    quit: bool,
    install_info: InstallInfo,
    updater: Updater,
    /// A check started from the tray reports its result in the overlay.
    tray_check: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Handles {
    pub audio: Sender<AudioCmd>,
    pub engine: Sender<EngineCmd>,
    pub inject: Sender<Edit>,
    pub hub: Arc<Hub>,
    pub overlay: Overlay,
    pub tray: crate::tray::TrayHandle,
    pub agent: AgentHandle,
    pub soundboard: crate::soundboard::Soundboard,
}

impl App {
    pub fn new(tx: Sender<Ev>, rx: Receiver<Ev>, h: Handles, autostart: bool) -> Self {
        let mut damaged = Vec::new();
        let cfg = Config::load().unwrap_or_else(|e| {
            damaged.extend(set_aside(&mimic_core::paths::config_file(), "settings", &format!("{e:#}")));
            Config::default()
        });
        let rules = RuleSet::load().unwrap_or_else(|e| {
            damaged.extend(set_aside(&mimic_core::paths::rules_file(), "dictionary", &format!("{e:#}")));
            RuleSet::default()
        });
        let mut state = crate::lifecycle::State::load();
        let prev_version = crate::lifecycle::on_start(&mut state);
        let install_info = crate::lifecycle::install_info();
        log::info!("running from {} ({})", install_info.dir, install_info.kind);
        let matcher = rules.matcher();
        let ring = Ring::new(S(cfg.audio.preroll_ms as f32 / 1000.0).max(1));
        let mut app = Self {
            matcher,
            rules,
            tx,
            rx,
            audio: h.audio,
            engine: h.engine,
            inject: h.inject,
            hub: h.hub,
            overlay: h.overlay,
            tray: h.tray,
            agent: h.agent,
            soundboard: h.soundboard,
            started: Instant::now(),
            model: ModelStatus::default(),
            mic: MicStatus::default(),
            mic_open: false,
            ring,
            vad_mem: Vad::default(),
            session: None,
            next_id: 1,
            last: None,
            history: VecDeque::new(),
            stats: Stats::default(),
            latencies: VecDeque::new(),
            last_used: Instant::now(),
            load_at: None,
            unload_at: None,
            installing: None,
            popup_reply: Vec::new(),
            popup_return: 0,
            apply_after_popup: false,
            apply_at: None,
            record: None,
            file_jobs: HashMap::new(),
            edit_tags: HashMap::new(),
            level_acc: (0.0, 0.0, 0),
            last_level: Instant::now(),
            mic_close_at: None,
            quit: false,
            install_info,
            updater: Updater::new(state, prev_version.clone()),
            tray_check: false,
            cfg,
        };
        app.model = ModelStatus { id: app.cfg.model.id.clone(), name: EngineSpec::from_config(&app.cfg).name(), ..Default::default() };
        hotkeys::set_bindings(&app.cfg.hotkeys);
        app.overlay.configure(&app.cfg.ui);
        let _ = app.engine.send(EngineCmd::Configure(EngineSpec::from_config(&app.cfg)));
        if app.cfg.general.autostart {
            crate::lifecycle::repair_autostart(&app.install_info);
        }
        for what in &damaged {
            app.overlay.notice(&format!("Your {what} file couldn't be read; Mimic kept a copy and started fresh"), true);
        }
        if let Some(prev) = prev_version.filter(|p| mimic_core::version::newer(mimic_core::VERSION, p)) {
            if damaged.is_empty() {
                app.overlay.notice(&format!("Updated to Mimic {}", mimic_core::VERSION), false);
            }
            log::info!("first start after updating from {prev}");
        }
        app.agent.send(AgentMsg::Config(Box::new(app.cfg.clone())));
        if app.cfg.agent.enabled {
            app.open_mic();
        }
        if app.cfg.general.onboarded {
            if app.cfg.audio.keep_mic_open {
                app.open_mic();
            }
            if app.cfg.model.residency != Residency::OnDemand {
                let delay = if autostart { app.cfg.general.autostart_delay_s as u64 * 1000 } else { 300 };
                app.load_at = Some(Instant::now() + Duration::from_millis(delay));
            }
        }
        if app.cfg.ui.keep_warm {
            platform::open_ui(&["--hidden"]);
        }
        app
    }

    pub fn run(mut self) {
        while !self.quit {
            match self.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ev) => self.handle(ev),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            // Drain bursts before ticking.
            while let Ok(ev) = self.rx.try_recv() {
                self.handle(ev);
                if self.quit {
                    break;
                }
            }
            self.tick();
        }
        log::info!("shutting down");
        self.agent.send(AgentMsg::Shutdown);
        // Tell open windows this is a deliberate quit (they close instead of restarting us).
        self.hub.broadcast("quit", json!({}));
        let _ = self.engine.send(EngineCmd::Shutdown);
        self.overlay.hide();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Hotkey(h) => self.on_hotkey(h),
            Ev::Audio(a) => self.on_audio(a),
            Ev::Engine(e) => self.on_engine(e),
            Ev::Injected(InjectEv::Done { tag, at }) => self.on_injected(tag, at),
            Ev::Ipc(call) => self.on_ipc(call),
            Ev::Install(p) => self.on_install(p),
            Ev::Tray(t) => self.on_tray(t),
            Ev::Update(u) => self.on_update(u),
        }
    }
}

/// A settings file that exists but can't be parsed is moved aside before defaults
/// are used, so the next save can't destroy it. Returns what to tell the user about.
fn set_aside(path: &std::path::Path, what: &'static str, err: &str) -> Option<&'static str> {
    log::error!("{what}: {err}");
    if std::fs::read(path).is_err() {
        return None; // unreadable right now (locked?), not damaged: leave it alone
    }
    match mimic_core::paths::quarantine(path) {
        Ok(to) => {
            log::warn!("{what}: kept the damaged file as {}", to.display());
            Some(what)
        }
        Err(e) => {
            log::error!("{what}: could not move the damaged file aside: {e}");
            None
        }
    }
}
