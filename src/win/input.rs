//! Synthesised paste keystrokes.

use super::hook::INJECT_TAG;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
    VIRTUAL_KEY,
};

const LCTRL: u16 = 0xA2;
const LSHIFT: u16 = 0xA0;
const V: u16 = 0x56;

/// Modifiers checked per side, since GetAsyncKeyState reports physical state per side.
const MODIFIERS: [u16; 8] = [0xA2, 0xA3, 0xA0, 0xA1, 0xA4, 0xA5, 0x5B, 0x5C];

fn key(vk: u16, up: bool) -> INPUT {
    let mut flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    // Right Ctrl, right Alt and both Win keys are extended keys.
    if matches!(vk, 0xA3 | 0xA5 | 0x5B | 0x5C) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                // SAFETY: plain table lookup.
                wScan: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as u16,
                dwFlags: flags,
                dwExtraInfo: INJECT_TAG,
                ..Default::default()
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    // SAFETY: valid INPUT slice.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    sent as usize == inputs.len()
}

/// Sends Ctrl+V (Ctrl+Shift+V for terminals). Modifiers the user is still holding are
/// released first so they don't turn it into e.g. Ctrl+Win+V, then pressed again.
/// Returns false if Windows blocked the input (UIPI, secure desktop).
pub fn paste(terminal: bool) -> bool {
    // SAFETY: plain query.
    let held: Vec<u16> = MODIFIERS
        .into_iter()
        .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
        .collect();

    let mut inputs: Vec<INPUT> = held.iter().map(|&vk| key(vk, true)).collect();
    inputs.push(key(LCTRL, false));
    if terminal {
        inputs.push(key(LSHIFT, false));
    }
    inputs.push(key(V, false));
    inputs.push(key(V, true));
    if terminal {
        inputs.push(key(LSHIFT, true));
    }
    inputs.push(key(LCTRL, true));
    let ok = send(&inputs);

    let restore: Vec<INPUT> = held.iter().map(|&vk| key(vk, false)).collect();
    if !restore.is_empty() {
        send(&restore);
    }
    ok
}
