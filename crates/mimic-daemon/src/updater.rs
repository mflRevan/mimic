//! Update checks and in-place updates, from the repository's latest GitHub release.
//!
//! Checks run in the background (every 3 hours with `general.auto_update`, twice a day with
//! only `general.check_updates`) and on request (`mimic update`). With `auto_update` a newer
//! version is downloaded right away and installed once nothing is going on (no dictation, the
//! agent idle for a few minutes); otherwise `update.install` does it on request. Either way
//! the installer is verified (size and SHA-256 from GitHub) and run in passive update mode
//! (`/P /UPDATE`): it stops Mimic, replaces the files and starts the new version in the tray
//! (see `ui/src-tauri/installer-hooks.nsh`). A ~6 MB download and a few seconds.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossbeam_channel::Sender;
use mimic_core::protocol::UpdateStatus;
use mimic_core::update::{self, Manifest};
use sha2::{Digest, Sha256};

use crate::app::Ev;
use crate::ipc_server::Replier;
use crate::lifecycle::State;

const INTERVAL: Duration = Duration::from_secs(12 * 3600);
const AUTO_INTERVAL: Duration = Duration::from_secs(3 * 3600);
const RETRY: Duration = Duration::from_secs(3600);
/// After startup, so a sign-in isn't slowed down by network work.
const FIRST_CHECK: Duration = Duration::from_secs(45);

pub enum UpdateEv {
    Checked(Result<Manifest, String>),
    Progress(f32),
    Downloaded(Result<PathBuf, String>),
}

pub struct Updater {
    state: State,
    checking: bool,
    error: Option<String>,
    downloading: Option<f32>,
    next_check: Instant,
    waiters: Vec<Replier>,
    /// Reopen the settings window after installing.
    reopen: bool,
    /// Downloaded installer waiting for the current dictation to end.
    ready: Option<PathBuf>,
    /// The download was started by auto-update (install only when idle).
    auto: bool,
    /// `general.auto_update` (checks every few hours).
    pub auto_pref: bool,
    updated_from: Option<String>,
    /// Version already announced during this run.
    announced: Option<String>,
}

