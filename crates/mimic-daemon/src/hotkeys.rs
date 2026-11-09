//! Global shortcuts via low-level keyboard/mouse hooks on a dedicated thread.
//!
//! The hook callback does nothing but set lookups and a channel send, so it
//! never adds input latency. Modifier-only push-to-talk (Ctrl+Win) passes the
//! modifiers through to the system (so Ctrl+Win+Arrow etc. keep working) and
//! injects a neutral key so releasing Win never opens the Start menu. Keys
//! that belong to Mimic (Space/R/Esc while dictating, F-keys, mouse buttons)
//! are swallowed.
//!
//! It also tracks a "user input epoch": any key or click that isn't ours
//! bumps it, which tells continuation that the text field may have changed.
//!
//! Key-ups do get lost: the secure desktop (Win+L, Ctrl+Alt+Del) and windows of elevated
//! apps (many games, Task Manager) never pass theirs to the hook. A key the machine still
//! thinks is held turns ordinary typing into shortcuts (a stale Ctrl+Win makes "A" private
//! talk, Space hands-free, a lone Ctrl start dictation). So the held keys are checked against
//! the system's key state on every event and four times a second, keys Mimic swallowed (which
//! the system state can't vouch for) count as released when they stop auto-repeating, and the
//! hooks are reinstalled every 30 s in case Windows silently dropped them.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crossbeam_channel::Sender;
use mimic_core::config::Hotkeys as HotkeyCfg;
use mimic_core::hotkey::{Hotkey, Key, Mods};
use parking_lot::RwLock;

use crate::app::Ev;

