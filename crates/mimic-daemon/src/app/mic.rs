//! Microphone open/close and incoming audio (levels, chunks into the ring and session).

use std::time::{Duration, Instant};

use mimic_core::protocol::*;
use serde_json::json;

use crate::audio::{AudioCmd, AudioEv};

use super::App;

impl App {
    // ------------------------------------------------------------- mic

    pub(super) fn open_mic(&mut self) {
        if !self.mic_open {
            self.mic_open = true;
            let _ = self.audio.send(AudioCmd::Open { device: self.cfg.audio.device.clone(), gain: self.cfg.audio.gain });
        }
        self.mic_close_at = None;
    }

    pub(super) fn close_mic(&mut self) {
        if self.mic_open {
            self.mic_open = false;
            self.ring.clear();
            let _ = self.audio.send(AudioCmd::Close);
        }
    }

    pub(super) fn on_audio(&mut self, a: AudioEv) {
        match a {
            AudioEv::Chunk(c) => self.on_chunk(c),
            AudioEv::Opened { device, sample_rate } => {
                self.mic = MicStatus { device, open: true, sample_rate, error: None };
                if !self.mic_open {
                    self.mic_open = true;
                }
                self.publish();
            }
            AudioEv::Closed => {
                self.mic.open = false;
                self.publish();
            }
            AudioEv::Error(e) => {
                self.mic.error = Some(e.clone());
                self.mic.open = false;
                self.publish();
                if self.session.is_some() {
                    self.overlay.notice(&format!("Microphone problem: {e}"), true);
                }
            }
        }
    }

    fn on_chunk(&mut self, c: Vec<f32>) {
        // Meters (overlay + subscribed UIs).
        let sum: f32 = c.iter().map(|x| x * x).sum();
        let peak = c.iter().fold(0f32, |m, x| m.max(x.abs()));
        self.level_acc.0 += sum;
        self.level_acc.1 = self.level_acc.1.max(peak);
        self.level_acc.2 += c.len();
        let rms_now = (sum / c.len().max(1) as f32).sqrt();
        self.overlay.level.set(rms_now);
        if self.last_level.elapsed() >= Duration::from_millis(40) && self.level_acc.2 > 0 {
            if self.hub.wants_levels() {
                let rms = (self.level_acc.0 / self.level_acc.2 as f32).sqrt();
                self.hub.broadcast_levels(json!({"rms": rms, "peak": self.level_acc.1}));
            }
            self.level_acc = (0.0, 0.0, 0);
            self.last_level = Instant::now();
        }

        let Some(s) = self.session.as_mut() else {
            self.ring.push(&c);
            return;
        };
        if !s.capturing || s.cancelled {
            self.ring.push(&c);
            return;
        }
        s.audio.extend_from_slice(&c);
        self.segment();
    }
}
