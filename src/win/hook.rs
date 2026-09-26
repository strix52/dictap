//! WH_KEYBOARD_LL hook. The callback runs the pure chord machine and, on a toggle, posts
//! one message to the IPC window. It never allocates, locks, logs or blocks: Windows
//! silently removes slow low-level hooks.

use crate::hotkey::{self, Chord, ChordMachine};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, PostMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::ipc::WM_APP_TOGGLE;

/// The active chord, packed. Written by the core on settings change, read by the callback.
static CHORD: AtomicU64 = AtomicU64::new(0);

/// Marks input we inject so it's recognisable in other tools; our hook ignores all injected input.
pub const INJECT_TAG: usize = 0x6765_6D64; // "gemd"

/// An unassigned VK: injecting it while Win is held stops the Start menu opening on release.
const VK_MASK: u16 = 0xE8;

thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static TARGET: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static MACHINE: RefCell<ChordMachine> = RefCell::new(ChordMachine::default());
}

pub fn set_chord(chord: Chord) {
    CHORD.store(chord.pack(), Ordering::Relaxed);
}

/// Installs (or reinstalls) the hook on the calling thread, which must pump messages.
/// Toggles are posted to `target`.
pub fn install(target: HWND) -> windows::core::Result<()> {
    uninstall();
    TARGET.with(|t| t.set(target));
    MACHINE.with(|m| m.borrow_mut().reset());
    // SAFETY: `callback` matches HOOKPROC; a null module handle is allowed for WH_KEYBOARD_LL.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), None, 0)? };
    HOOK.with(|h| h.set(Some(hook)));
    Ok(())
}

pub fn uninstall() {
    if let Some(hook) = HOOK.with(|h| h.take()) {
        // SAFETY: `hook` came from SetWindowsHookExW on this thread.
        let _ = unsafe { UnhookWindowsHookEx(hook) };
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: plain query.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                dwExtraInfo: INJECT_TAG,
                ..Default::default()
            },
        },
    }
}

unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if (info.flags & LLKHF_INJECTED).0 == 0 && info.vkCode <= 0xFF {
            let vk = info.vkCode as u16;
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let chord = Chord::unpack(CHORD.load(Ordering::Relaxed));
            let mods = hotkey::mods_after(vk, down, key_down);
            let out = MACHINE.with(|m| m.borrow_mut().feed(chord, vk, down, mods));
            if out.mask_win {
                let inputs = [key_input(VK_MASK, false), key_input(VK_MASK, true)];
                // SAFETY: valid INPUT array.
                unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
            }
            if out.toggle {
                let target = TARGET.with(Cell::get);
                // SAFETY: posting to our own window; failure only loses one toggle.
                let _ = unsafe { PostMessageW(Some(target), WM_APP_TOGGLE, WPARAM(0), LPARAM(0)) };
            }
            if out.swallow {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding the unmodified hook arguments.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