#[derive(Debug, Clone, PartialEq)]
pub enum HotkeyEv {
    PttDown,
    PttUp,
    HandsFree,
    AddRule,
    Cancel,
    /// Another key was pressed while holding push-to-talk (a system shortcut).
    OtherKey,
    Recorded(Option<Hotkey>),
    /// An agent shortcut. `Talk` also reports its release (`down == false`) so it can be held.
    Agent(AgentKey, bool),
    /// Soundboard slot (0-based).
    Sound(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKey {
    /// Hold to talk privately to the agent (instructions, commands).
    Talk,
    /// Toggle autopilot (the agent answers on your behalf).
    Autopilot,
    /// Have the agent answer the conversation right now, in your voice.
    Reply,
    /// Stop the agent speaking / thinking.
    Stop,
}

fn agent_bindings(b: &HotkeyCfg) -> [(Hotkey, AgentKey); 4] {
    [(b.agent_talk, AgentKey::Talk), (b.agent_autopilot, AgentKey::Autopilot), (b.agent_reply, AgentKey::Reply), (b.agent_stop, AgentKey::Stop)]
}

/// A dictation is running (enables the cancel key).
pub static ACTIVE: AtomicBool = AtomicBool::new(false);
pub static PAUSED: AtomicBool = AtomicBool::new(false);
pub static RECORDING: AtomicBool = AtomicBool::new(false);
/// While the rule popup is open, typing in it must not count as user input.
pub static EPOCH_FROZEN: AtomicBool = AtomicBool::new(false);
pub static USER_EPOCH: AtomicU64 = AtomicU64::new(0);
/// Physically held modifiers: 1 ctrl, 2 shift, 4 alt, 8 meta.
pub static MODS_DOWN: AtomicU8 = AtomicU8::new(0);

static BINDINGS: RwLock<Option<HotkeyCfg>> = RwLock::new(None);

pub fn set_bindings(cfg: &HotkeyCfg) {
    *BINDINGS.write() = Some(cfg.clone());
}

pub fn epoch() -> u64 {
    USER_EPOCH.load(Ordering::SeqCst)
}

fn bump_epoch() {
    if !EPOCH_FROZEN.load(Ordering::Relaxed) {
        USER_EPOCH.fetch_add(1, Ordering::SeqCst);
    }
}

/// Modifiers that make typed text turn into shortcuts.
pub fn blocking_mods_down() -> bool {
    MODS_DOWN.load(Ordering::SeqCst) & (1 | 4 | 8) != 0
}

// ------------------------------------------------------------------ pure state machine

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input {
    Ctrl,
    Shift,
    Alt,
    Meta,
    Key(Key),
    /// A key Mimic has no name for (still counts for "other key" / input epoch).
    Other(u32),
}

impl Input {
    fn is_mod(self) -> bool {
        matches!(self, Input::Ctrl | Input::Shift | Input::Alt | Input::Meta)
    }
}

fn mods_of(down: &[Input]) -> Mods {
    let mut m = Mods::default();
    for i in down {
        match i {
            Input::Ctrl => m.ctrl = true,
            Input::Shift => m.shift = true,
            Input::Alt => m.alt = true,
            Input::Meta => m.meta = true,
            _ => {}
        }
    }
    m
}

#[derive(Debug, Default)]
pub struct Machine {
    down: Vec<Input>,
    swallowed: Vec<Input>,
    ptt_active: bool,
    ptt_key: Option<Input>,
    /// The held agent-talk key, so its release can be reported.
    talk_key: Option<Input>,
    rec_mods: Mods,
    rec_key: Option<Key>,
    rec_any: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub swallow: bool,
    pub events: Vec<HotkeyEv>,
    /// Inject the neutral mask key (prevents Start menu / menu bar on release).
    pub mask: bool,
    pub user_input: bool,
}

impl Machine {
    /// `phys` is the raw input (left/right modifiers already folded, except when
    /// a side-specific key like RightAlt is bound — callers pass it as `Key`).
    pub fn on(&mut self, b: &HotkeyCfg, input: Input, is_down: bool, active: bool, recording: bool) -> Outcome {
        let mut o = Outcome::default();
        if recording {
            return self.record(input, is_down);
        }
        if is_down {
            let repeat = self.down.contains(&input);
            if !repeat {
                self.down.push(input);
            }
            if self.swallowed.contains(&input) {
                o.swallow = true;
                return o;
            }
            let others: Vec<Input> = self.down.iter().copied().filter(|i| *i != input).collect();
            let mods_before = mods_of(&others);
            let mods_now = mods_of(&self.down);
            let key = match input {
                Input::Key(k) => Some(k),
                _ => None,
            };

            // Push-to-talk on a real key (F9, Mouse4, Ctrl+Shift+Space, RightAlt...).
            if let Some(pk) = b.push_to_talk.key {
                if key == Some(pk) && mods_before == b.push_to_talk.mods {
                    if !repeat && !self.ptt_active {
                        self.ptt_active = true;
                        self.ptt_key = Some(input);
                        o.events.push(HotkeyEv::PttDown);
                    }
                    self.swallowed.push(input);
                    o.swallow = true;
                    return o;
                }
            }
            if input.is_mod() {
                if b.push_to_talk.key.is_none() && !self.ptt_active && !repeat && mods_now == b.push_to_talk.mods {
                    self.ptt_active = true;
                    self.ptt_key = None;
                    o.events.push(HotkeyEv::PttDown);
                    o.mask = mods_now.meta || mods_now.alt;
                } else if self.ptt_active && !repeat {
                    o.events.push(HotkeyEv::OtherKey);
                }
                return o;
            }
            // A non-modifier key.
            if self.ptt_active {
                let hit = |h: &Hotkey| h.key.is_some() && h.key == key;
                let ev = if hit(&b.hands_free) {
                    Some(HotkeyEv::HandsFree)
                } else if hit(&b.add_rule) {
                    Some(HotkeyEv::AddRule)
                } else if hit(&b.cancel) {
                    Some(HotkeyEv::Cancel)
                } else {
                    agent_bindings(b).iter().find(|(h, _)| hit(h)).map(|(_, k)| HotkeyEv::Agent(*k, true))
                };
                match ev {
                    Some(e) => {
                        if !repeat {
                            if e == HotkeyEv::Agent(AgentKey::Talk, true) {
                                self.talk_key = Some(input);
                            }
                            o.events.push(e);
                        }
                        self.swallowed.push(input);
                        o.swallow = true;
                    }
                    None => {
                        if !repeat {
                            o.events.push(HotkeyEv::OtherKey);
                        }
                        o.user_input = true;
                    }
                }
                return o;
            }
            let exact = |h: &Hotkey| h.key.is_some() && h.key == key && mods_before == h.mods;
            let ev = if exact(&b.hands_free) {
                Some(HotkeyEv::HandsFree)
            } else if exact(&b.add_rule) {
                Some(HotkeyEv::AddRule)
            } else if active && exact(&b.cancel) {
                Some(HotkeyEv::Cancel)
            } else {
                agent_bindings(b)
                    .iter()
                    .find(|(h, _)| exact(h))
                    .map(|(_, k)| HotkeyEv::Agent(*k, true))
                    .or_else(|| b.sounds.iter().position(exact).map(HotkeyEv::Sound))
            };
            match ev {
                Some(e) => {
                    if !repeat {
                        if e == HotkeyEv::Agent(AgentKey::Talk, true) {
                            self.talk_key = Some(input);
                        }
                        o.events.push(e);
                    }
                    self.swallowed.push(input);
                    o.swallow = true;
                    o.mask = mods_before.meta || mods_before.alt;
                }
                None => o.user_input = true,
            }
            o
        } else {
            self.down.retain(|i| *i != input);
            let was_swallowed = self.swallowed.contains(&input);
            self.swallowed.retain(|i| *i != input);
            if self.talk_key == Some(input) {
                self.talk_key = None;
                o.events.push(HotkeyEv::Agent(AgentKey::Talk, false));
            }
            if self.ptt_active {
                if let Some(k) = self.ptt_key {
                    if k == input {
                        self.ptt_active = false;
                        self.ptt_key = None;
                        o.events.push(HotkeyEv::PttUp);
                    }
                } else if input.is_mod() {
                    let m = b.push_to_talk.mods;
                    let part = match input {
                        Input::Ctrl => m.ctrl,
                        Input::Shift => m.shift,
                        Input::Alt => m.alt,
                        Input::Meta => m.meta,
                        _ => false,
                    };
                    if part {
                        self.ptt_active = false;
                        o.events.push(HotkeyEv::PttUp);
                    }
                }
            }
            o.swallow = was_swallowed;
            o
        }
    }

    fn record(&mut self, input: Input, is_down: bool) -> Outcome {
        let mut o = Outcome { swallow: true, ..Default::default() };
        if is_down {
            if !self.down.contains(&input) {
                self.down.push(input);
            }
            self.rec_any = true;
            let m = mods_of(&self.down);
            // Remember the largest combination seen.
            if m.count() >= self.rec_mods.count() {
                self.rec_mods = m;
            }
            if let Input::Key(k) = input {
                self.rec_key = Some(k);
            }
            o.mask = true;
        } else {
            self.down.retain(|i| *i != input);
            if self.down.is_empty() && self.rec_any {
                let hk = Hotkey { mods: self.rec_mods, key: self.rec_key };
                let valid = hk.key.is_some() || hk.mods.count() >= 2;
                o.events.push(HotkeyEv::Recorded(valid.then_some(hk)));
                self.rec_mods = Mods::default();
                self.rec_key = None;
                self.rec_any = false;
            }
        }
        o
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keys the machine believes are held.
    pub fn held(&self) -> &[Input] {
        &self.down
    }

    pub fn is_swallowed(&self, input: Input) -> bool {
        self.swallowed.contains(&input)
    }

    /// Release keys that aren't held any more although their key-up never arrived.
    /// `still_down(i)`: `Some(false)` if the key is known to be up, `None` if that can't be told.
    pub fn reconcile(&mut self, b: &HotkeyCfg, still_down: impl Fn(Input) -> Option<bool>) -> (Vec<Input>, Outcome) {
        let stale: Vec<Input> = self.down.iter().copied().filter(|i| still_down(*i) == Some(false)).collect();
        let mut all = Outcome::default();
        for i in &stale {
            let o = self.on(b, *i, false, false, false);
            all.events.extend(o.events);
        }
        (stale, all)
    }
}

/// Every main key Mimic listens for (side-specific modifiers and mouse buttons must be told apart).
fn bound_keys(b: &HotkeyCfg) -> Vec<Option<Key>> {
    let mut v = vec![b.push_to_talk.key, b.hands_free.key, b.add_rule.key, b.agent_talk.key, b.agent_autopilot.key, b.agent_reply.key, b.agent_stop.key];
    v.extend(b.sounds.iter().map(|h| h.key));
    v
}

// ------------------------------------------------------------------ Windows hook

#[cfg(windows)]
pub use win::spawn;

#[cfg(windows)]
mod win {
    use super::*;
    use std::cell::RefCell;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    static TX: OnceLock<Sender<Ev>> = OnceLock::new();

    thread_local! {
        static MACHINE: RefCell<Machine> = RefCell::new(Machine::default());
        static WAS_RECORDING: RefCell<bool> = const { RefCell::new(false) };
        /// Per held input: its virtual key and when its last down/repeat event came.
        static SEEN: RefCell<std::collections::HashMap<Input, (u32, std::time::Instant)>> = RefCell::new(Default::default());
        static HOOKS: RefCell<(Option<HHOOK>, Option<HHOOK>)> = const { RefCell::new((None, None)) };
    }

    /// A swallowed key that hasn't auto-repeated for this long was released unseen. Keyboards
    /// repeat a held key every 30-400 ms after a delay of at most 1 s.
    const STALE_SWALLOWED: std::time::Duration = std::time::Duration::from_millis(2000);
    const RECONCILE_MS: u32 = 250;
    const REHOOK_EVERY: u32 = 120; // ticks: 30 s

    fn install_hooks() {
        unsafe {
            let hinst = GetModuleHandleW(None).ok();
            HOOKS.with(|h| {
                let mut h = h.borrow_mut();
                if let Some(k) = h.0.take() {
                    let _ = UnhookWindowsHookEx(k);
                }
                if let Some(m) = h.1.take() {
                    let _ = UnhookWindowsHookEx(m);
                }
                let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hinst.map(|h| h.into()), 0);
                let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst.map(|h| h.into()), 0);
                if kb.is_err() || ms.is_err() {
                    log::error!("installing input hooks failed: {:?} {:?}", kb.as_ref().err(), ms.as_ref().err());
                }
                *h = (kb.ok(), ms.ok());
            });
        }
    }

    /// Is this held input still physically down? `None` when the system can't tell: keys Mimic
    /// swallowed never reach the system's key state (they go by their auto-repeat instead),
    /// and mouse buttons don't repeat.
    fn still_down(input: Input, swallowed: bool) -> Option<bool> {
        let async_down = |vk: i32| unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 };
        let vk = SEEN.with(|s| s.borrow().get(&input).map(|(vk, t)| (*vk, *t)));
        if swallowed {
            if matches!(input, Input::Key(Key::Mouse4 | Key::Mouse5 | Key::MouseMiddle)) {
                return None;
            }
            return vk.map(|(_, t)| t.elapsed() < STALE_SWALLOWED);
        }
        Some(match input {
            Input::Ctrl => async_down(0x11),
            Input::Shift => async_down(0x10),
            Input::Alt => async_down(0x12),
            Input::Meta => async_down(0x5B) || async_down(0x5C),
            _ => match vk {
                Some((vk, _)) => async_down(vk as i32),
                None => return None,
            },
        })
    }

