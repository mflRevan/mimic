//! `mimic` — command line control for Mimic.
//!
//! `cli` holds the clap definitions, `commands/` one module per command area,
//! `client` the connection to the background app and `ui` the terminal helpers.

mod cli;
mod client;
mod commands;
mod ui;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};
use commands::{agent, app, config, devices, dictate, doctor, history, install, ipc, logs, model, rules, setup, sound, status, transcribe, update};
use ui::paint;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("{} {e:#}", paint("error:", "31;1"));
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let json_out = cli.json;
    match cli.cmd {
        Cmd::Status { watch } => status::run(json_out, watch),
        Cmd::Start => app::start(),
        Cmd::Stop => app::stop(),
        Cmd::Restart => app::restart(),
        Cmd::Open { page } => app::open(page),
        Cmd::Popup => app::popup(),
        Cmd::Dictate(d) => dictate::run(d),
        Cmd::Config(c) => config::run(c, json_out),
        Cmd::Agent(a) => agent::run(a, json_out),
        Cmd::Sound { what, args } => sound::run(what, args, json_out),
        Cmd::Rules(r) => rules::run(r, json_out),
        Cmd::Model(m) => model::run(m, json_out),
        Cmd::Devices => devices::run(json_out),
        Cmd::Transcribe { file, raw } => transcribe::run(file, raw, json_out),
        Cmd::History { clear, limit } => history::run(clear, limit, json_out),
        Cmd::Logs { follow, path, lines } => logs::run(follow, path, lines),
        Cmd::Autostart { state } => install::autostart(state),
        Cmd::Doctor => doctor::run(),
        Cmd::Update { check, install, skip } => update::run(check, install, skip, json_out),
        Cmd::Uninstall => install::uninstall(),
        Cmd::Setup(s) => setup::run(s),
        Cmd::Ipc { method, params } => ipc::run(method, params),
    }
}