pub enum Outcome {
    Nothing,
    /// A newer version showed up (not skipped, not yet announced).
    Available(String),
    /// This version is below the supported minimum.
    Unsupported(Option<String>),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Updater {
    pub fn new(state: State, updated_from: Option<String>) -> Self {
        // Respect the cached check across restarts instead of hitting the network every launch.
        let age = state.checked_at.map(|t| Duration::from_millis(now_ms().saturating_sub(t)));
        let next_check = match age {
            Some(a) if a < INTERVAL => Instant::now() + (INTERVAL - a).max(FIRST_CHECK),
            _ => Instant::now() + FIRST_CHECK,
        };
        Self {
            state,
            checking: false,
            error: None,
            downloading: None,
            next_check,
            waiters: Vec::new(),
            reopen: false,
            ready: None,
            auto: false,
            auto_pref: false,
            updated_from,
            announced: None,
        }
    }

    fn latest(&self) -> Option<&Manifest> {
        self.state.manifest.as_ref()
    }

    pub fn status(&self, auto: bool) -> UpdateStatus {
        let current = mimic_core::VERSION;
        let m = self.latest();
        let available = m.is_some_and(|m| m.is_newer_than(current));
        UpdateStatus {
            current: current.into(),
            latest: m.map(|m| m.version.clone()),
            available,
            required: m.is_some_and(|m| m.unsupported(current)),
            skipped: available && self.state.skipped.as_deref() == m.map(|m| m.version.as_str()),
            message: m.and_then(|m| m.message.clone()).filter(|s| !s.is_empty()),
            notes_url: m.and_then(|m| m.notes.clone()),
            checked_at: self.state.checked_at,
            checking: self.checking,
            downloading: self.downloading,
            error: self.error.clone(),
            auto,
            updated_from: self.updated_from.clone(),
        }
    }

    /// Time for a background check?
    pub fn due(&self, auto: bool) -> bool {
        auto && !self.checking && self.downloading.is_none() && self.ready.is_none() && Instant::now() >= self.next_check
    }

    /// Check more often when updates install themselves.
    pub fn set_auto(&mut self, auto: bool) {
        if auto && self.next_check > Instant::now() + AUTO_INTERVAL {
            self.next_check = Instant::now() + AUTO_INTERVAL;
        }
    }

    /// Start a check; `reply` (if any) gets the status once it finished.
    pub fn check(&mut self, tx: &Sender<Ev>, reply: Option<Replier>) {
        self.waiters.extend(reply);
        if self.checking {
            return;
        }
        self.checking = true;
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("mimic-update-check".into())
            .spawn(move || {
                let r = fetch().map_err(|e| format!("{e:#}"));
                let _ = tx.send(Ev::Update(UpdateEv::Checked(r)));
            })
            .ok();
    }

    /// A check finished. Returns waiters to answer and what (if anything) to tell the user.
    pub fn on_checked(&mut self, r: Result<Manifest, String>) -> (Vec<Replier>, Outcome) {
        self.checking = false;
        match r {
            Ok(m) => {
                log::info!("update check: latest {} (running {})", m.version, mimic_core::VERSION);
                self.error = None;
                self.state.manifest = Some(m);
                self.state.checked_at = Some(now_ms());
                self.state.save();
                self.next_check = Instant::now() + INTERVAL;
                self.set_auto(self.auto_pref);
            }
            Err(e) => {
                log::warn!("update check failed: {e}");
                self.error = Some(e);
                self.next_check = Instant::now() + RETRY;
            }
        }
        (std::mem::take(&mut self.waiters), self.outcome())
    }

    fn outcome(&mut self) -> Outcome {
        let st = self.status(true);
        let Some(latest) = st.latest.clone() else { return Outcome::Nothing };
        if self.announced.as_deref() == Some(latest.as_str()) {
            return Outcome::Nothing;
        }
        if st.required {
            self.announced = Some(latest);
            return Outcome::Unsupported(st.message);
        }
        if st.available && !st.skipped {
            self.announced = Some(latest.clone());
            return Outcome::Available(latest);
        }
        Outcome::Nothing
    }

    /// `version: None` clears the skip.
    pub fn skip(&mut self, version: Option<String>) {
        self.state.skipped = version;
        self.state.save();
    }

    pub fn latest_version(&self) -> Option<String> {
        self.latest().map(|m| m.version.clone())
    }

    /// Auto-update: download the latest installer now; it installs once Mimic is idle.
    pub fn install_auto(&mut self, tx: &Sender<Ev>) -> Result<String, String> {
        let r = self.install(tx, false);
        if r.is_ok() {
            self.auto = true;
        }
        r
    }

    /// The downloaded installer waits for an idle moment (auto-update), not just the end of a dictation.
    pub fn waits_for_idle(&self) -> bool {
        self.auto
    }

    /// Download and verify the latest installer in the background.
    pub fn install(&mut self, tx: &Sender<Ev>, reopen: bool) -> Result<String, String> {
        if self.downloading.is_some() || self.ready.is_some() {
            // An automatic download is under way (or done): asking for it now means install it
            // as soon as it's there, without waiting for an idle moment.
            if self.auto {
                self.auto = false;
                self.reopen |= reopen;
                return Ok(self.latest_version().unwrap_or_default());
            }
            return Err("The update is already downloading".into());
        }
        self.auto = false;
        let m = self.latest().cloned().ok_or("No update information yet. Check for updates first.")?;
        if !m.is_newer_than(mimic_core::VERSION) {
            return Err(format!("Mimic {} is up to date", mimic_core::VERSION));
        }
        if m.sha256.is_none() {
            return Err("This update can't be verified (the release has no checksum).".into());
        }
        if !cfg!(windows) {
            return Err("In-app updates are only available on Windows".into());
        }
        self.reopen = reopen;
        self.downloading = Some(0.0);
        self.error = None;
        let tx = tx.clone();
        let version = m.version.clone();
        std::thread::Builder::new()
            .name("mimic-update-download".into())
            .spawn(move || {
                let progress = |_: &str, p: Option<f32>| {
                    if let Some(p) = p {
                        let _ = tx.send(Ev::Update(UpdateEv::Progress(p)));
                    }
                };
                let r = download(&m, &progress).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Ev::Update(UpdateEv::Downloaded(r)));
            })
            .map_err(|e| e.to_string())?;
        Ok(version)
    }

    pub fn on_progress(&mut self, p: f32) {
        if self.downloading.is_some() {
            self.downloading = Some(p.clamp(0.0, 1.0));
        }
    }

    pub fn on_downloaded(&mut self, r: Result<PathBuf, String>) -> Result<(), String> {
        self.downloading = None;
        match r {
            Ok(p) => {
                self.ready = Some(p);
                Ok(())
            }
            Err(e) => {
                log::error!("update download failed: {e}");
                self.error = Some(e.clone());
                Err(e)
            }
        }
    }

    pub fn set_error(&mut self, e: String) {
        self.error = Some(e);
    }

    /// The verified installer, once nothing is in the way of running it.
    pub fn take_ready(&mut self) -> Option<(PathBuf, bool)> {
        self.ready.take().map(|p| (p, self.reopen))
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).user_agent(format!("Mimic/{}", mimic_core::VERSION)).build().into()
}

