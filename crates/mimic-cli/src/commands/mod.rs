//! One module per command area. Each exposes the function(s) `main` dispatches to, plus its subcommand enums.

pub mod agent;
pub mod app;
pub mod config;
pub mod devices;
pub mod dictate;
pub mod doctor;
pub mod history;
pub mod install;
pub mod ipc;
pub mod logs;
pub mod model;
pub mod rules;
pub mod setup;
pub mod sound;
pub mod status;
pub mod transcribe;
pub mod update;
