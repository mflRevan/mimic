//! Capturing one application's audio (Windows process loopback, Windows 10 20348+ / 11): what
//! Discord plays, without the rest of the system. Stereo float at 48 kHz from WASAPI, handed on
//! as 16 kHz mono like every other stream.
//!
//! An app that plays nothing delivers no packets at all, so silence is synthesized from the
//! wall clock: phrase segmentation needs time to pass to notice a pause. If the app isn't
//! running (or restarts), capture retries every few seconds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::{AudioEv, Sink};
use crate::dsp::Resampler;

/// A running capture; dropping it stops the thread.
pub struct AppCapture {
    stop: Arc<AtomicBool>,
}

impl Drop for AppCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn spawn(exe: &str, sink: Sink) -> AppCapture {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let exe = exe.to_string();
    std::thread::Builder::new()
        .name(format!("mimic-hear-{exe}"))
        .spawn(move || {
            let mut reported_missing = false;
            while !s2.load(Ordering::Relaxed) {
                match imp::root_pid(&exe) {
                    Some(pid) => {
                        reported_missing = false;
                        sink(AudioEv::Opened { device: format!("{exe} (pid {pid})"), sample_rate: 48_000 });
                        if let Err(e) = imp::capture(pid, &sink, &s2) {
                            log::warn!("capturing {exe}: {e:#}");
                            sink(AudioEv::Error(format!("{e:#}")));
                        } else {
                            sink(AudioEv::Closed);
                        }
                    }
                    None if !reported_missing => {
                        reported_missing = true;
                        sink(AudioEv::Error(format!("{exe} is not running")));
                    }
                    None => {}
                }
                // Retry (the app may start, or have restarted).
                for _ in 0..30 {
                    if s2.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn app capture thread");
    AppCapture { stop }
}

/// Executable names (without `.exe`) of running apps with a visible window, for pickers.
pub fn running_apps() -> Vec<String> {
    imp::running_apps()
}

/// Feeds 16 kHz chunks, padding with silence when the app is quiet.
struct Feeder<'a> {
    sink: &'a Sink,
    rs: Resampler,
    out: Vec<f32>,
    start: Instant,
    /// 48 kHz frames handed on so far (real or padded).
    frames: u64,
}

impl<'a> Feeder<'a> {
    fn new(sink: &'a Sink) -> Self {
        Self { sink, rs: Resampler::new(48_000, 2), out: Vec::new(), start: Instant::now(), frames: 0 }
    }

    fn push(&mut self, interleaved: &[f32]) {
        self.frames += (interleaved.len() / 2) as u64;
        self.out.clear();
        self.rs.process(interleaved, &mut self.out);
        if !self.out.is_empty() {
            (self.sink)(AudioEv::Chunk(self.out.clone()));
        }
    }

    /// Catch up with the wall clock if the app went quiet (keeping 60 ms of slack).
    fn pad(&mut self) {
        let due = (self.start.elapsed().as_secs_f64() * 48_000.0) as u64;
        if due > self.frames + 2880 {
            let n = (due - self.frames - 960) as usize;
            let zeros = vec![0.0f32; n * 2];
            self.push(&zeros);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{anyhow, Result};
    use std::collections::VecDeque;
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};

    fn processes() -> Vec<(u32, u32, String)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                    let name = String::from_utf16_lossy(&e.szExeFile[..n]);
                    out.push((e.th32ProcessID, e.th32ParentProcessID, name));
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    fn stem(name: &str) -> String {
        name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).to_lowercase()
    }

    /// The top process of the app's tree (Discord runs several; audio comes from a child, and
    /// capturing the root's tree gets all of them).
    pub fn root_pid(exe: &str) -> Option<u32> {
        let want = stem(exe);
        let ps = processes();
        let named: Vec<&(u32, u32, String)> = ps.iter().filter(|p| stem(&p.2) == want).collect();
        named.iter().find(|p| !named.iter().any(|q| q.0 == p.1)).or(named.first()).map(|p| p.0)
    }

    pub fn running_apps() -> Vec<String> {
        let mut v: Vec<String> = crate::platform::window_pids()
            .into_iter()
            .filter_map(|pid| processes().into_iter().find(|p| p.0 == pid).map(|p| p.2))
            .map(|n| n.trim_end_matches(".exe").to_string())
            .collect();
        v.sort_by_key(|n| n.to_lowercase());
        v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        v.retain(|n| !n.eq_ignore_ascii_case("mimic-daemon") && !n.eq_ignore_ascii_case("explorer"));
        v
    }

    pub fn capture(pid: u32, sink: &Sink, stop: &AtomicBool) -> Result<()> {
        wasapi::initialize_mta().ok().map_err(|e| anyhow!("COM: {e:?}"))?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let mut client = AudioClient::new_application_loopback_client(pid, true).map_err(|e| anyhow!("{e}"))?;
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
        client.initialize_client(&format, &Direction::Capture, &mode).map_err(|e| anyhow!("{e}"))?;
        let event = client.set_get_eventhandle().map_err(|e| anyhow!("{e}"))?;
        let capture = client.get_audiocaptureclient().map_err(|e| anyhow!("{e}"))?;
        client.start_stream().map_err(|e| anyhow!("{e}"))?;
        let mut feeder = Feeder::new(sink);
        let mut bytes: VecDeque<u8> = VecDeque::new();
        let mut floats: Vec<f32> = Vec::new();
        let mut alive_check = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if let Ok(Some(n)) = capture.get_next_packet_size() {
                if n > 0 {
                    capture.read_from_device_to_deque(&mut bytes).map_err(|e| anyhow!("{e}"))?;
                }
            }
            if bytes.len() >= 8 {
                let usable = bytes.len() / 8 * 8;
                floats.clear();
                let (a, b) = bytes.as_slices();
                let mut raw: Vec<u8> = Vec::with_capacity(usable);
                raw.extend_from_slice(a);
                raw.extend_from_slice(b);
                raw.truncate(usable);
                floats.extend(raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
                bytes.drain(..usable);
                feeder.push(&floats);
            }
            feeder.pad();
            // The app may have quit: then capture just stays silent; notice and reattach.
            if alive_check.elapsed() > Duration::from_secs(5) {
                alive_check = Instant::now();
                if !processes().iter().any(|p| p.0 == pid) {
                    break;
                }
            }
            let _ = event.wait_for_event(20);
        }
        let _ = client.stop_stream();
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn root_pid(_: &str) -> Option<u32> {
        None
    }
    pub fn running_apps() -> Vec<String> {
        vec![]
    }
    pub fn capture(_: u32, _: &Sink, _: &AtomicBool) -> anyhow::Result<()> {
        anyhow::bail!("per-app capture is only available on Windows")
    }
}
