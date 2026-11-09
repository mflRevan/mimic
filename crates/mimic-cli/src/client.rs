//! Talking to the background app: connect to it, start it, stop it.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use mimic_core::ipc::Client;
use serde_json::Value;

pub fn connect() -> Result<Client> {
    Client::connect().map_err(|_| anyhow!("Mimic is not running. Start it with `mimic start`."))
}

fn daemon_exe() -> std::path::PathBuf {
    let name = if cfg!(windows) { "mimic-daemon.exe" } else { "mimic-daemon" };
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).filter(|p| p.exists()).unwrap_or_else(|| name.into())
}

pub fn start_daemon() -> Result<Client> {
    if let Ok(c) = Client::connect() {
        return Ok(c);
    }
    let mut cmd = std::process::Command::new(daemon_exe());
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
        // The daemon outlives us. If it inherited our stdout/stderr (a pipe when a script
        // or the installer captures output), the reader would wait for it to exit.
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                SetHandleInformation(GetStdHandle(h), HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    cmd.spawn().with_context(|| format!("starting {}", daemon_exe().display()))?;
    Client::connect_wait(Duration::from_secs(10))
}

/// Ask the daemon to quit and wait until it is gone. Never hangs: a daemon that
/// doesn't answer is reported (the installer runs this before replacing files).
pub fn stop_daemon() -> Result<bool> {
    let Ok(c) = Client::connect() else { return Ok(false) };
    let _ = c.call_timeout("daemon.quit", Value::Null, Duration::from_secs(3));
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        if Client::connect().is_err() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("Mimic did not stop. End `mimic-daemon.exe` in Task Manager, then try again.")
}