    /// Release held keys that aren't down any more (their key-up never reached the hook).
    fn reconcile(except: Option<Input>) {
        // Recording a shortcut swallows every key, so the system state can't vouch for any.
        if RECORDING.load(Ordering::SeqCst) {
            return;
        }
        let guard = BINDINGS.read();
        let Some(b) = guard.as_ref() else { return };
        let (stale, out) = MACHINE.with(|m| {
            let mut m = m.borrow_mut();
            if m.held().is_empty() {
                return (Vec::new(), Outcome::default());
            }
            let swallowed: Vec<Input> = m.held().iter().copied().filter(|i| m.is_swallowed(*i)).collect();
            m.reconcile(b, |i| if Some(i) == except { None } else { still_down(i, swallowed.contains(&i)) })
        });
        drop(guard);
        if stale.is_empty() {
            return;
        }
        log::info!("hotkeys: {stale:?} released without a key-up reaching Mimic (lock screen, elevated window); resynced");
        SEEN.with(|s| {
            let mut s = s.borrow_mut();
            for i in &stale {
                s.remove(i);
                update_mods(*i, false);
            }
        });
        if let Some(tx) = TX.get() {
            for e in out.events {
                let _ = tx.send(Ev::Hotkey(e));
            }
        }
    }

    pub fn spawn(tx: Sender<Ev>) {
        let _ = TX.set(tx);
        std::thread::Builder::new()
            .name("mimic-hotkeys".into())
            .spawn(|| unsafe {
                let _ = windows::Win32::System::Threading::SetThreadPriority(
                    windows::Win32::System::Threading::GetCurrentThread(),
                    windows::Win32::System::Threading::THREAD_PRIORITY_TIME_CRITICAL,
                );
                install_hooks();
                // A thread timer: resync held keys, and now and then reinstall the hooks.
                let _ = SetTimer(None, 0, RECONCILE_MS, None);
                let mut ticks = 0u32;
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_TIMER {
                        reconcile(None);
                        ticks += 1;
                        if ticks % REHOOK_EVERY == 0 {
                            install_hooks();
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            })
            .expect("spawn hotkey thread");
    }

    fn vk_input(vk: u32, bindings: &HotkeyCfg) -> Input {
        // Side-specific keys bound on their own keep their identity.
        let side = |k: Key| bound_keys(bindings).contains(&Some(k));
        match vk {
            0xA5 if side(Key::RightAlt) => Input::Key(Key::RightAlt),
            0xA3 if side(Key::RightCtrl) => Input::Key(Key::RightCtrl),
            0xA1 if side(Key::RightShift) => Input::Key(Key::RightShift),
            0xA2 | 0xA3 | 0x11 => Input::Ctrl,
            0xA0 | 0xA1 | 0x10 => Input::Shift,
            0xA4 | 0xA5 | 0x12 => Input::Alt,
            0x5B | 0x5C => Input::Meta,
            0x41..=0x5A | 0x30..=0x39 => Input::Key(Key::Char(char::from_u32(vk).unwrap())),
            0x70..=0x87 => Input::Key(Key::F((vk - 0x70 + 1) as u8)),
            0x20 => Input::Key(Key::Space),
            0x0D => Input::Key(Key::Enter),
            0x09 => Input::Key(Key::Tab),
            0x1B => Input::Key(Key::Escape),
            0x08 => Input::Key(Key::Backspace),
            0x2E => Input::Key(Key::Delete),
            0x2D => Input::Key(Key::Insert),
            0x24 => Input::Key(Key::Home),
            0x23 => Input::Key(Key::End),
            0x21 => Input::Key(Key::PageUp),
            0x22 => Input::Key(Key::PageDown),
            0x25 => Input::Key(Key::Left),
            0x26 => Input::Key(Key::Up),
            0x27 => Input::Key(Key::Right),
            0x28 => Input::Key(Key::Down),
            0x14 => Input::Key(Key::CapsLock),
            0x91 => Input::Key(Key::ScrollLock),
            0x13 => Input::Key(Key::Pause),
            0x2C => Input::Key(Key::PrintScreen),
            0xC0 => Input::Key(Key::Grave),
            0xBD => Input::Key(Key::Minus),
            0xBB => Input::Key(Key::Equal),
            0xDB => Input::Key(Key::BracketLeft),
            0xDD => Input::Key(Key::BracketRight),
            0xDC => Input::Key(Key::Backslash),
            0xBA => Input::Key(Key::Semicolon),
            0xDE => Input::Key(Key::Quote),
            0xBC => Input::Key(Key::Comma),
            0xBE => Input::Key(Key::Period),
            0xBF => Input::Key(Key::Slash),
            other => Input::Other(other),
        }
    }

    fn update_mods(input: Input, down: bool) {
        let bit = match input {
            Input::Ctrl => 1,
            Input::Shift => 2,
            Input::Alt => 4,
            Input::Meta => 8,
            _ => return,
        };
        if down {
            MODS_DOWN.fetch_or(bit, Ordering::SeqCst);
        } else {
            MODS_DOWN.fetch_and(!bit, Ordering::SeqCst);
        }
    }

    pub fn send_mask() {
        let ev = [
            INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), ..Default::default() } } },
            INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
        ];
        unsafe {
            SendInput(&ev, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn handle(input: Input, vk: u32, down: bool) -> bool {
        // First drop keys whose key-up was lost, so they can't turn this key into a shortcut.
        reconcile(Some(input));
        if down {
            // A swallowed key that stopped repeating long ago and is pressed again: its key-up
            // was lost; release it first so this press counts.
            let stale =
                SEEN.with(|s| s.borrow().get(&input).is_some_and(|(_, t)| t.elapsed() >= STALE_SWALLOWED)) && MACHINE.with(|m| m.borrow().is_swallowed(input));
            if stale {
                let _ = handle(input, vk, false);
            }
            SEEN.with(|s| s.borrow_mut().insert(input, (vk, std::time::Instant::now())));
        } else {
            SEEN.with(|s| s.borrow_mut().remove(&input));
        }
        update_mods(input, down);
        let recording = RECORDING.load(Ordering::SeqCst);
        // Leaving record mode: forget its key state.
        WAS_RECORDING.with(|w| {
            let mut w = w.borrow_mut();
            if *w && !recording {
                MACHINE.with(|m| m.borrow_mut().reset());
            }
            *w = recording;
        });
        if PAUSED.load(Ordering::Relaxed) && !recording {
            if down && !input.is_mod() {
                bump_epoch();
            }
            return false;
        }
        let guard = BINDINGS.read();
        let Some(b) = guard.as_ref() else { return false };
        let out = MACHINE.with(|m| m.borrow_mut().on(b, input, down, ACTIVE.load(Ordering::Relaxed), recording));
        drop(guard);
        if out.user_input {
            bump_epoch();
        }
        if out.mask {
            send_mask();
        }
        if let Some(tx) = TX.get() {
            for e in out.events {
                let _ = tx.send(Ev::Hotkey(e));
            }
        }
        out.swallow
    }

    unsafe extern "system" fn kb_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let injected = k.flags.0 & LLKHF_INJECTED.0 != 0;
            if !injected && k.vkCode != 0xE8 {
                let msg = wparam.0 as u32;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let input = {
                    let g = BINDINGS.read();
                    match g.as_ref() {
                        Some(b) => vk_input(k.vkCode, b),
                        None => Input::Other(k.vkCode),
                    }
                };
                if handle(input, k.vkCode, down) {
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let m = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let injected = m.flags & LLMHF_INJECTED != 0;
            if !injected {
                let msg = wparam.0 as u32;
                let xbutton = (m.mouseData >> 16) & 0xffff;
                let (input, down) = match msg {
                    WM_XBUTTONDOWN | WM_XBUTTONUP => (Some(if xbutton == 1 { Key::Mouse4 } else { Key::Mouse5 }), msg == WM_XBUTTONDOWN),
                    WM_MBUTTONDOWN | WM_MBUTTONUP => (Some(Key::MouseMiddle), msg == WM_MBUTTONDOWN),
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                        bump_epoch();
                        (None, true)
                    }
                    _ => (None, false),
                };
                if let Some(k) = input {
                    let bound = {
                        let g = BINDINGS.read();
                        g.as_ref().is_some_and(|b| bound_keys(b).contains(&Some(k)))
                    };
                    if bound || RECORDING.load(Ordering::SeqCst) {
                        let vk = match k {
                            Key::Mouse4 => 0x05,
                            Key::Mouse5 => 0x06,
                            _ => 0x04,
                        };
                        if handle(Input::Key(k), vk, down) {
                            return LRESULT(1);
                        }
                    } else if down {
                        bump_epoch();
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(not(windows))]
pub fn spawn(_tx: Sender<Ev>) {
    log::warn!("global shortcuts are not implemented on this platform yet");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HotkeyCfg {
        HotkeyCfg::default()
    }

    fn press(m: &mut Machine, b: &HotkeyCfg, i: Input, down: bool) -> Outcome {
        m.on(b, i, down, false, false)
    }

    #[test]
    fn ctrl_win_hold_and_release() {
        let b = cfg();
        let mut m = Machine::default();
        assert_eq!(press(&mut m, &b, Input::Ctrl, true).events, vec![]);
        let o = press(&mut m, &b, Input::Meta, true);
        assert_eq!(o.events, vec![HotkeyEv::PttDown]);
        assert!(o.mask && !o.swallow);
        // autorepeat does nothing
        assert_eq!(press(&mut m, &b, Input::Meta, true).events, vec![]);
        let o = press(&mut m, &b, Input::Meta, false);
        assert_eq!(o.events, vec![HotkeyEv::PttUp]);
        assert_eq!(press(&mut m, &b, Input::Ctrl, false).events, vec![]);
    }

    #[test]
    fn space_locks_and_is_swallowed() {
        let b = cfg();
        let mut m = Machine::default();
        press(&mut m, &b, Input::Meta, true);
        press(&mut m, &b, Input::Ctrl, true);
        let o = press(&mut m, &b, Input::Key(Key::Space), true);
        assert_eq!(o.events, vec![HotkeyEv::HandsFree]);
        assert!(o.swallow);
        assert!(press(&mut m, &b, Input::Key(Key::Space), false).swallow);
        let o = press(&mut m, &b, Input::Key(Key::Char('R')), true);
        assert_eq!(o.events, vec![HotkeyEv::AddRule]);
    }

    #[test]
    fn agent_talk_is_held_and_released() {
        let b = cfg();
        let mut m = Machine::default();
        press(&mut m, &b, Input::Ctrl, true);
        press(&mut m, &b, Input::Meta, true); // push-to-talk starts first
        let o = press(&mut m, &b, Input::Key(Key::Char('A')), true);
        assert_eq!(o.events, vec![HotkeyEv::Agent(AgentKey::Talk, true)]);
        assert!(o.swallow);
        // Releasing the modifiers first ends push-to-talk but not the agent talk.
        assert_eq!(press(&mut m, &b, Input::Meta, false).events, vec![HotkeyEv::PttUp]);
        let o = press(&mut m, &b, Input::Key(Key::Char('A')), false);
        assert_eq!(o.events, vec![HotkeyEv::Agent(AgentKey::Talk, false)]);
        assert!(o.swallow);
        // Other agent keys fire once on press.
        press(&mut m, &b, Input::Meta, true);
        let o = press(&mut m, &b, Input::Key(Key::Char('P')), true);
        assert_eq!(o.events, vec![HotkeyEv::Agent(AgentKey::Autopilot, true)]);
        assert_eq!(press(&mut m, &b, Input::Key(Key::Char('P')), false).events, vec![]);
    }

    #[test]
    fn soundboard_slots_on_alt_digits() {
        let b = cfg();
        let mut m = Machine::default();
        press(&mut m, &b, Input::Alt, true);
        let o = press(&mut m, &b, Input::Key(Key::Char('3')), true);
        assert_eq!(o.events, vec![HotkeyEv::Sound(2)]);
        assert!(o.swallow && o.mask);
        assert!(press(&mut m, &b, Input::Key(Key::Char('3')), false).swallow);
        press(&mut m, &b, Input::Alt, false);
        // A plain digit is just typing.
        let o = press(&mut m, &b, Input::Key(Key::Char('3')), true);
        assert!(o.events.is_empty() && !o.swallow);
    }

    #[test]
    fn system_shortcut_reports_other_key() {
        let b = cfg();
        let mut m = Machine::default();
        press(&mut m, &b, Input::Ctrl, true);
        press(&mut m, &b, Input::Meta, true);
        let o = press(&mut m, &b, Input::Key(Key::Left), true);
        assert_eq!(o.events, vec![HotkeyEv::OtherKey]);
        assert!(!o.swallow);
    }

    #[test]
    fn plain_typing_bumps_input_and_escape_only_when_active() {
        let b = cfg();
        let mut m = Machine::default();
        let o = press(&mut m, &b, Input::Key(Key::Char('A')), true);
        assert!(o.user_input && !o.swallow && o.events.is_empty());
        press(&mut m, &b, Input::Key(Key::Char('A')), false);
        let o = m.on(&b, Input::Key(Key::Escape), true, false, false);
        assert!(!o.swallow);
        m.on(&b, Input::Key(Key::Escape), false, false, false);
        let o = m.on(&b, Input::Key(Key::Escape), true, true, false);
        assert!(o.swallow);
        assert_eq!(o.events, vec![HotkeyEv::Cancel]);
    }

    #[test]
    fn key_based_ptt() {
        let mut b = cfg();
        b.push_to_talk = Hotkey::parse("F9").unwrap();
        let mut m = Machine::default();
        let o = press(&mut m, &b, Input::Key(Key::F(9)), true);
        assert_eq!(o.events, vec![HotkeyEv::PttDown]);
        assert!(o.swallow);
        let o = press(&mut m, &b, Input::Key(Key::F(9)), false);
        assert_eq!(o.events, vec![HotkeyEv::PttUp]);
        assert!(o.swallow);
    }

    #[test]
    fn standalone_add_rule_combo() {
        let b = cfg();
        let mut m = Machine::default();
        // Ctrl+Win+R pressed quickly also goes through PTT first; that's fine:
        // the controller treats AddRule during a fresh hold as "open popup".
        press(&mut m, &b, Input::Ctrl, true);
        press(&mut m, &b, Input::Meta, true);
        let o = press(&mut m, &b, Input::Key(Key::Char('R')), true);
        assert_eq!(o.events, vec![HotkeyEv::AddRule]);
    }

    /// A lost key-up (Win+L, an elevated window) must not leave Ctrl+Win "held": once the
    /// system says they're up, they're released, a running push-to-talk ends, and plain keys
    /// are plain again.
    #[test]
    fn lost_key_ups_are_reconciled() {
        let b = cfg();
        let mut m = Machine::default();
        press(&mut m, &b, Input::Ctrl, true);
        assert_eq!(press(&mut m, &b, Input::Meta, true).events, vec![HotkeyEv::PttDown]);
        // ...the key-ups never arrive. Without reconciling, "A" is private talk:
        let mut stuck = Machine::default();
        press(&mut stuck, &b, Input::Ctrl, true);
        press(&mut stuck, &b, Input::Meta, true);
        assert_eq!(press(&mut stuck, &b, Input::Key(Key::Char('A')), true).events, vec![HotkeyEv::Agent(AgentKey::Talk, true)]);
        // With it:
        let (stale, o) = m.reconcile(&b, |_| Some(false));
        assert_eq!(stale, vec![Input::Ctrl, Input::Meta]);
        assert_eq!(o.events, vec![HotkeyEv::PttUp]);
        assert!(m.held().is_empty());
        let o = press(&mut m, &b, Input::Key(Key::Char('A')), true);
        assert!(o.events.is_empty() && !o.swallow);
        // Keys that can't be checked stay.
        press(&mut m, &b, Input::Alt, true);
        let (stale, _) = m.reconcile(&b, |_| None);
        assert!(stale.is_empty() && m.held().contains(&Input::Alt));
    }

    #[test]
    fn recording_captures_combo() {
        let b = cfg();
        let mut m = Machine::default();
        let rec = |m: &mut Machine, i, d| m.on(&b, i, d, false, true);
        assert!(rec(&mut m, Input::Ctrl, true).swallow);
        rec(&mut m, Input::Shift, true);
        rec(&mut m, Input::Key(Key::Char('D')), true);
        rec(&mut m, Input::Key(Key::Char('D')), false);
        rec(&mut m, Input::Shift, false);
        let o = rec(&mut m, Input::Ctrl, false);
        assert_eq!(o.events, vec![HotkeyEv::Recorded(Some(Hotkey::parse("Ctrl+Shift+D").unwrap()))]);
        // a lone modifier is not valid
        rec(&mut m, Input::Ctrl, true);
        let o = rec(&mut m, Input::Ctrl, false);
        assert_eq!(o.events, vec![HotkeyEv::Recorded(None)]);
    }
}
