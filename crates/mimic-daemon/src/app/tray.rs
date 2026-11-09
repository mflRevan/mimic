//! Tray menu commands.

use std::sync::atomic::Ordering;

use crate::engine::EngineCmd;
use crate::hotkeys::{self, HotkeyEv};
use crate::platform;
use crate::tray::TrayCmd;

use super::App;

impl App {
    // ------------------------------------------------------------- tray

    pub(super) fn on_tray(&mut self, t: TrayCmd) {
        match t {
            TrayCmd::Open => platform::open_ui(&[]),
            TrayCmd::Dictionary => platform::open_ui(&["--page", "dictionary"]),
            TrayCmd::AddWord => {
                self.popup_return = platform::foreground_window();
                hotkeys::EPOCH_FROZEN.store(true, Ordering::SeqCst);
                platform::open_ui(&["--popup"]);
            }
            TrayCmd::ToggleHandsFree => self.on_hotkey(HotkeyEv::HandsFree),
            TrayCmd::LoadModel => {
                let _ = self.engine.send(EngineCmd::Load { urgent: true });
            }
            TrayCmd::UnloadModel => {
                let _ = self.engine.send(EngineCmd::Unload);
            }
            TrayCmd::PauseHotkeys(p) => {
                hotkeys::PAUSED.store(p, Ordering::Relaxed);
                self.publish();
            }
            TrayCmd::Update => platform::open_ui(&["--page", "about"]),
            TrayCmd::CheckUpdates => {
                self.tray_check = true;
                self.updater.check(&self.tx, None);
                self.publish();
            }
            TrayCmd::Quit => self.quit = true,
        }
    }
}
