//! The top-level clap definitions: `mimic`'s global flags and its list of commands.
//! Doc comments here are the `--help` text. Each command area's subcommands live with it in `commands/`.

use clap::{Parser, Subcommand};

use crate::commands::agent::AgentCmd;
use crate::commands::config::ConfigCmd;
use crate::commands::dictate::DictateCmd;
use crate::commands::model::ModelCmd;
use crate::commands::rules::RulesCmd;
use crate::commands::setup::SetupCmd;

const ABOUT: &str = "Mimic — fast, private, local dictation.

Hold your shortcut (default Ctrl+Win), speak, release: text appears where your cursor is.
This CLI controls the Mimic background app: status, settings, your dictionary, models.";

const AFTER: &str = "Examples:
  mimic status                          what Mimic is doing right now
  mimic config set audio.preroll_ms 300 keep 0.3 s of audio from before you press the shortcut
  mimic rules add Omarchy --heard \"on march 9\" --heard \"oh marchy\"
  mimic model use parakeet-v3 && mimic model install
  mimic transcribe meeting.wav

Settings live in the config file (`mimic config path`); every key is listed by `mimic config list`.
Most commands need the background app; `mimic start` launches it (it starts with Windows by default).";

#[derive(Parser)]
#[command(name = "mimic", version, about = ABOUT, after_help = AFTER, propagate_version = true)]
pub struct Cli {
    /// Machine-readable JSON output (for scripts).
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Show what Mimic is doing: model, microphone, shortcuts, stats.
    #[command(visible_alias = "st")]
    Status {
        /// Keep watching and print every change (Ctrl+C to stop).
        #[arg(short, long)]
        watch: bool,
    },
    /// Start the Mimic background app.
    Start,
    /// Quit the Mimic background app.
    Stop,
    /// Restart the Mimic background app.
    Restart,
    /// Open the Mimic window (optionally on a page: home, dictionary, shortcuts, model, audio, text, output, advanced).
    Open { page: Option<String> },
    /// Start/stop dictation without the shortcut (types into the focused app).
    #[command(subcommand)]
    Dictate(DictateCmd),
    /// Read and change settings.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Your dictionary: words and "heard X → write Y" rules.
    #[command(subcommand, visible_alias = "dict")]
    Rules(RulesCmd),
    /// Speech models: list, switch, install, load into memory, unload.
    #[command(subcommand)]
    Model(ModelCmd),
    /// The agent: follows your conversations, talks for you in your voice, takes notes.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Soundboard: play slot 1-9 (again to stop), `stop`, or `set <slot> <file>`.
    Sound {
        /// A slot number, `stop`, or `set`.
        what: String,
        /// For `set`: the slot and the audio file.
        args: Vec<String>,
    },
    /// List microphones.
    Devices,
    /// Transcribe a WAV file with the current model and your rules.
    Transcribe {
        /// Path to a .wav file (any sample rate, mono or stereo).
        file: std::path::PathBuf,
        /// Print the raw model output instead of the processed text.
        #[arg(long)]
        raw: bool,
    },
    /// Recent dictations (kept in memory only; never written to disk).
    History {
        /// Forget the history.
        #[arg(long)]
        clear: bool,
        /// How many to show.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Show the log file (or follow it).
    Logs {
        /// Keep printing new lines.
        #[arg(short, long)]
        follow: bool,
        /// Only print the path.
        #[arg(long)]
        path: bool,
        /// Number of lines.
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// Start Mimic when you sign in (on/off).
    Autostart { state: OnOff },
    /// Check that everything is set up correctly.
    Doctor,
    /// Check for a new version and install it (Mimic restarts by itself).
    Update {
        /// Only check, don't offer to install.
        #[arg(long)]
        check: bool,
        /// Install the latest version without asking.
        #[arg(long, conflicts_with_all = ["check", "skip"])]
        install: bool,
        /// Stop reminding me about the latest version.
        #[arg(long, conflicts_with = "check")]
        skip: bool,
    },
    /// Remove Mimic from this computer (runs the uninstaller; it asks whether to delete your settings and models).
    Uninstall,
    /// Installer helpers.
    #[command(subcommand, hide = true)]
    Setup(SetupCmd),
    /// Open the quick "add a word" popup (same as the shortcut).
    Popup,
    /// Call any daemon method directly (advanced; see the protocol docs):
    /// `mimic ipc popup.context` or `mimic ipc config.get '{"path":"audio"}'`.
    Ipc {
        method: String,
        /// JSON params.
        params: Option<String>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}
