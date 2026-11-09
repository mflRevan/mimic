//! The agent's IPC methods (`agent.*`), used by the app, the CLI and tests.

use mimic_core::agent::{ContextDoc, Section, Who};
use mimic_core::protocol::ModelPhase;
use serde_json::{json, Value};

use super::{now_ms, think, Agent};
use crate::ipc_server::IpcCall;

impl Agent {
    pub(super) fn on_ipc(&mut self, call: IpcCall) {
        let IpcCall { method, params, reply } = call;
        let ps = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let off = !self.enabled;
        match method.as_str() {
            "agent.status" => reply.ok(serde_json::to_value(self.snapshot_status()).unwrap_or_default()),
            "agent.timeline" => {
                let n = params.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
                reply.ok(json!(self.timeline.recent(n, |_| true)));
            }
            "agent.timeline.clear" => {
                self.timeline.clear();
                self.summarized_upto = 0;
                reply.ok(json!({}));
            }
            "agent.autopilot" if off => reply.err("the agent is off (mimic agent on)"),
            "agent.autopilot" => {
                let on = params.get("on").and_then(Value::as_bool).unwrap_or(!self.autopilot);
                self.set_autopilot(on);
                reply.ok(json!({"autopilot": on}));
            }
            // Private input, exactly as if spoken: routed, or handled by the model.
            "agent.tell" | "agent.command" => {
                let t = ps("text");
                self.push_line(now_ms(), Who::Private, "", &t);
                self.private(t);
                reply.ok(json!({}));
            }
            "agent.say" if off => reply.err("the agent is off"),
            "agent.say" => {
                self.say_now(ps("text"));
                reply.ok(json!({}));
            }
            "agent.reply" if off => reply.err("the agent is off"),
            "agent.reply" => {
                let hint = Some(ps("hint")).filter(|h| !h.is_empty());
                self.start_reply(true, hint);
                reply.ok(json!({}));
            }
            "agent.stop" => {
                self.interrupt(true);
                reply.ok(json!({}));
            }
            // The context document.
            "agent.context" => reply.ok(json!({"doc": self.ctx, "markdown": self.ctx.to_markdown(), "path": think::context_path().display().to_string()})),
            "agent.context.add" => match Section::parse(&ps("section")) {
                Some(s) => {
                    self.context_add(s, &ps("text"));
                    reply.ok(json!(self.ctx));
                }
                None => reply.err("unknown section (me, style, people, situation, facts, rules, summary)"),
            },
            "agent.context.set" => {
                if let Some(md) = params.get("markdown").and_then(Value::as_str) {
                    self.ctx = ContextDoc::from_markdown(md);
                } else if let Some(s) = Section::parse(&ps("section")) {
                    self.ctx.set(s, &ps("text"));
                } else {
                    return reply.err("give {markdown} or {section, text}");
                }
                self.save_context();
                reply.ok(json!(self.ctx));
            }
            "agent.context.remove" => {
                let n = match Section::parse(&ps("section")) {
                    Some(s) => self.ctx.remove(s, &ps("text")),
                    None => self.ctx.forget(&ps("text")),
                };
                self.save_context();
                reply.ok(json!({"removed": n}));
            }
            "agent.context.new" => {
                self.new_session();
                reply.ok(json!(self.ctx));
            }
            // Pretend something was heard (testing, scripting).
            "agent.inject" => {
                let text = ps("text");
                match ps("who").as_str() {
                    "me" => {
                        self.push_line(now_ms(), Who::Me, "", &text);
                    }
                    "private" => {
                        self.push_line(now_ms(), Who::Private, "", &text);
                        self.private(text);
                    }
                    _ => {
                        // Optionally as a given voice ("speaker": 2), like a labelled line from a call.
                        let source = ps("source");
                        let speaker = params.get("speaker").and_then(Value::as_u64).map(|v| v as u32);
                        let name = speaker.map(|s| self.speakers.label(s)).unwrap_or_default();
                        self.push_voice_line(now_ms(), Who::Them, &source, speaker, &name, &text);
                        if self.autopilot {
                            self.schedule_reply(&text, true);
                        }
                    }
                }
                reply.ok(json!({}));
            }
            "agent.voice.pin" => {
                let s = self.a().tts.voice_s;
                match self.shared.voice.lock().pin(s) {
                    Ok(()) => reply.ok(json!({})),
                    Err(e) => reply.err(&format!("{e:#}")),
                }
            }
            "agent.voice.unpin" => {
                self.shared.voice.lock().unpin();
                reply.ok(json!({}));
            }
            "agent.voice.clear" => {
                self.shared.voice.lock().clear();
                reply.ok(json!({}));
            }
            "agent.voice.import" => {
                let r = self.shared.voice.lock().import(std::path::Path::new(&ps("path")), &ps("text"));
                match r {
                    Ok(()) => reply.ok(json!({})),
                    Err(e) => reply.err(&format!("{e:#}")),
                }
            }
            "agent.voice.export" => {
                let s = self.a().tts.voice_s;
                let r = self.shared.voice.lock().reference(s);
                match r {
                    Some((pcm, rate, text)) => {
                        let p = std::path::PathBuf::from(ps("path"));
                        match std::fs::write(&p, crate::dsp::wav_bytes_at(&pcm, rate)) {
                            Ok(()) => reply.ok(json!({"path": p.display().to_string(), "text": text, "seconds": pcm.len() as f32 / rate as f32})),
                            Err(e) => reply.err(&e.to_string()),
                        }
                    }
                    None => reply.err("no voice recorded yet"),
                }
            }
            // The built-in texts of the three settings (to reset an edited one).
            "agent.modes.defaults" => reply.ok(serde_json::to_value(mimic_core::agent::Modes::default()).unwrap_or_default()),
            // Voices heard on calls.
            "agent.speakers" => {
                let v: Vec<Value> = self
                    .speakers
                    .voices
                    .iter()
                    .map(|v| json!({"id": v.id, "name": v.name, "label": v.label(), "seconds": v.seconds, "last_heard": v.last_heard}))
                    .collect();
                reply.ok(json!(v));
            }
            "agent.speakers.rename" => {
                let id = params.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
                match self.name_speaker(mimic_core::agent::route::SpeakerRef::Id(id), &ps("name")) {
                    Ok(m) => reply.ok(json!({"message": m})),
                    Err(e) => reply.err(&e),
                }
            }
            "agent.speakers.forget" => {
                let id = params.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
                let ok = self.speakers.forget(id);
                self.save_speakers();
                reply.ok(json!({"forgot": ok}));
            }
            "agent.speakers.clear" => {
                self.speakers = Default::default();
                self.save_speakers();
                reply.ok(json!({}));
            }
            // What can be heard / talked into.
            "agent.sources" => {
                std::thread::spawn(move || {
                    reply.ok(json!({
                        "apps": crate::audio_app::running_apps(),
                        "outputs": crate::audio::list_outputs(),
                        "inputs": crate::audio::list_devices(),
                    }))
                });
            }
            "agent.load" => {
                for s in [&mut self.llm_status, &mut self.tts_status] {
                    if s.phase == ModelPhase::Error {
                        *s = Default::default();
                    }
                }
                self.ensure_services();
                self.publish(true);
                reply.ok(json!({}));
            }
            other => reply.err(&format!("unknown method `{other}`")),
        }
    }
}
