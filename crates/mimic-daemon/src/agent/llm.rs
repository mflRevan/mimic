//! The "brain": any OpenAI-compatible chat endpoint, or a managed `llama-server` running a small
//! local model (Gemma 4 E4B: multilingual, ~8 GB VRAM, thinking off).

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use mimic_core::agent::stream::{ChatAccumulator, SseEvent, SseParser};
use mimic_core::agent::{LlmCfg, LlmProvider};
use serde_json::{json, Value};

use super::server::{self, Server};

pub const LOCAL_MODEL_DIR: &str = "gemma-4-e4b";
pub const LOCAL_MODEL_FILE: &str = "gemma-4-E4B-it-Q8_0.gguf";
pub const LOCAL_NAME: &str = "Gemma 4 E4B (local)";

/// Where a request goes.
#[derive(Clone)]
pub struct Client {
    base: String,
    key: String,
    model: String,
    local: bool,
    temperature: f32,
    max_tokens: u32,
    http: ureq::Agent,
    pub name: String,
}

/// Cancels in-flight requests: a request remembers the generation it started in.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicU64>);

impl Cancel {
    pub fn token(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self, token: u64) -> bool {
        self.0.load(Ordering::SeqCst) != token
    }
}

impl Client {
    pub fn new(cfg: &LlmCfg, local_base: Option<&str>) -> Result<Self> {
        let http: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(cfg.timeout_s.max(5) as u64))).http_status_as_error(false).build().into();
        let (base, key, model, local, name) = match cfg.provider {
            LlmProvider::Local => {
                let b = local_base.ok_or_else(|| anyhow!("the local model is not running"))?;
                (format!("{b}/v1"), String::new(), "local".to_string(), true, LOCAL_NAME.to_string())
            }
            LlmProvider::Custom => {
                let b = cfg.base_url.trim().trim_end_matches('/').to_string();
                if b.is_empty() {
                    bail!("set agent.llm.base_url for the custom endpoint");
                }
                let host = b.split("://").nth(1).unwrap_or(&b).split('/').next().unwrap_or("").to_string();
                let name = if cfg.model.is_empty() { host } else { format!("{} @ {host}", cfg.model) };
                (b, cfg.api_key.clone(), cfg.model.clone(), false, name)
            }
        };
        Ok(Self { base, key, model, local, temperature: cfg.temperature, max_tokens: cfg.max_tokens, http, name })
    }

    fn body(&self, messages: &[Value], tools: &[Value], stream: bool) -> Value {
        let mut b = json!({
            "messages": messages,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": stream,
        });
        if !self.model.is_empty() {
            b["model"] = json!(self.model);
        }
        if !tools.is_empty() {
            b["tools"] = json!(tools);
        }
        if self.local {
            // llama-server: reuse the KV cache of the shared prompt prefix between turns.
            b["cache_prompt"] = json!(true);
            // The model thinks by default; a voice agent can't wait for that.
            b["chat_template_kwargs"] = json!({"enable_thinking": false});
            // Gemma 4's recommended sampling (model card): temperature 1.0, top_p 0.95, top_k 64.
            b["top_p"] = json!(0.95);
            b["top_k"] = json!(64);
        }
        b
    }

    /// Streamed chat completion. `on_text` gets each new piece of text and returns false to stop.
    pub fn chat(&self, messages: &[Value], tools: &[Value], cancel: &Cancel, mut on_text: impl FnMut(&str) -> bool) -> Result<ChatAccumulator> {
        let token = cancel.token();
        let body = self.body(messages, tools, true);
        let mut req = self.http.post(&format!("{}/chat/completions", self.base)).header("Content-Type", "application/json");
        if !self.key.is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", self.key));
        }
        let resp = req.send(serde_json::to_vec(&body)?.as_slice())?;
        let status = resp.status();
        let mut reader = resp.into_body().into_reader();
        if status != 200 {
            let mut s = String::new();
            let _ = reader.take(4000).read_to_string(&mut s);
            bail!("{} answered {status}: {}", self.name, s.trim());
        }
        let mut acc = ChatAccumulator::default();
        let mut sse = SseParser::default();
        let mut events = Vec::new();
        let mut buf = [0u8; 4096];
        'read: loop {
            if cancel.is_cancelled(token) {
                break;
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            events.clear();
            sse.feed(&buf[..n], &mut events);
            for e in events.drain(..) {
                match e {
                    SseEvent::Done => break 'read,
                    SseEvent::Data(d) => {
                        let Ok(v) = serde_json::from_str::<Value>(&d) else {
                            continue;
                        };
                        if let Some(err) = v.get("error") {
                            bail!("{}: {}", self.name, err);
                        }
                        let new = acc.apply(&v);
                        if !new.is_empty() && !on_text(&new) {
                            break 'read;
                        }
                    }
                }
            }
        }
        Ok(acc)
    }
}

// ------------------------------------------------------------------ local model

pub fn llama_exe() -> PathBuf {
    mimic_core::paths::runtime_dir().join("llama").join(if cfg!(windows) { "llama-server.exe" } else { "llama-server" })
}

pub fn local_model(cfg: &LlmCfg) -> PathBuf {
    if cfg.local_model.trim().is_empty() {
        mimic_core::paths::models_dir().join(LOCAL_MODEL_DIR).join(LOCAL_MODEL_FILE)
    } else {
        PathBuf::from(cfg.local_model.trim())
    }
}

pub fn local_installed(cfg: &LlmCfg) -> bool {
    llama_exe().exists() && local_model(cfg).exists()
}

/// Start `llama-server` with the local model on the GPU.
pub fn start_local(cfg: &LlmCfg, progress: &dyn Fn(&str, Option<f32>)) -> Result<Server> {
    let model = local_model(cfg);
    if !model.exists() {
        bail!("the local model is not downloaded ({})", model.display());
    }
    let port = server::free_port()?;
    let args: Vec<String> = vec![
        "-m".into(),
        model.display().to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-ngl".into(),
        "99".into(),
        "-c".into(),
        cfg.local_ctx.max(2048).to_string(),
        // One conversation at a time: the smallest KV cache, and prompt caching across turns.
        "-np".into(),
        "1".into(),
        "--reasoning".into(),
        "off".into(),
        "--jinja".into(),
        "--no-webui".into(),
    ];
    progress("Starting the local model", None);
    let s = Server::start(
        server::Spec {
            name: "llama-server",
            exe: &llama_exe(),
            args,
            env: vec![],
            port,
            health: "/health",
            timeout: Duration::from_secs(180),
            low_priority: true,
            progress: None,
        },
        progress,
    )?;
    // Warm up so the first real turn doesn't pay for graph setup.
    let mut warm = cfg.clone();
    warm.max_tokens = 1;
    if let Ok(c) = Client::new(&warm, Some(&s.base)) {
        let _ = c.chat(&[json!({"role": "user", "content": "hi"})], &[], &Cancel::default(), |_| true);
    }
    Ok(s)
}
