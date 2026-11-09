//! A managed local server (llama-server, drama): started hidden, bound to Mimic's lifetime
//! (a job object kills it even if Mimic crashes), health-checked, stopped on drop.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;

use crate::engine::phonon::pump_lines;

pub struct Server {
    name: &'static str,
    child: Child,
    pub base: String,
    tail: Arc<Mutex<Vec<String>>>,
}

pub fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

pub struct Spec<'a> {
    pub name: &'static str,
    pub exe: &'a Path,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub port: u16,
    /// Path answering 200 once the server is ready.
    pub health: &'a str,
    pub timeout: Duration,
    /// Start at low priority (loading never competes with your apps).
    pub low_priority: bool,
    /// Extracts a progress fraction from a log line.
    pub progress: Option<fn(&str) -> Option<f32>>,
}

impl Server {
    pub fn start(spec: Spec, on_progress: &dyn Fn(&str, Option<f32>)) -> Result<Self> {
        if !spec.exe.exists() {
            bail!("{} is not installed ({} is missing)", spec.name, spec.exe.display());
        }
        let mut cmd = Command::new(spec.exe);
        cmd.args(&spec.args).envs(spec.env.iter().cloned()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = spec.exe.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        crate::platform::hide_child(&mut cmd, spec.low_priority);
        let mut child = cmd.spawn().with_context(|| format!("starting {}", spec.exe.display()))?;
        #[cfg(windows)]
        crate::platform::bind_child_lifetime(&child);
        log::info!("{} starting (pid {}, port {})", spec.name, child.id(), spec.port);

        let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let last: Arc<Mutex<Option<f32>>> = Arc::new(Mutex::new(None));
        for stream in [
            child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tail = tail.clone();
            let last = last.clone();
            let name = spec.name;
            let parse = spec.progress;
            pump_lines(stream, move |l| {
                let l = l.trim();
                if l.is_empty() {
                    return;
                }
                if let Some(p) = parse.and_then(|f| f(l)) {
                    *last.lock() = Some(p);
                } else {
                    log::debug!("{name}: {l}");
                }
                let mut t = tail.lock();
                t.push(l.to_string());
                if t.len() > 16 {
                    t.remove(0);
                }
            });
        }

        let base = format!("http://127.0.0.1:{}", spec.port);
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(2))).http_status_as_error(false).build().into();
        let t = Instant::now();
        let mut me = Self { name: spec.name, child, base, tail };
        loop {
            if let Some(status) = me.child.try_wait()? {
                bail!("{} exited ({status}):\n{}", spec.name, me.tail());
            }
            if let Ok(r) = agent.get(&format!("{}{}", me.base, spec.health)).call() {
                if r.status() == 200 {
                    break;
                }
            }
            if let Some(p) = last.lock().take() {
                on_progress("Loading", Some(p));
            }
            if t.elapsed() > spec.timeout {
                bail!("{} did not start within {} s:\n{}", spec.name, spec.timeout.as_secs(), me.tail());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        #[cfg(windows)]
        if spec.low_priority {
            crate::platform::set_process_priority_normal(&me.child);
        }
        log::info!("{} ready after {:.1}s", spec.name, t.elapsed().as_secs_f32());
        Ok(me)
    }

    pub fn tail(&self) -> String {
        self.tail.lock().join("\n")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        log::info!("{} stopped", self.name);
    }
}