/// Updates come from a public repository and need no login. `MIMIC_GITHUB_TOKEN` is only for
/// builds that update from a private fork.
fn github_token() -> Option<String> {
    std::env::var("MIMIC_GITHUB_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The latest release (or the `MIMIC_UPDATE_URL` feed).
fn fetch() -> Result<Manifest> {
    if let Some(url) = update::manifest_url() {
        let body = agent().get(&url).call()?.body_mut().read_to_string()?;
        let m: Manifest = serde_json::from_str(&body).context("update feed is not valid JSON")?;
        m.validate()?;
        return Ok(m);
    }
    let mut req = agent().get(update::GITHUB_LATEST_URL).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
    let token = github_token();
    if let Some(t) = &token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) if token.is_none() => {
            bail!("no release found (a private fork needs MIMIC_GITHUB_TOKEN)")
        }
        Err(ureq::Error::StatusCode(404)) => bail!("no release published yet"),
        Err(e) => return Err(e.into()),
    };
    let body = resp.body_mut().read_to_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let m = Manifest::from_github(&v).context("the latest release has no Windows installer")?;
    m.validate()?;
    Ok(m)
}

/// Where to download the installer from: a private repository's asset goes through the API
/// (authenticated), which redirects to a short-lived signed URL that must be fetched *without*
/// the token.
fn resolve_download(m: &Manifest) -> Result<String> {
    let (Some(api), Some(token)) = (m.api_url.as_deref(), github_token()) else { return Ok(m.url.clone()) };
    let a: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(format!("Mimic/{}", mimic_core::VERSION))
        .max_redirects(0)
        .build()
        .into();
    let resp = a.get(api).header("Accept", "application/octet-stream").header("Authorization", &format!("Bearer {token}")).call()?;
    match resp.headers().get("location").and_then(|l| l.to_str().ok()) {
        Some(loc) if loc.starts_with("https://") => Ok(loc.to_string()),
        _ => Ok(m.url.clone()),
    }
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn download(m: &Manifest, progress: crate::provision::Progress) -> Result<PathBuf> {
    let want = m.sha256.as_deref().context("no checksum")?.to_lowercase();
    let dest = mimic_core::paths::updates_dir().join(format!("Mimic_{}_x64-setup.exe", m.version));
    // A previous download may already be here; trust it only if it verifies.
    for attempt in 0..2 {
        let url = resolve_download(m)?;
        crate::provision::download(&url, &dest, "Downloading update", 0.0, 1.0, progress)?;
        let size_ok = m.size.is_none_or(|s| std::fs::metadata(&dest).map(|md| md.len() == s).unwrap_or(false));
        if size_ok && sha256_file(&dest)? == want {
            log::info!("update {} downloaded and verified: {}", m.version, dest.display());
            return Ok(dest);
        }
        let _ = std::fs::remove_file(&dest);
        if attempt == 1 {
            bail!("the downloaded update didn't match its checksum; nothing was installed");
        }
        log::warn!("update {}: checksum mismatch, downloading again", m.version);
    }
    unreachable!()
}

/// Run the installer in passive update mode. It stops Mimic, so the caller quits right after.
pub fn launch_installer(path: &Path, reopen: bool) -> Result<()> {
    let mut args = vec!["/P", "/UPDATE"];
    if reopen {
        args.push("/R"); // the installer starts the settings app when it's done
    }
    crate::platform::spawn_independent(path, &args).with_context(|| format!("starting {}", path.display()))?;
    log::info!("started update installer {} {}", path.display(), args.join(" "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes() {
        let p = std::env::temp_dir().join(format!("mimic-sha-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn status_logic() {
        let m =
            |v: &str, min: Option<&str>| Manifest { version: v.into(), url: "https://x".into(), min_supported: min.map(String::from), ..Default::default() };
        let mut u = Updater::new(State::default(), None);
        assert!(!u.status(true).available);
        assert!(matches!(u.on_checked(Ok(m("999.0.0", None))).1, Outcome::Available(_)));
        assert!(u.status(true).available);
        // Announced once per run.
        assert!(matches!(u.on_checked(Ok(m("999.0.0", None))).1, Outcome::Nothing));
        u.skip(Some("999.0.1".into()));
        assert!(matches!(u.on_checked(Ok(m("999.0.1", None))).1, Outcome::Nothing));
        assert!(u.status(true).skipped);
        assert!(matches!(u.on_checked(Ok(m("999.0.2", Some("999.0.0")))).1, Outcome::Unsupported(_)));
        assert!(u.status(true).required);
        let e = u.on_checked(Err("offline".into()));
        assert!(matches!(e.1, Outcome::Nothing));
        assert_eq!(u.status(true).error.as_deref(), Some("offline"));
        assert_eq!(u.status(true).latest.as_deref(), Some("999.0.2"), "a failed check keeps the last result");
    }
}
