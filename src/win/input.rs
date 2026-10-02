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

/// How much of a keystroke sequence Windows accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    All,
    /// Some events were injected and some weren't: the target may have seen half a chord.
    Partial,
    /// None were (UIPI, secure desktop, another thread holding the input queue).
    Blocked,
}

pub fn classify(sent: usize, expected: usize) -> Sent {
    match sent {
        0 => Sent::Blocked,
        n if n == expected => Sent::All,
        _ => Sent::Partial,
    }
}

fn send(inputs: &[INPUT]) -> Sent {
    classify(send_count(inputs), inputs.len())
}

fn send_count(inputs: &[INPUT]) -> usize {
    // SAFETY: valid INPUT slice.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    sent as usize
}

fn unreleased(events: &[(u16, bool)], accepted: usize) -> Vec<u16> {
    let mut down = Vec::new();
    for &(vk, up) in events.iter().take(accepted) {
        if up {
            down.retain(|&k| k != vk);
        } else if !down.contains(&vk) {
            down.push(vk);
        }
    }
    down
}

/// Sends Ctrl+V (Ctrl+Shift+V for terminals). Modifiers the user is still holding are
/// released first so they don't turn it into e.g. Ctrl+Win+V, then pressed again.
/// Reports whether Windows took every event, only some, or none.
pub fn paste(terminal: bool) -> Sent {
    // SAFETY: plain query.
    let held: Vec<u16> = MODIFIERS
        .into_iter()
        .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
        .collect();

    let mut events: Vec<(u16, bool)> = held.iter().map(|&vk| (vk, true)).collect();
    events.push((LCTRL, false));
    if terminal {
        events.push((LSHIFT, false));
    }
    events.push((V, false));
    events.push((V, true));
    if terminal {
        events.push((LSHIFT, true));
    }
    events.push((LCTRL, true));
    let inputs: Vec<INPUT> = events.iter().map(|&(vk, up)| key(vk, up)).collect();
    let accepted = send_count(&inputs);
    let ok = classify(accepted, inputs.len());
    if ok == Sent::Partial {
        let cleanup: Vec<INPUT> = unreleased(&events, accepted)
            .into_iter()
            .rev()
            .map(|vk| key(vk, true))
            .collect();
        if !cleanup.is_empty() {
            send(&cleanup);
        }
    }

    // Only re-press what the user is still holding; a key let go meanwhile would otherwise
    // stay stuck down.
    let restore: Vec<INPUT> = held
        .iter()
        .filter(|&&vk| super::hook::physically_down(vk))
        .map(|&vk| key(vk, false))
        .collect();
    if !restore.is_empty() {
        send(&restore);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_chords_release_only_accepted_unbalanced_downs() {
        let events = [
            (LCTRL, false),
            (LSHIFT, false),
            (V, false),
            (V, true),
            (LSHIFT, true),
            (LCTRL, true),
        ];
        assert!(unreleased(&events, 0).is_empty());
        assert_eq!(unreleased(&events, 1), vec![LCTRL]);
        assert_eq!(unreleased(&events, 3), vec![LCTRL, LSHIFT, V]);
        assert_eq!(unreleased(&events, 4), vec![LCTRL, LSHIFT]);
        assert!(unreleased(&events, 6).is_empty());
    }

    #[test]
    fn classifies_the_sendinput_count() {
        assert_eq!(classify(4, 4), Sent::All);
        assert_eq!(classify(0, 4), Sent::Blocked);
        assert_eq!(classify(2, 4), Sent::Partial);
    }
}
