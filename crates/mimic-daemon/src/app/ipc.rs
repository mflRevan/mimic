//! The IPC method dispatcher (`on_ipc`): every request from the UI and CLI.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::config::Config;
use mimic_core::models;
use mimic_core::rules::{Rule, RuleSet};
use mimic_core::text::{self, Context, Transcript};
use serde_json::{json, Value};

use crate::agent::Msg as AgentMsg;
use crate::dsp::{self, SR};
use crate::engine::{self, DecodeOpts, EngineCmd, Job, JobKind};
use crate::hotkeys;
use crate::ipc_server::IpcCall;
use crate::platform;

use super::{App, Mode};

impl App {
    // ------------------------------------------------------------- IPC

    pub(super) fn on_ipc(&mut self, call: IpcCall) {
        if call.method.starts_with("agent.") {
            self.agent.send(AgentMsg::Ipc(call));
            return;
        }
        match call.method.as_str() {
            "soundboard.play" => {
                let slot = call.params.get("slot").and_then(Value::as_u64).unwrap_or(0) as usize;
                let r = self.soundboard.toggle(&self.cfg.soundboard, slot.saturating_sub(1));
                return match r {
                    Ok(started) => call.reply.ok(json!({"playing": started.is_some()})),
                    Err(e) => call.reply.err(&format!("{e:#}")),
                };
            }
            "soundboard.status" => {
                return call.reply.ok(json!({"playing": self.soundboard.playing(), "config": self.cfg.soundboard}));
            }
            "soundboard.stop" => {
                self.soundboard.stop();
                return call.reply.ok(json!({}));
            }
            // A file picked in the app: copied into `<data dir>/sounds` and put in the slot.
            "soundboard.import" => {
                let slot = call.params.get("slot").and_then(Value::as_u64).unwrap_or(0) as usize;
                let name = call.params.get("name").and_then(Value::as_str).unwrap_or("sound.wav").to_string();
                let data = call.params.get("data").and_then(Value::as_str).unwrap_or("");
                let r = crate::soundboard::import(slot, &name, data).and_then(|path| {
                    let mut next = self.cfg.clone();
                    let s = next.soundboard.slots.get_mut(slot - 1).ok_or_else(|| anyhow::anyhow!("no slot {slot}"))?;
                    let old = std::mem::replace(&mut s.file, path.display().to_string());
                    if s.label.trim().is_empty() {
                        s.label = std::path::Path::new(&name).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    }
                    crate::soundboard::forget(&old, &path);
                    self.apply_config(next).map_err(|e| anyhow::anyhow!(e))?;
                    Ok(path)
                });
                return match r {
                    Ok(p) => call.reply.ok(json!({"file": p.display().to_string()})),
                    Err(e) => call.reply.err(&format!("{e:#}")),
                };
            }
            _ => {}
        }
        let IpcCall { method, params, reply } = call;
        let p = |k: &str| params.get(k).cloned().unwrap_or(Value::Null);
        let ps = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        match method.as_str() {
            "ping" => reply.ok(json!("pong")),
            "status" => reply.ok(serde_json::to_value(self.status()).unwrap_or_default()),
            "subscribe" => {
                let levels = params.get("levels").and_then(|v| v.as_bool()).unwrap_or(false);
                reply.subscribe(levels);
                if levels && !self.mic_open && self.session.is_none() {
                    self.open_mic();
                    if !self.cfg.audio.keep_mic_open {
                        self.mic_close_at = Some(Instant::now() + Duration::from_secs(120));
                    }
                }
                reply.ok(json!(true));
                self.publish();
            }
            "config.get" => match self.cfg.get_path(&ps("path")) {
                Ok(v) => reply.ok(v),
                Err(e) => reply.err(&format!("{e:#}")),
            },
            "config.set" => {
                let mut next = self.cfg.clone();
                match next.set_path(&ps("path"), p("value")) {
                    Ok(()) => match self.apply_config(next) {
                        Ok(()) => reply.ok(self.cfg.get_path(&ps("path")).unwrap_or_default()),
                        Err(e) => reply.err(&e),
                    },
                    Err(e) => reply.err(&format!("{e:#}")),
                }
            }
            "config.merge" => {
                let mut next = self.cfg.clone();
                match next.merge(&p("patch")) {
                    Ok(()) => match self.apply_config(next) {
                        Ok(()) => reply.ok(self.cfg.to_value()),
                        Err(e) => reply.err(&e),
                    },
                    Err(e) => reply.err(&format!("{e:#}")),
                }
            }
            "config.reset" => {
                let mut next = Config::default();
                next.general.onboarded = self.cfg.general.onboarded;
                match self.apply_config(next) {
                    Ok(()) => reply.ok(self.cfg.to_value()),
                    Err(e) => reply.err(&e),
                }
            }
            "rules.list" => {
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(500) as usize;
                reply.ok(serde_json::to_value(self.rules.search(&ps("query"), limit)).unwrap_or_default());
            }
            "rules.upsert" => {
                let target = ps("target");
                if target.trim().is_empty() {
                    return reply.err("the replacement text is empty");
                }
                let variants: Vec<String> = params
                    .get("variants")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let out = self.rules.upsert(&target, &variants);
                self.rules_changed();
                if params.get("apply_last").and_then(|v| v.as_bool()).unwrap_or(false) {
                    if hotkeys::EPOCH_FROZEN.load(Ordering::SeqCst) {
                        self.apply_after_popup = true;
                    } else {
                        self.apply_at = Some(Instant::now());
                    }
                }
                self.overlay.notice(&format!("Saved: {}", out.rule.target), false);
                reply.ok(serde_json::to_value(out).unwrap_or_default());
            }
            "rules.replace" => {
                let rule: Result<Rule, _> = serde_json::from_value(p("rule"));
                match rule {
                    Ok(r) => {
                        self.rules.replace(&ps("old_target"), r);
                        self.rules_changed();
                        reply.ok(json!(true));
                    }
                    Err(e) => reply.err(&format!("invalid rule: {e}")),
                }
            }
            "rules.delete" => {
                let ok = self.rules.remove(&ps("target"));
                self.rules_changed();
                reply.ok(json!(ok));
            }
            "rules.delete_variant" => {
                let ok = self.rules.remove_variant(&ps("target"), &ps("variant"));
                self.rules_changed();
                reply.ok(json!(ok));
            }
            "rules.preview" => {
                let tr = Transcript::from_text(&ps("text"), 1.0);
                let out = text::process(&tr.words, &self.cfg.text, Some(&self.matcher), Context { sentence_start: true, is_final: true });
                reply.ok(json!({"text": out.text, "rules": out.rules_hit}));
            }
            "rules.import" => {
                let r: Result<RuleSet, _> = serde_json::from_value(p("rules"));
                match r {
                    Ok(set) => {
                        let n = set.rules.len();
                        for rule in set.rules {
                            self.rules.upsert(&rule.target, &rule.variants);
                        }
                        self.rules_changed();
                        reply.ok(json!({"imported": n}));
                    }
                    Err(e) => reply.err(&format!("invalid rules file: {e}")),
                }
            }
            "model.catalog" => {
                let list: Vec<Value> = models::CATALOG
                    .iter()
                    .map(|m| {
                        let mut v = serde_json::to_value(m).unwrap_or_default();
                        v["installed"] = json!(engine::installed(m.id));
                        v["active"] = json!(m.id == self.cfg.model.id);
                        v["installing"] = json!(self.installing.as_deref() == Some(m.id));
                        v
                    })
                    .collect();
                reply.ok(json!(list));
            }
            "model.use" => {
                let id = ps("id");
                if models::find(&id).is_none() {
                    return reply.err(&format!("unknown model `{id}`"));
                }
                let mut next = self.cfg.clone();
                next.model.id = id;
                match self.apply_config(next) {
                    Ok(()) => reply.ok(json!(true)),
                    Err(e) => reply.err(&e),
                }
            }
            "model.install" => {
                let id = Some(ps("id")).filter(|s| !s.is_empty()).unwrap_or_else(|| self.cfg.model.id.clone());
                match self.start_install(id) {
                    Ok(()) => reply.ok(json!(true)),
                    Err(e) => reply.err(&e),
                }
            }
            "model.load" => {
                let _ = self.engine.send(EngineCmd::Load { urgent: true });
                reply.ok(json!(true));
            }
            "model.unload" => {
                let _ = self.engine.send(EngineCmd::Unload);
                reply.ok(json!(true));
            }
            "devices.list" => reply.ok(serde_json::to_value(crate::audio::list_devices()).unwrap_or_default()),
            "dictation.start" => {
                if self.session.is_none() {
                    let hf = params.get("hands_free").and_then(|v| v.as_bool()).unwrap_or(true);
                    self.start_session(if hf { Mode::HandsFree } else { Mode::Hold });
                }
                reply.ok(json!(true));
            }
            "dictation.stop" => {
                self.release();
                reply.ok(json!(true));
            }
            "dictation.cancel" => {
                if self.session.is_some() {
                    self.cancel(false);
                }
                reply.ok(json!(true));
            }
            "transcribe.file" => {
                let path = std::path::PathBuf::from(ps("path"));
                match dsp::read_wav(&path) {
                    Ok(pcm) => {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.file_jobs.insert(id, reply);
                        let job = Job { id, session: 0, phrase: 0, kind: JobKind::File, audio: Arc::new(pcm), range: (0, 0), opts: self.decode_opts() };
                        let _ = self.engine.send(EngineCmd::Job(job));
                    }
                    Err(e) => reply.err(&format!("{e:#}")),
                }
            }
            "history.list" => reply.ok(serde_json::to_value(&self.history).unwrap_or_default()),
            "history.clear" => {
                self.history.clear();
                reply.ok(json!(true));
            }
            "popup.context" => {
                let finishing = self.session.as_ref().is_some_and(|s| s.add_rule_after);
                if finishing {
                    self.popup_reply.push(reply);
                } else {
                    reply.ok(serde_json::to_value(self.popup_context()).unwrap_or_default());
                }
            }
            "popup.open" => {
                self.open_rule_popup();
                reply.ok(json!(true));
            }
            "popup.closed" => {
                hotkeys::EPOCH_FROZEN.store(false, Ordering::SeqCst);
                if std::mem::take(&mut self.apply_after_popup) {
                    // Let focus settle back into the text field first.
                    self.apply_at = Some(Instant::now() + Duration::from_millis(120));
                }
                reply.ok(json!(true));
            }
            "hotkeys.record" => {
                if let Some((old, _)) = self.record.take() {
                    old.err("superseded");
                }
                hotkeys::RECORDING.store(true, Ordering::SeqCst);
                self.record = Some((reply, Instant::now()));
            }
            "hotkeys.cancel_record" => {
                if let Some((old, _)) = self.record.take() {
                    old.err("cancelled");
                }
                hotkeys::RECORDING.store(false, Ordering::SeqCst);
                reply.ok(json!(true));
            }
            "hotkeys.pause" => {
                let paused = params.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
                if !paused {
                    // The settings app uses this to abandon a shortcut recording too.
                    if let Some((old, _)) = self.record.take() {
                        old.err("cancelled");
                    }
                    hotkeys::RECORDING.store(false, Ordering::SeqCst);
                }
                hotkeys::PAUSED.store(paused, Ordering::Relaxed);
                reply.ok(json!(true));
                self.publish();
            }
            "endpoint.test" => {
                let cfg = self.cfg.custom.clone();
                std::thread::spawn(move || {
                    let t = Instant::now();
                    let res = crate::engine::openai::OpenAi::new(&cfg).and_then(|mut e| {
                        use crate::engine::Engine;
                        e.transcribe(&test_tone(), &DecodeOpts::default())
                    });
                    match res {
                        Ok(tr) => reply.ok(json!({"ok": true, "latency_ms": t.elapsed().as_millis() as u64, "text": tr.text})),
                        Err(e) => reply.err(&format!("{e:#}")),
                    }
                });
            }
            "autostart.set" => {
                let mut next = self.cfg.clone();
                next.general.autostart = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                match self.apply_config(next) {
                    Ok(()) => reply.ok(json!(true)),
                    Err(e) => reply.err(&e),
                }
            }
            "ui.open" => {
                let page = ps("page");
                if page.is_empty() {
                    platform::open_ui(&[]);
                } else {
                    platform::open_ui(&["--page", &page]);
                }
                reply.ok(json!(true));
            }
            "logs.path" => reply.ok(json!(mimic_core::paths::logs_dir().display().to_string())),
            "update.status" => reply.ok(serde_json::to_value(self.updater.status(self.updates_on())).unwrap_or_default()),
            "update.check" => {
                self.updater.check(&self.tx, Some(reply));
                self.publish();
            }
            "update.install" => {
                let reopen = params.get("reopen").and_then(|v| v.as_bool()).unwrap_or(false);
                match self.updater.install(&self.tx, reopen) {
                    Ok(v) => {
                        reply.ok(json!({ "version": v }));
                        self.publish();
                    }
                    Err(e) => reply.err(&e),
                }
            }
            "update.skip" => {
                // No `version` skips the latest; `version: null` clears the skip.
                let v = match params.get("version") {
                    Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => self.updater.latest_version(),
                };
                self.updater.skip(v);
                reply.ok(json!(true));
                self.publish();
            }
            "daemon.quit" => {
                reply.ok(json!(true));
                self.quit = true;
            }
            other => reply.err(&format!("unknown method `{other}`")),
        }
    }
}

/// One second of a soft vowel-like tone for endpoint tests (silence makes some servers 400).
fn test_tone() -> Vec<f32> {
    (0..SR)
        .map(|i| {
            let t = i as f32 / SR as f32;
            0.05 * ((2.0 * std::f32::consts::PI * 220.0 * t).sin() + 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin())
        })
        .collect()
}
