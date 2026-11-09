//! `mimic logs`: show or follow the daemon log file.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};

pub fn run(follow: bool, path: bool, lines: usize) -> Result<()> {
    let p = mimic_core::paths::daemon_log();
    if path {
        println!("{}", p.display());
        return Ok(());
    }
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let all: Vec<&str> = text.lines().collect();
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("{l}");
    }
    if follow {
        let mut len = text.len() as u64;
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let now = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if now > len {
                let t = std::fs::read(&p)?;
                print!("{}", String::from_utf8_lossy(&t[len as usize..]));
                let _ = std::io::stdout().flush();
            }
            len = now;
        }
    }
    Ok(())
}
