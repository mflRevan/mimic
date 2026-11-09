//! Thinking and speaking: what you say privately is routed deterministically when it can be
//! (actions, context edits) and goes to the model only otherwise; the context document is
//! kept on disk (`context.md`, editable by hand) and its summary is maintained by the model;
//! a reply is spoken as one utterance, and if someone talks over it, it finishes its sentence
//! (or stops) and the transcript keeps exactly what was said.

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use mimic_core::agent::prompt::{self, Context};
use mimic_core::agent::route::{self, Route};
use mimic_core::agent::stream::{self, SentenceSplitter};
use mimic_core::agent::{BargeIn, ContextDoc, Line, Section, Who};

use super::speaker::SpeakerEv;
use super::{llm, local_time, now_ms, tools, Agent, Barge, LlmEv, Msg, Utterance};
use crate::hotkeys::AgentKey;
use crate::overlay::OvState;

pub(super) fn context_path() -> PathBuf {
    mimic_core::paths::data_dir().join("context.md")
}

/// The context as last saved. The conversation part is dropped if it was last touched more than
/// `forget_after_min` ago (Mimic was closed or idle meanwhile); what you said about yourself stays.
pub(super) fn load_context(forget_after_min: u32) -> (ContextDoc, Option<SystemTime>) {
    let p = context_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            let mut doc = ContextDoc::from_markdown(&s);
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            let age = mtime.and_then(|t| t.elapsed().ok()).unwrap_or_default();
            if forget_after_min > 0 && age > std::time::Duration::from_secs(forget_after_min as u64 * 60) {
                doc.new_session();
            }
            (doc, mtime)
        }
        Err(_) => (ContextDoc::new(), None),
    }
}

impl Agent {
    // ------------------------------------------------------------- context

    pub(super) fn save_context(&mut self) {
        let p = context_path();
        if let Err(e) = mimic_core::paths::write_atomic(&p, self.ctx.to_markdown().as_bytes()) {
            log::warn!("saving the context: {e}");
        }
        self.ctx_mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        self.publish(true);
    }

    /// You edited `context.md` by hand: take it.
    pub(super) fn reload_context_if_edited(&mut self) {
        let p = context_path();
        let m = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        if m.is_some() && m != self.ctx_mtime {
            if let Ok(s) = std::fs::read_to_string(&p) {
                log::info!("context.md changed on disk: reloaded");
                self.ctx = ContextDoc::from_markdown(&s);
                self.ctx_mtime = m;
                self.publish(true);
            }
        }
    }

    pub(super) fn context_add(&mut self, s: Section, text: &str) {
        self.last_activity = Instant::now();
        if self.ctx.add(s, text) {
            self.overlay.notice(&format!("{}: {}", s.title(), text.trim()), false);
            self.push_line(now_ms(), Who::Note, "", &format!("{}: {}", s.title(), text.trim()));
            self.save_context();
        }
    }

    pub(super) fn new_session(&mut self) {
        self.forget_conversation();
        self.overlay.notice("New conversation", false);
    }

    /// Drop the conversation: transcript, summary and the conversation part of the context.
    pub(super) fn forget_conversation(&mut self) {
        self.ctx.new_session();
        self.timeline.clear();
        self.summarized_upto = 0;
        self.save_context();
        self.hub.broadcast("agent.lines", serde_json::json!([]));
    }

    // ------------------------------------------------------------- hotkeys

    pub(super) fn on_hotkey(&mut self, k: AgentKey, down: bool) {
        if !self.enabled {
            if down {
                self.overlay.notice("The agent is off (turn it on in Mimic)", true);
            }
            return;
        }
        match (k, down) {
            (AgentKey::Talk, true) => {
                if !self.talking {
                    self.talking = true;
                    self.talk_released = false;
                    self.talk_texts.clear();
                    let pos = self.mic_seg.pos();
                    self.talk.set(true, pos);
                    self.reply_due = None;
                    self.overlay.show(OvState::Listening);
                    self.overlay.text("Talking to Mimic");
                }
            }
            (AgentKey::Talk, false) => {
                if self.talking {
                    self.talking = false;
                    // The last words are in once the phrase is closed now.
                    self.flush_mic();
                    let pos = self.mic_seg.pos();
                    self.talk.set(false, pos);
                    self.talk_released = true;
                    self.overlay.hide();
                    self.maybe_finish_talk();
                }
            }
            (AgentKey::Autopilot, true) => self.set_autopilot(!self.autopilot),
            (AgentKey::Reply, true) => self.start_reply(true, None),
            (AgentKey::Stop, true) => {
                self.interrupt(true);
                self.overlay.notice("Stopped", false);
            }
            _ => {}
        }
        self.publish(true);
    }

