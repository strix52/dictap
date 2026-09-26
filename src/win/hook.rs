//! WH_KEYBOARD_LL hook on its own thread, which does nothing else: Windows silently removes
//! a low-level hook whose thread is slow to answer, so it must never share a message loop
//! with windows that paint. The callback runs the pure chord machine and, on a toggle,
//! posts one message to the IPC window. It never allocates, locks, logs or blocks.

use crate::hotkey::{self, Chord, ChordMachine};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, MSG, PostMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

use super::ipc::WM_APP_TOGGLE;

/// The active chord, packed. Written by the core on settings change, read by the callback.
static CHORD: AtomicU64 = AtomicU64::new(0);

/// Marks input we inject so it's recognisable in other tools; our hook ignores all injected input.
pub const INJECT_TAG: usize = 0x6765_6D64; // "gemd"

/// An unassigned VK: injecting it while Win is held stops the Start menu opening on release.
const VK_MASK: u16 = 0xE8;

/// Thread message: reinstall the hook (after resume or unlock).
const WM_APP_REINSTALL: u32 = WM_APP + 1;

/// The hook thread's id, for posting to it.
static THREAD: AtomicU32 = AtomicU32::new(0);

/// Sided modifier VKs, one bit each in `PHYSICAL`.
const SIDED: [u16; 8] = [0xA2, 0xA3, 0xA0, 0xA1, 0xA4, 0xA5, 0x5B, 0x5C];

/// Modifiers the user is physically holding, as seen by the hook (injected input excluded,
/// unlike GetAsyncKeyState).
static PHYSICAL: AtomicU8 = AtomicU8::new(0);

/// Whether the user is physically holding this sided modifier.
pub fn physically_down(vk: u16) -> bool {
    SIDED
        .iter()
        .position(|&k| k == vk)
        .is_some_and(|i| PHYSICAL.load(Ordering::Relaxed) & (1 << i) != 0)
}

thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static TARGET: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static MACHINE: RefCell<ChordMachine> = RefCell::new(ChordMachine::default());
}

pub fn set_chord(chord: Chord) {
    CHORD.store(chord.pack(), Ordering::Relaxed);
}

/// Starts the hook thread; toggles are posted to `target`. Returns once the hook is in.
pub fn start(target: HWND) -> windows::core::Result<()> {
    let target = target.0 as isize;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("hook".into())
        .spawn(move || {
            let target = HWND(target as *mut _);
            // SAFETY: plain query.
            THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
            let installed = install(target);
            let ok = installed.is_ok();
            let _ = tx.send(installed);
            if !ok {
                return;
            }
            let mut msg = MSG::default();
            // SAFETY: standard message loop on the thread that owns the hook.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                if msg.hwnd.is_invalid() && msg.message == WM_APP_REINSTALL {
                    if let Err(e) = install(target) {
                        log::error!("keyboard hook reinstall failed: {e}");
                    }
                    continue;
                }
                // SAFETY: dispatching a message this thread received.
                unsafe { DispatchMessageW(&msg) };
            }
            uninstall();
        })
        .expect("spawn hook thread");
    rx.recv()
        .unwrap_or_else(|_| Err(windows::core::Error::empty()))
}

fn post_thread(msg: u32) {
    let tid = THREAD.load(Ordering::Acquire);
    if tid != 0 {
        // SAFETY: posting a plain thread message to our own hook thread.
        let _ = unsafe { PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0)) };
    }
}

/// Reinstalls the hook (from any thread).
pub fn reinstall() {
    post_thread(WM_APP_REINSTALL);
}

/// Ends the hook thread (from any thread).
pub fn stop() {
    post_thread(WM_QUIT);
}

/// Installs (or reinstalls) the hook on the calling thread, which must pump messages.
fn install(target: HWND) -> windows::core::Result<()> {
    uninstall();
    TARGET.with(|t| t.set(target));
    MACHINE.with(|m| m.borrow_mut().reset());
    // SAFETY: `callback` matches HOOKPROC; a null module handle is allowed for WH_KEYBOARD_LL.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), None, 0)? };
    HOOK.with(|h| h.set(Some(hook)));
    Ok(())
}

fn uninstall() {
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
            if let Some(i) = SIDED.iter().position(|&k| k == vk) {
                let bit = 1u8 << i;
                if down {
                    PHYSICAL.fetch_or(bit, Ordering::Relaxed);
                } else {
                    PHYSICAL.fetch_and(!bit, Ordering::Relaxed);
                }
            }
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
