//! Speaking: a worker that turns queued sentences into audio on the router, one at a time,
//! refreshing the clone reference first when your voice bank changed. Every job carries a
//! generation; bumping it (barge-in, stop) abandons queued and in-flight speech at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use super::tts::{self, Drama};
use super::voice::VoiceBank;
use super::Msg;
use crate::router::Router;

pub struct Job {
    pub gen: u64,
    /// Timeline line this sentence belongs to.
    pub line: u64,
    pub text: String,
}

pub enum SpeakerEv {
    /// A sentence started playing (for the timeline).
    Started {
        gen: u64,
        line: u64,
        text: String,
    },
    /// The queue for this generation is empty (audio may still be draining in the router).
    Idle,
    Error(String),
}

#[derive(Clone)]
pub struct Speaker {
    tx: Sender<Job>,
    pub gen: Arc<AtomicU64>,
    /// Generation of the job being spoken + 1 (0 = idle).
    busy: Arc<AtomicU64>,
    /// Progress of the current utterance (what has been generated so far).
    pub progress: Arc<Progress>,
}

/// How much of the current utterance has been generated (playback lags by the router's queue).
#[derive(Default)]
pub struct Progress {
    /// Samples pushed to the router (at the TTS rate).
    pub pushed: AtomicU64,
    /// Generation of the utterance these counters belong to.
    pub gen: AtomicU64,
    /// The whole utterance has been generated.
    pub done: std::sync::atomic::AtomicBool,
}

impl Progress {
    pub fn pushed_s(&self) -> f64 {
        self.pushed.load(Ordering::SeqCst) as f64 / tts::RATE as f64
    }
}

pub struct Shared {
    pub tts: Mutex<Option<Arc<Drama>>>,
    pub voice: Mutex<VoiceBank>,
    pub voice_s: Mutex<f32>,
}

impl Speaker {
    pub fn spawn(shared: Arc<Shared>, router: Router, events: Sender<Msg>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let gen = Arc::new(AtomicU64::new(0));
        let busy = Arc::new(AtomicU64::new(0));
        let progress = Arc::new(Progress::default());
        let me = Self { tx, gen: gen.clone(), busy: busy.clone(), progress: progress.clone() };
        std::thread::Builder::new().name("mimic-speaker".into()).spawn(move || run(rx, gen, busy, shared, router, events, progress)).expect("spawn speaker");
        me
    }

    pub fn say(&self, line: u64, text: String) {
        let gen = self.gen.load(Ordering::SeqCst);
        let _ = self.tx.send(Job { gen, line, text });
    }

    /// Abandon everything queued or playing.
    pub fn stop(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current(&self) -> u64 {
        self.gen.load(Ordering::SeqCst)
    }

    /// Speaking a sentence of the current generation (abandoned speech winding down doesn't count).
    pub fn active(&self) -> bool {
        self.busy.load(Ordering::SeqCst) == self.current() + 1
    }
}

/// Give the TTS the current clone reference if it changed. Returns whether a voice is set.
fn refresh_voice(tts: &Drama, shared: &Shared, registered: &mut bool) -> bool {
    let reference = {
        let mut v = shared.voice.lock();
        if v.dirty || !*registered {
            v.dirty = false;
            Some(v.reference(*shared.voice_s.lock()))
        } else {
            None
        }
    };
    match reference {
        None => *registered,
        Some(None) => false,
        Some(Some((pcm, rate, text))) => match tts.set_voice(&pcm, rate, &text) {
            Ok(()) => {
                *registered = true;
                log::debug!("voice reference updated ({:.1} s)", pcm.len() as f32 / rate as f32);
                true
            }
            Err(e) => {
                log::warn!("voice reference: {e:#}");
                shared.voice.lock().dirty = true;
                *registered
            }
        },
    }
}

fn run(rx: Receiver<Job>, gen: Arc<AtomicU64>, busy: Arc<AtomicU64>, shared: Arc<Shared>, router: Router, events: Sender<Msg>, progress: Arc<Progress>) {
    // Which TTS instance has our voice (a restarted server knows none).
    let mut registered_on: usize = 0;
    loop {
        let job = match rx.recv_timeout(std::time::Duration::from_millis(400)) {
            Ok(j) => j,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Idle: keep the TTS's reference fresh so speaking never waits for it.
                if let Some(t) = shared.tts.lock().clone() {
                    let mut reg = registered_on == Arc::as_ptr(&t) as usize;
                    if refresh_voice(&t, &shared, &mut reg) && reg {
                        registered_on = Arc::as_ptr(&t) as usize;
                    }
                }
                continue;
            }
        };
        if job.gen != gen.load(Ordering::SeqCst) {
            continue;
        }
        busy.store(job.gen + 1, Ordering::SeqCst);
        // The voice model may still be loading: wait for it (up to 2 minutes) unless cancelled.
        let t0 = std::time::Instant::now();
        let tts = loop {
            if let Some(t) = shared.tts.lock().clone() {
                break Some(t);
            }
            if job.gen != gen.load(Ordering::SeqCst) || t0.elapsed() > std::time::Duration::from_secs(120) {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let Some(tts) = tts else {
            if job.gen == gen.load(Ordering::SeqCst) {
                let _ = events.send(Msg::Speaker(SpeakerEv::Error("the voice model isn't loaded".into())));
            }
            busy.store(0, Ordering::SeqCst);
            continue;
        };
        let mut reg = registered_on == Arc::as_ptr(&tts) as usize;
        let has_voice = refresh_voice(&tts, &shared, &mut reg);
        if reg {
            registered_on = Arc::as_ptr(&tts) as usize;
        }
        let voice = has_voice.then_some(tts::VOICE_NAME);
        let _ = events.send(Msg::Speaker(SpeakerEv::Started { gen: job.gen, line: job.line, text: job.text.clone() }));
        let cancelled = || gen.load(Ordering::SeqCst) != job.gen;
        progress.gen.store(job.gen, Ordering::SeqCst);
        progress.pushed.store(0, Ordering::SeqCst);
        progress.done.store(false, Ordering::SeqCst);
        let t0 = std::time::Instant::now();
        let mut first = true;
        let under0 = router.shared.underruns.load(Ordering::Relaxed);
        router.voice_open(true);
        let mut sink = |pcm: &[f32]| {
            if first {
                first = false;
                log::info!("speech: first audio after {} ms ({:?})", t0.elapsed().as_millis(), job.text.chars().take(40).collect::<String>());
            }
            let ok = router.push_voice(pcm, &cancelled);
            if ok {
                progress.pushed.fetch_add(pcm.len() as u64, Ordering::SeqCst);
            }
            ok
        };
        if let Err(e) = tts.speak(&job.text, voice, &mut sink) {
            log::warn!("speech: {e:#}");
            shared.voice.lock().dirty = true;
            let _ = events.send(Msg::Speaker(SpeakerEv::Error(format!("{e:#}"))));
        }
        router.voice_open(false);
        progress.done.store(true, Ordering::SeqCst);
        // Playback lags generation; judge the utterance once it has drained (or was cut).
        while router.pending_s() > 0.0 && gen.load(Ordering::SeqCst) == job.gen && rx.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let under = router.shared.underruns.load(Ordering::Relaxed) - under0;
        if under > 0 {
            log::info!("speech: {under} underrun(s) (generation slower than playback); lead now {:.2} s", router.lead_s());
        } else if gen.load(Ordering::SeqCst) == job.gen {
            router.voice_clean();
        }
        if rx.is_empty() {
            busy.store(0, Ordering::SeqCst);
            let _ = events.send(Msg::Speaker(SpeakerEv::Idle));
        }
    }
}