    pub(super) fn set_autopilot(&mut self, on: bool) {
        self.autopilot = on;
        self.reply_due = None;
        if !on {
            self.interrupt(true);
        }
        self.overlay.notice(if on { "Autopilot on: Mimic answers for you" } else { "Autopilot off" }, false);
        self.publish(true);
    }

    // ------------------------------------------------------------- private input

    /// Something you said (or typed) privately to the agent.
    pub(super) fn private(&mut self, text: String) {
        let r = route::route(&text);
        log::info!("private: {text:?} -> {r:?}");
        match r {
            // A question gets an answer (no tools: small models reach for them anyway).
            Route::Model if route::is_question(&text.to_lowercase()) => self.run_command(text, false),
            Route::Model => self.run_command(text, true),
            other => self.apply_route(other),
        }
    }

    fn apply_route(&mut self, r: Route) {
        match r {
            Route::Note(t) => match tools::append_note(&self.a().notes_dir.clone(), &t) {
                Ok(_) => self.done(&format!("Noted: {t}")),
                Err(e) => self.overlay.notice(&format!("Note: {e:#}"), true),
            },
            Route::Snapshot(label) => match self.snapshot(&label) {
                Ok(dir) => self.done(&format!("Snapshot saved ({})", dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
                Err(e) => self.overlay.notice(&format!("Snapshot: {e:#}"), true),
            },
            Route::Say(t) => self.say_now(t),
            Route::Reply(hint) => self.start_reply(true, hint),
            Route::Autopilot(on) => self.set_autopilot(on),
            Route::Stop => self.interrupt(true),
            Route::Context(s, t) => self.context_add(s, &t),
            Route::Forget(t) => {
                let n = self.ctx.forget(&t);
                self.done(&if n > 0 { format!("Forgot {t}") } else { format!("Nothing about {t}") });
                if n > 0 {
                    self.save_context();
                }
            }
            Route::NameSpeaker(target, name, about) => match self.name_speaker(target, &name) {
                Ok(msg) => {
                    if !about.is_empty() {
                        self.context_add(Section::People, &format!("{name}: {about}"));
                    }
                    self.done(&msg)
                }
                Err(e) => self.overlay.notice(&e, true),
            },
            Route::NewSession => self.new_session(),
            Route::Model => {}
        }
    }

    fn done(&mut self, msg: &str) {
        self.push_line(now_ms(), Who::Note, "", msg);
        self.overlay.notice(msg, false);
    }

    pub(super) fn push_line(&mut self, at: u64, who: Who, source: &str, text: &str) -> u64 {
        self.push_voice_line(at, who, source, None, "", text)
    }

    pub(super) fn push_voice_line(&mut self, at: u64, who: Who, source: &str, speaker: Option<u32>, name: &str, text: &str) -> u64 {
        self.last_activity = Instant::now();
        let l = self.timeline.push_voice(at, who, source, speaker, name, text, 2500).clone();
        self.hub.broadcast("agent.line", serde_json::to_value(&l).unwrap_or_default());
        l.id
    }

    // ------------------------------------------------------------- the model

    /// The verbatim conversation the model sees: what isn't in the summary yet, at most
    /// `context_lines` (older ones beyond that are dropped until the summary catches up).
    fn recent_lines(&self) -> Vec<Line> {
        let n = self.a().context_lines.max(4) as usize;
        let upto = self.summarized_upto;
        self.timeline.recent(n, |l| l.id > upto).into_iter().cloned().collect()
    }

    fn client(&mut self) -> Option<llm::Client> {
        if self.llm.is_none() {
            self.ensure_services();
        }
        if self.llm.is_none() {
            let why = self.llm_status.error.clone().unwrap_or_else(|| "the model is still loading".into());
            self.overlay.notice(&format!("Mimic can't think yet: {why}"), true);
        }
        self.llm.clone()
    }

    /// Answer the conversation in your voice. `forced`: you asked for it (never silent).
    pub(super) fn start_reply(&mut self, forced: bool, hint: Option<String>) {
        if !self.enabled {
            return;
        }
        if self.thinking || self.speaking {
            if !forced {
                return;
            }
            self.interrupt(false);
        }
        let Some(client) = self.client() else { return };
        self.reply_due = None;
        self.thinking = true;
        self.reply_t0 = Some(Instant::now());
        self.reply_must = forced;
        self.reply_buf.clear();
        self.cancel.cancel();
        let token = self.cancel.token();
        let lines = self.recent_lines();
        let doc = self.ctx.clone();
        let scenario = self.a().modes.text(self.a().mode).to_string();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let now = local_time();
        self.reply_now = now.clone();
        let max_sentences = self.a().max_sentences.max(1) as usize + usize::from(forced);
        std::thread::spawn(move || {
            let refs: Vec<&Line> = lines.iter().collect();
            let ctx = Context { scenario: &scenario, doc: &doc, lines: &refs, now: &now };
            let messages = prompt::reply_messages(&ctx, forced, hint.as_deref());
            if log::log_enabled!(log::Level::Debug) {
                for m in &messages {
                    log::debug!("reply prompt [{}]:\n{}", m["role"].as_str().unwrap_or(""), m["content"].as_str().unwrap_or(""));
                }
            }
            let mut split = SentenceSplitter::new();
            let mut said = 0usize;
            let r = client.chat(&messages, &[], &cancel, |piece| {
                for s in split.push(piece) {
                    if said < max_sentences {
                        said += 1;
                        let _ = tx.send(Msg::Llm(LlmEv::Sentence { token, text: s }));
                    }
                }
                // Enough said: stop generating.
                said < max_sentences && !cancel.is_cancelled(token)
            });
            let (text, error) = match r {
                Ok(acc) => {
                    if said < max_sentences && !stream::is_silent(&acc.content) {
                        if let Some(s) = split.finish() {
                            let _ = tx.send(Msg::Llm(LlmEv::Sentence { token, text: s }));
                        }
                    }
                    (acc.content, None)
                }
                Err(e) => (String::new(), Some(format!("{e:#}"))),
            };
            let _ = tx.send(Msg::Llm(LlmEv::ReplyDone { token, text, error }));
        });
        self.publish(true);
    }

    /// A private request the router couldn't place: the model acts on it with tools, or (for a
    /// question) just answers.
    pub(super) fn run_command(&mut self, utterance: String, with_tools: bool) {
        let Some(client) = self.client() else {
            // Without a model, keep it as a rule so nothing is lost.
            self.context_add(Section::Rules, &utterance);
            return;
        };
        let lines: Vec<Line> = self.timeline.recent(self.a().context_lines.max(4) as usize, |_| true).into_iter().cloned().collect();
        let doc = self.ctx.clone();
        let autopilot = self.autopilot;
        let scenario = self.a().modes.text(self.a().mode).to_string();
        let voices = self.recent_voices();
        let tx = self.tx.clone();
        self.thinking = true;
        std::thread::spawn(move || {
            let refs: Vec<&Line> = lines.iter().collect();
            let now = local_time();
            let ctx = Context { scenario: &scenario, doc: &doc, lines: &refs, now: &now };
            let messages = prompt::command_messages(&ctx, &utterance, autopilot, &voices);
            let tools = if with_tools { prompt::command_tools() } else { vec![] };
            let acc = client.chat(&messages, &tools, &llm::Cancel::default(), |_| true).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Llm(LlmEv::CommandDone { utterance, acc }));
        });
        self.publish(true);
    }

    /// Fold older conversation into the summary (context-language-model style: the model keeps
    /// its own context compact), when enough new lines piled up and nothing else is going on.
    pub(super) fn maybe_summarize(&mut self) {
        let every = self.a().summarize_every as usize;
        if every == 0 || self.summarizing || self.thinking || self.speaking || self.llm.is_none() {
            return;
        }
        let upto = self.summarized_upto;
        let fresh: Vec<Line> = self.timeline.recent(usize::MAX, |l| l.id > upto && l.who.is_spoken()).into_iter().cloned().collect();
        // Keep the newest lines verbatim; fold the older ones once there are enough of them.
        let keep = (self.a().context_lines as usize / 2).max(4);
        if fresh.len() < keep + every {
            return;
        }
        let older: Vec<Line> = fresh[..fresh.len() - keep].to_vec();
        let new_upto = older.last().map(|l| l.id).unwrap_or(upto);
        let Some(client) = self.llm.clone() else { return };
        self.summarizing = true;
        let doc = self.ctx.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let refs: Vec<&Line> = older.iter().collect();
            let messages = prompt::summarize_messages(&doc, &refs);
            let text = client.chat(&messages, &[], &llm::Cancel::default(), |_| true).map(|a| a.content.trim().to_string()).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Llm(LlmEv::Summary { upto: new_upto, text }));
        });
    }

    pub(super) fn on_llm(&mut self, e: LlmEv) {
        match e {
            LlmEv::Sentence { token, text } => {
                if token != self.cancel.token() {
                    return;
                }
                if self.line.is_none() {
                    // Small models sometimes parrot instead of staying quiet: drop such a reply.
                    let theirs = self.timeline.lines().rev().find(|l| l.who == Who::Them).map(|l| l.text.clone()).unwrap_or_default();
                    // ...or by starting over with what it said last time.
                    let mine = self.timeline.lines().rev().find(|l| l.who == Who::Agent && !l.text.is_empty()).map(|l| l.text.clone()).unwrap_or_default();
                    let mine_first = SentenceSplitter::new().push(&format!("{mine} ")).into_iter().next().unwrap_or_default();
                    if stream::is_echo(&text, &theirs, &self.reply_now) || stream::is_echo(&text, &mine_first, "") {
                        if self.reply_must {
                            // An answer is owed: skip the repeated sentence, keep the rest.
                            log::info!("reply: skipped an echoed sentence ({text:?})");
                        } else {
                            log::info!("reply: dropped an echo ({text:?})");
                            self.cancel.cancel();
                            self.thinking = false;
                        }
                        return;
                    }
                    if let Some(t0) = self.reply_t0 {
                        log::info!("reply: first sentence {} ms after the request", t0.elapsed().as_millis());
                    }
                    self.line = Some(self.push_line(now_ms(), Who::Agent, "", ""));
                }
                // Collected, then spoken as one utterance when the answer is complete.
                self.reply_buf.push(text);
            }
            LlmEv::ReplyDone { token, text, error } => {
                if token != self.cancel.token() {
                    return;
                }
                self.thinking = false;
                let whole = std::mem::take(&mut self.reply_buf).join(" ");
                if let Some(e) = error {
                    self.overlay.notice(&format!("Mimic: {e}"), true);
                } else if whole.trim().is_empty() && stream::is_silent(&text) {
                    log::info!("reply: stays silent");
                }
                if !whole.trim().is_empty() {
                    if let Some(t0) = self.reply_t0 {
                        log::info!("reply: complete {} ms after the request", t0.elapsed().as_millis());
                    }
                    self.speak(whole);
                }
            }
            LlmEv::CommandDone { utterance, acc } => {
                self.thinking = false;
                match acc {
                    Err(e) => {
                        self.overlay.notice(&format!("Mimic: {e}"), true);
                        self.context_add(Section::Rules, &utterance);
                    }
                    Ok(acc) => self.act(&utterance, acc),
                }
            }
            LlmEv::Summary { upto, text } => {
                self.summarizing = false;
                match text {
                    Ok(t) if !t.is_empty() && !stream::is_silent(&t) => {
                        self.summarized_upto = upto;
                        self.ctx.set(Section::Summary, &t);
                        log::info!("summary updated (lines up to {upto})");
                        self.save_context();
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("summary: {e}"),
                }
            }
        }
        self.publish(true);
    }

    fn act(&mut self, utterance: &str, acc: stream::ChatAccumulator) {
        if acc.tool_calls.is_empty() {
            // No tool: the model just answers you privately. Nothing is stored implicitly (that
            // turned every stray phrase into a "rule").
            let reply = acc.content.trim().to_string();
            if !reply.is_empty() && !stream::is_silent(&reply) {
                self.push_line(now_ms(), Who::Note, "", &reply);
                self.overlay.notice(&reply, false);
            }
            let _ = utterance;
            return;
        }
        let notes = self.a().notes_dir.clone();
        for call in acc.tool_calls {
            log::info!("tool {}({})", call.name, call.arguments);
            let mut snap = |label: &str| self.snapshot(label);
            match tools::run(&call, &notes, &mut snap) {
                Ok(tools::Action::Done(msg)) => self.done(&msg),
                Ok(tools::Action::Say(text)) => self.say_now(text),
                Ok(tools::Action::Reply(hint)) => self.start_reply(true, hint),
                Ok(tools::Action::Autopilot(on)) => self.set_autopilot(on),
                Ok(tools::Action::ContextAdd(s, t)) => self.context_add(s, if t.is_empty() { utterance } else { &t }),
                Ok(tools::Action::ContextRemove(t)) => self.apply_route(Route::Forget(t)),
                Ok(tools::Action::NameSpeaker(who, name)) => {
                    let target = self.speaker_ref(&who);
                    self.apply_route(Route::NameSpeaker(target, name, String::new()));
                }
                Err(e) => {
                    log::warn!("tool {}: {e:#}", call.name);
                    self.overlay.notice(&format!("{}: {e:#}", call.name), true);
                }
            }
        }
    }

    pub(super) fn snapshot(&self, label: &str) -> anyhow::Result<PathBuf> {
        let lines = self.timeline.recent(80, |_| true);
        let sr = crate::dsp::SR;
        let mut audio = vec![("me".to_string(), self.mic_hist.last(sr * 60), sr as u32)];
        for h in &self.hears {
            audio.push((h.label.to_lowercase().replace(' ', "-"), h.hist.last(sr * 60), sr as u32));
        }
        let context = self.ctx.to_markdown();
        tools::save_snapshot(&self.a().notes_dir, &tools::Snapshot { label, lines: &lines, context: &context, audio })
    }

    // ------------------------------------------------------------- speaking

    pub(super) fn speak(&mut self, text: String) {
        if !self.router.has_output() {
            self.overlay.notice("Nowhere to speak: set the virtual mic (or let me hear it) in Mimic", true);
            return;
        }
        if self.shared.tts.lock().is_none() && !self.tts_loading {
            // Unloaded while idle (or never started): load now; the speaker waits for it.
            self.start_tts();
        }
        let line = self.line.unwrap_or(0);
        self.speaker.say(line, text);
        self.speaking = true;
        self.tts_used = Instant::now();
    }

    /// Say exact words now (from the router, a tool, the CLI or the app).
    pub(super) fn say_now(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if self.speaking || self.thinking {
            self.interrupt(false);
        }
        self.cancel.cancel();
        self.line = Some(self.push_line(now_ms(), Who::Agent, "", ""));
        // One utterance, one request (consistent delivery).
        if let Some(t) = stream::clean_for_speech(&text) {
            self.speak(t);
        }
    }

    pub(super) fn on_speaker(&mut self, e: SpeakerEv) {
        match e {
            SpeakerEv::Started { gen, line, text } => {
                if gen != self.speaker.current() {
                    return;
                }
                // The transcript shows the words (the voice direction is performed, not said).
                let words = stream::spoken_words(&text);
                if let Some(l) = self.timeline.set_text(line, &words).cloned() {
                    self.hub.broadcast("agent.line", serde_json::to_value(&l).unwrap_or_default());
                }
                self.utter = Some(Utterance { line, words, gen });
            }
            SpeakerEv::Idle => {}
            SpeakerEv::Error(e) => self.overlay.notice(&format!("Voice: {e}"), true),
        }
    }

    /// How far playback of the current utterance is (0..1), from what was generated and what
    /// is still queued in the router.
    pub(super) fn playback_fraction(&self) -> f64 {
        let Some(u) = &self.utter else { return 0.0 };
        let p = &self.speaker.progress;
        if p.gen.load(std::sync::atomic::Ordering::SeqCst) != u.gen {
            return 0.0;
        }
        let pushed = p.pushed_s();
        let played = (pushed - self.router.pending_s() as f64).max(0.0);
        // Until generation is done, estimate the length from the words (~14 characters a second).
        let total = if p.done.load(std::sync::atomic::Ordering::SeqCst) { pushed } else { pushed.max(u.words.chars().count() as f64 / 14.0) };
        (played / total.max(0.1)).clamp(0.0, 1.0)
    }

    /// Someone started talking over the agent: finish the sentence (or stop at once).
    pub(super) fn check_barge_in(&mut self) {
        let mode = self.a().barge_in;
        // Only a live utterance (a flushed one may still show queued audio for a moment).
        if !self.speaking || self.utter.is_none() || mode == BargeIn::Off {
            self.barge = None;
            return;
        }
        if let Some(b) = &self.barge {
            if self.playback_fraction() + 1e-3 >= b.stop_at {
                log::info!("barge-in: sentence finished after {} ms, stopping", b.since.elapsed().as_millis());
                self.interrupt(true);
            }
            return;
        }
        let monitor = self.monitor_device();
        let Some(h) = self.hears.iter().find(|h| !h.echoes(&monitor) && h.seg.voiced_for(300)) else { return };
        let label = h.label.clone();
        match mode {
            BargeIn::Stop => {
                log::info!("barge-in ({label}): stopping");
                self.interrupt(true);
            }
            BargeIn::Finish => {
                let f = self.playback_fraction();
                let stop_at = self.utter.as_ref().map(|u| stream::end_of_sentence(&u.words, f)).unwrap_or(1.0);
                log::info!("barge-in ({label}) at {:.0}%: finishing the sentence (to {:.0}%)", f * 100.0, stop_at * 100.0);
                self.barge = Some(Barge { since: Instant::now(), stop_at });
            }
            BargeIn::Off => {}
        }
    }

    /// They said something while the agent was talking: a quick "mhm" or "yeah" isn't a
    /// reason to stop.
    pub(super) fn on_heard_while_speaking(&mut self, text: &str, secs: f32) {
        use mimic_core::agent::turn::{decide, Turn};
        if self.barge.is_some() && secs < 1.5 && decide(text) == Turn::Skip {
            log::info!("barge-in: just a backchannel ({text:?}), carrying on");
            self.barge = None;
        }
    }

    /// Stop thinking and speaking now. `cut`: keep in the transcript only what was said.
    pub(super) fn interrupt(&mut self, cut: bool) {
        let said = if cut {
            self.utter.as_ref().map(|u| {
                let f = self.barge.as_ref().map(|b| b.stop_at).unwrap_or_else(|| self.playback_fraction());
                (u.line, stream::spoken_prefix(&u.words, f), f < 0.999)
            })
        } else {
            None
        };
        self.cancel.cancel();
        self.reply_buf.clear();
        self.speaker.stop();
        self.router.flush();
        self.thinking = false;
        self.reply_due = None;
        self.barge = None;
        self.utter = None;
        if let Some(id) = self.line.take() {
            if let Some((line, text, partial)) = said.filter(|s| s.0 == id) {
                self.timeline.set_text(line, &text);
                if partial {
                    self.timeline.mark_interrupted(line, &text);
                }
                if let Some(l) = self.timeline.lines().find(|l| l.id == line).cloned() {
                    log::info!("agent was cut off after: {:?}", l.text);
                    self.hub.broadcast("agent.line", serde_json::to_value(&l).unwrap_or_default());
                }
            }
        }
        self.speaking = false;
    }
}
