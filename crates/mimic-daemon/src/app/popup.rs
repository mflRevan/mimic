//! The add-a-rule popup and applying the last fix.

use std::sync::atomic::Ordering;
use std::time::Duration;

use mimic_core::chunk::{self, ChunkLimits};
use mimic_core::protocol::*;
use mimic_core::text;

use crate::hotkeys;
use crate::inject::Edit;
use crate::platform;

use super::App;

impl App {
    // ------------------------------------------------------------- rule popup

    pub(super) fn popup_context(&self) -> PopupContext {
        let fresh = self.last.as_ref().filter(|l| l.at.elapsed() < Duration::from_secs(600));
        match fresh {
            Some(l) => PopupContext {
                chunk: chunk::guess(&l.words, Some(&l.env), ChunkLimits::default()),
                return_hwnd: self.popup_return,
                from_dictation: true,
                can_apply_last: self.continuation_valid(&l.ctx),
            },
            None => PopupContext { return_hwnd: self.popup_return, ..Default::default() },
        }
    }

    pub(super) fn open_rule_popup(&mut self) {
        self.popup_return = platform::foreground_window();
        hotkeys::EPOCH_FROZEN.store(true, Ordering::SeqCst);
        // Ctrl+Win+R from idle starts a push-to-talk hold first; with no speech yet,
        // drop it so the popup opens right away with the previous dictation.
        let empty = self.session.as_ref().is_some_and(|s| s.phrases.is_empty() && s.open.voiced_blocks < 2);
        if empty {
            self.cancel(true);
        }
        if let Some(s) = self.session.as_mut() {
            s.add_rule_after = true;
            if s.released.is_none() {
                self.release();
            }
        }
        platform::open_ui(&["--popup"]);
        if self.session.is_none() {
            let ctx = self.popup_context();
            self.hub.broadcast("popup", serde_json::to_value(&ctx).unwrap_or_default());
        }
    }

    pub(super) fn reply_popup(&mut self) {
        let ctx = self.popup_context();
        let v = serde_json::to_value(&ctx).unwrap_or_default();
        for r in self.popup_reply.drain(..) {
            r.ok(v.clone());
        }
        self.hub.broadcast("popup", v);
    }

    /// Re-run the last phrase through the pipeline with the new rules and fix it in place.
    pub(super) fn apply_last_fix(&mut self) {
        let Some(l) = self.last.as_mut() else { return };
        let now = platform::focus_ctx();
        let test = std::env::var_os("MIMIC_TEST_SINK").is_some();
        if !test && (l.ctx != now || l.epoch != hotkeys::epoch()) {
            log::debug!("not fixing last dictation in place: focus or text changed");
            return;
        }
        let out = text::process(&l.words, &self.cfg.text, Some(&self.matcher), l.flags);
        if out.text == l.text {
            return;
        }
        let old: Vec<char> = l.text.chars().collect();
        let new: Vec<char> = out.text.chars().collect();
        let common = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let edit = Edit {
            backspaces: old.len() - common,
            text: new[common..].iter().collect(),
            method: self.cfg.output.method,
            newline: self.cfg.output.newline,
            tag: 0,
        };
        l.text = out.text;
        l.auto_tail = out.auto_tail;
        let _ = self.inject.send(edit);
        self.overlay.notice("Fixed", false);
    }
}
