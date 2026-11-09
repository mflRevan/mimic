//! Terminal output helpers: colours (off when piped or with `NO_COLOR`), JSON printing, prompts.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use serde_json::Value;

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub fn paint(s: &str, code: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint(s, "2")
}

pub fn bold(s: &str) -> String {
    paint(s, "1")
}

pub fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

pub fn confirm(q: &str) -> Result<bool> {
    print!("{q} [y/N] ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes"))
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}
